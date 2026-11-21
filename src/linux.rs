use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

const BATCH: usize = 64;

#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped_extents: u32,
    extent_count: u32,
    reserved: u32,
    extents: [FiemapExtent; BATCH],
}

const _: () = {
    assert!(std::mem::size_of::<FiemapExtent>() == 56);
    assert!(std::mem::offset_of!(Fiemap, extents) == 32);
};

const FS_IOC_FIEMAP: u64 = 0xC020_660B;
const FIEMAP_FLAG_SYNC: u32 = 0x1;
const FIEMAP_EXTENT_LAST: u32 = 0x1;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub logical: u64,
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

impl Extent {
    pub fn shared(&self) -> bool {
        self.flags & FIEMAP_EXTENT_SHARED != 0
    }
}

pub fn extents(f: &File, sync: bool) -> io::Result<Vec<Extent>> {
    let mut out = Vec::new();
    let mut start = 0u64;
    loop {
        let mut req = Fiemap {
            start,
            length: u64::MAX - start,
            flags: if sync { FIEMAP_FLAG_SYNC } else { 0 },
            mapped_extents: 0,
            extent_count: BATCH as u32,
            reserved: 0,
            extents: [FiemapExtent::default(); BATCH],
        };
        let rc = unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, &mut req) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let n = req.mapped_extents as usize;
        if n == 0 {
            return Ok(out);
        }
        let mut last = false;
        for e in &req.extents[..n] {
            out.push(Extent {
                logical: e.logical,
                physical: e.physical,
                length: e.length,
                flags: e.flags,
            });
            last |= e.flags & FIEMAP_EXTENT_LAST != 0;
        }
        if last {
            return Ok(out);
        }
        let e = &req.extents[n - 1];
        start = e.logical + e.length;
    }
}

#[repr(C)]
struct DedupeInfo {
    dest_fd: i64,
    dest_offset: u64,
    bytes_deduped: u64,
    status: i32,
    reserved: u32,
}

#[repr(C)]
struct DedupeRange {
    src_offset: u64,
    src_length: u64,
    dest_count: u16,
    reserved1: u16,
    reserved2: u32,
    info: DedupeInfo,
}

const _: () = {
    assert!(std::mem::size_of::<DedupeRange>() == 56);
    assert!(std::mem::offset_of!(DedupeRange, info) == 24);
};

const FIDEDUPERANGE: u64 = 0xC018_9436;
const FILE_DEDUPE_RANGE_DIFFERS: i32 = 1;
const CHUNK: u64 = 16 * 1024 * 1024;

#[derive(Debug)]
pub enum DedupeOutcome {
    Deduped(u64),
    Differs,
}

pub fn dedupe(src: &File, dst: &File, len: u64) -> io::Result<DedupeOutcome> {
    let mut offset = 0u64;
    while offset < len {
        let length = (len - offset).min(CHUNK);
        let mut r = DedupeRange {
            src_offset: offset,
            src_length: length,
            dest_count: 1,
            reserved1: 0,
            reserved2: 0,
            info: DedupeInfo {
                dest_fd: dst.as_raw_fd() as i64,
                dest_offset: offset,
                bytes_deduped: 0,
                status: 0,
                reserved: 0,
            },
        };
        let rc = unsafe { libc::ioctl(src.as_raw_fd(), FIDEDUPERANGE as _, &mut r) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if r.info.status == FILE_DEDUPE_RANGE_DIFFERS {
            return Ok(DedupeOutcome::Differs);
        }
        if r.info.status < 0 {
            return Err(io::Error::from_raw_os_error(-r.info.status));
        }
        if r.info.bytes_deduped == 0 || r.info.bytes_deduped > length {
            return Err(io::Error::other("dedupe made no progress"));
        }
        offset += r.info.bytes_deduped;
    }
    Ok(DedupeOutcome::Deduped(offset))
}

pub const BTRFS_SUPER_MAGIC: i64 = 0x9123_683E;

pub fn fs_magic(path: &std::path::Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
    (rc == 0).then_some(st.f_type as i64)
}

pub fn fs_name(path: &std::path::Path) -> &'static str {
    match fs_magic(path) {
        Some(0x9123_683E) => "btrfs",
        Some(0x5846_5342) => "xfs",
        Some(0xEF53) => "ext4",
        Some(0x0102_1994) => "tmpfs",
        Some(0x2FC1_2FC1) => "zfs",
        Some(0xCA45_1A4E) => "bcachefs",
        Some(0x794C_7630) => "overlayfs",
        Some(0xF2F5_2010) => "f2fs",
        Some(0x6969) => "nfs",
        Some(_) => "other",
        None => "unknown",
    }
}
