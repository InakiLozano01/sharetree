import runpy
import unittest
from pathlib import Path
from textwrap import dedent


LINT = runpy.run_path(str(Path(__file__).with_name("comment-lint.py")))


def powershell_comments(source):
    return [
        (comment.line, comment.contents)
        for comment in LINT["source_comments"]("example.ps1", source.encode(), "pwsh")
    ]


def workflow_comments(workflow):
    return [
        comment.contents
        for comment in LINT["workflow_scripts"](".github/workflows/test.yml", workflow.encode())
    ]


class PowerShellComments(unittest.TestCase):
    def test_here_strings_preserve_comments_in_interpolated_code_and_after_the_closer(self):
        source = dedent('''\
            $verbatim = @'
            'quoted' # literal text
            # more literal text
            '@
            $expandable = @"
            "quoted" # literal text
            # more literal text
            <# more literal text #>
            $(Get-Item $path # interpolation comment
            )
            "@ # trailing comment
            # following comment
            ''')
        self.assertEqual(powershell_comments(source), [
            (9, b"# interpolation comment"),
            (11, b"# trailing comment"),
            (12, b"# following comment"),
        ])

    def test_requires_directives_are_exempt_and_regular_comments_are_counted(self):
        source = dedent('''\
            #Requires -RunAsAdministrator
              #rEqUiReS -version 7.4
            Write-Output 'hello' #Requires -Modules Pester
            #RequiresOther -Version 7.4
            # Requires elevation
            <# #Requires -Version 7.4 #>
            "$(Get-Date
            #Requires -Version 7.4
            )"
            ''')
        self.assertEqual(powershell_comments(source), [
            (4, b"#RequiresOther -Version 7.4"),
            (5, b"# Requires elevation"),
            (6, b"<# #Requires -Version 7.4 #>"),
            (8, b"#Requires -Version 7.4"),
        ])


class WorkflowComments(unittest.TestCase):
    def test_matrix_os_uses_windows_default_shell(self):
        for matrix in ('os: [windows-latest]', 'include: [{os: windows-latest}]'):
            with self.subTest(matrix=matrix):
                workflow = dedent(r'''
                    jobs:
                      build:
                        strategy:
                          matrix: {MATRIX}
                        runs-on: ${{ matrix.os }}
                        steps:
                          - run: |
                              #Requires -Version 7.4
                              Write-Host "C:\temp\" # Windows comment
                    ''').replace("MATRIX", matrix)
                self.assertEqual(workflow_comments(workflow), [b"# Windows comment"])

    def test_mixed_matrix_preserves_comment_occurrences_without_double_counting(self):
        workflow = dedent(r'''
            jobs:
              build:
                strategy:
                  matrix:
                    os: [ubuntu-latest, windows-latest]
                runs-on: ${{matrix.os}}
                steps:
                  - run: |
                      echo hello # shared comment
                      echo goodbye # shared comment
                  - run: |
                      #Requires -Version 7.4
                  - run: |
                      Write-Host "C:\temp\" # Windows comment
                  - shell: sh
                    run: |
                      cat <<'EOF'
                      # here-doc text
                      EOF
            ''')
        self.assertEqual(workflow_comments(workflow), [
            b"# shared comment",
            b"# shared comment",
            b"#Requires -Version 7.4",
            b"# Windows comment",
        ])


if __name__ == "__main__":
    unittest.main()
