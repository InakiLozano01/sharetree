Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

if (-not $IsWindows -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
    throw 'The ReFS fixture requires a GitHub-hosted Windows runner.'
}
if (-not $env:RUNNER_TEMP -or -not (Test-Path -LiteralPath $env:RUNNER_TEMP -PathType Container)) {
    throw 'RUNNER_TEMP must be an existing directory.'
}

$fixturePath = Join-Path $env:RUNNER_TEMP "sharetree-refs-$([guid]::NewGuid().ToString('N'))"
$imagePath = Join-Path $fixturePath 'scratch.vhdx'
$diskpartScript = Join-Path $fixturePath 'create-vhd.txt'
$mountAttempted = $false
New-Item -ItemType Directory -Path $fixturePath | Out-Null

try {
    Set-Content -LiteralPath $diskpartScript -Encoding Ascii -Value ('create vdisk file="{0}" maximum=16384 type=expandable' -f $imagePath)
    & diskpart.exe /s $diskpartScript
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $imagePath -PathType Leaf)) {
        throw "DiskPart could not create the ReFS fixture at $imagePath (exit $LASTEXITCODE)."
    }

    $existingDiskNumbers = @(Get-Disk | Select-Object -ExpandProperty Number)
    $mountAttempted = $true
    Mount-DiskImage -ImagePath $imagePath -Access ReadWrite -NoDriveLetter | Out-Null
    $images = @(Get-DiskImage -ImagePath $imagePath)
    if ($images.Count -ne 1 -or $images[0].ImagePath -ne $imagePath -or $images[0].Attached -ne $true) {
        throw "Could not confirm the fixture image is attached at $imagePath."
    }
    $image = $images[0]
    $disks = @($image | Get-Disk)
    if ($disks.Count -ne 1) {
        throw "Expected exactly one disk for $imagePath, found $($disks.Count)."
    }
    $disk = $disks[0]
    if ($null -eq $disk.Number -or $image.DevicePath -ne "\\.\PhysicalDrive$($disk.Number)" -or
        $existingDiskNumbers -contains $disk.Number -or $disk.PartitionStyle -ne 'RAW' -or
        $disk.NumberOfPartitions -ne 0 -or $disk.IsBoot -ne $false -or $disk.IsSystem -ne $false -or
        $disk.IsReadOnly -ne $false -or $disk.IsOffline -ne $false) {
        throw "Expected a fresh RAW disk owned by $imagePath, found disk $($disk.Number) at $($image.DevicePath)."
    }
    Write-Host "Confirmed fresh RAW disk $($disk.Number) for $imagePath."
    $disk = Initialize-Disk -InputObject $disk -PartitionStyle GPT -PassThru -Confirm:$false
    $partition = New-Partition -InputObject $disk -UseMaximumSize -AssignDriveLetter
    Format-Volume -Partition $partition -FileSystem ReFS -AllocationUnitSize 4096 -Confirm:$false -Force | Out-Null

    $volumes = @(Get-Volume -Partition $partition)
    if ($volumes.Count -ne 1 -or $volumes[0].FileSystem -ne 'ReFS' -or
        $volumes[0].AllocationUnitSize -ne 4096 -or $volumes[0].DriveLetter -notmatch '^[A-Z]$') {
        throw 'Expected a mounted 4 KiB ReFS volume on the fixture partition.'
    }
    $testPath = "$($volumes[0].DriveLetter):\sharetree-tests"
    $env:SHARETREE_TEST_REFS_DIR = (New-Item -ItemType Directory -Path $testPath).FullName
    Write-Host "Confirmed writable Windows Server ReFS with 4 KiB allocation units at $env:SHARETREE_TEST_REFS_DIR."

    $testName = 'refs_worktree_uses_verified_clones_and_keeps_edits_isolated'
    $testOutput = @(& cargo test --locked --color never --test windows $testName -- --ignored --exact --format pretty --color never)
    $testExitCode = $LASTEXITCODE
    $testOutput | Write-Host
    if ($testExitCode -ne 0) {
        throw "ReFS cargo test exited with code $testExitCode."
    }
    $passingTests = @($testOutput -ceq "test $testName ... ok")
    if ($passingTests.Count -ne 1) {
        throw "Expected exactly one passing ReFS test ($testName)."
    }
} catch {
    Write-Host "ReFS fixture or test failed: $_"
    throw
} finally {
    if ($mountAttempted) {
        try {
            Dismount-DiskImage -ImagePath $imagePath | Out-Null
        } catch {
            Write-Host "Dismount-DiskImage reported: $_"
        }
        $images = @(Get-DiskImage -ImagePath $imagePath)
        if ($images.Count -ne 1 -or $images[0].ImagePath -ne $imagePath -or $images[0].Attached -ne $false) {
            throw "ReFS cleanup could not confirm detachment of $imagePath."
        }
    }
    Remove-Item -LiteralPath $fixturePath -Recurse -Force
    if (Test-Path -LiteralPath $fixturePath) {
        throw "ReFS cleanup could not remove $fixturePath."
    }
    Write-Host "Removed ReFS fixture $fixturePath."
}
