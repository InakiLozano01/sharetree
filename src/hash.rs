use std::fs::File;
use std::io::{self, Read};

use sha1_checked::Digest;

use crate::git::ObjectFormat;

const BUF: usize = 256 * 1024;

enum State {
    Sha1(Box<sha1_checked::Sha1>),
    Sha256(sha2::Sha256),
}

struct BlobHasher {
    state: State,
}

impl BlobHasher {
    fn new(fmt: ObjectFormat, len: u64) -> BlobHasher {
        let header = format!("blob {len}\0");
        let mut state = match fmt {
            ObjectFormat::Sha1 => State::Sha1(Box::default()),
            ObjectFormat::Sha256 => State::Sha256(sha2::Sha256::new()),
        };
        match &mut state {
            State::Sha1(h) => h.update(header.as_bytes()),
            State::Sha256(h) => h.update(header.as_bytes()),
        }
        BlobHasher { state }
    }

    fn update(&mut self, data: &[u8]) {
        match &mut self.state {
            State::Sha1(h) => h.update(data),
            State::Sha256(h) => h.update(data),
        }
    }

    fn finish(self) -> Option<Vec<u8>> {
        match self.state {
            State::Sha1(h) => {
                let r = h.try_finalize();
                (!r.has_collision()).then(|| r.hash().to_vec())
            }
            State::Sha256(h) => Some(h.finalize().to_vec()),
        }
    }
}

pub fn blob_oid(f: &mut File, len: u64, fmt: ObjectFormat) -> io::Result<Option<Vec<u8>>> {
    advise_sequential(f, len);
    let mut h = BlobHasher::new(fmt, len);
    let mut buf = vec![0u8; len.saturating_add(1).min(BUF as u64) as usize];
    let mut left = len;
    loop {
        let n = match f.read(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        if n as u64 > left {
            return Ok(None);
        }
        h.update(&buf[..n]);
        left -= n as u64;
    }
    if left != 0 {
        return Ok(None);
    }
    Ok(h.finish())
}

#[cfg(target_os = "linux")]
fn advise_sequential(f: &File, len: u64) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, len as i64, libc::POSIX_FADV_SEQUENTIAL);
    }
}

#[cfg(not(target_os = "linux"))]
fn advise_sequential(_f: &File, _len: u64) {}

#[cfg(test)]
pub fn to_hex(raw: &[u8]) -> String {
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn oid_of(data: &[u8], fmt: ObjectFormat) -> Option<String> {
        let mut t = tempfile::tempfile().unwrap();
        t.write_all(data).unwrap();
        use std::io::Seek;
        t.rewind().unwrap();
        blob_oid(&mut t, data.len() as u64, fmt)
            .unwrap()
            .map(|o| to_hex(&o))
    }

    #[test]
    fn matches_git_hash_object() {
        assert_eq!(
            oid_of(b"hello\n", ObjectFormat::Sha1).unwrap(),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
        assert_eq!(
            oid_of(b"", ObjectFormat::Sha1).unwrap(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
        assert_eq!(
            oid_of(b"hello\n", ObjectFormat::Sha256).unwrap(),
            "2cf8d83d9ee29543b34a87727421fdecb7e3f3a183d337639025de576db9ebb4"
        );
    }

    #[test]
    fn length_must_match_exactly() {
        let mut t = tempfile::tempfile().unwrap();
        t.write_all(b"hello\n").unwrap();
        use std::io::Seek;
        t.rewind().unwrap();
        assert_eq!(blob_oid(&mut t, 5, ObjectFormat::Sha1).unwrap(), None);
        t.rewind().unwrap();
        assert_eq!(blob_oid(&mut t, 7, ObjectFormat::Sha1).unwrap(), None);
    }

    #[test]
    fn shattered_collision_is_rejected() {
        let prefix = include_bytes!("../tests/data/shattered-1-prefix.bin");
        let r = sha1_checked::Sha1::try_digest(prefix);
        assert!(r.has_collision(), "sha1dc must flag the SHAttered block");
        let mut h = BlobHasher {
            state: State::Sha1(Box::default()),
        };
        h.update(prefix);
        assert_eq!(h.finish(), None);
    }
}
