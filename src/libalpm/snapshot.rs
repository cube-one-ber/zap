//! Snapshots of parsed sync databases. Inflating a large repository database
//! dominates every command that reads it, so the entries a package cache
//! needs are kept, uncompressed and indexed, beside a key naming the exact
//! database file they came from. pacman replaces database files by rename,
//! so a sync changes the key and the next load rebuilds the snapshot.
//!
//! A snapshot is only a faster way to read the same bytes. A damaged or
//! foreign file is ignored, and the root worker never uses snapshots.
use super::package::{Origin, Pkg};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAGIC: &[u8; 8] = b"ZAPSNAP1";

/// Identifies one version of a database file: device, inode, size and
/// modification time.
pub type Key = [u64; 5];

pub fn key(meta: &std::fs::Metadata) -> Key {
    [meta.dev(), meta.ino(), meta.size(), meta.mtime() as u64, meta.mtime_nsec() as u64]
}

/// Where the snapshot of the database at `db_path` lives in `dir`. The path
/// is hashed in, so databases of different roots never share a file.
pub fn file(dir: &Path, name: &str, db_path: &Path) -> PathBuf {
    // FNV-1a: stable across builds, unlike the standard library's hasher.
    let hash = db_path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    dir.join(format!("{name}-{hash:016x}.snapshot"))
}

/// The contents of a database: inflated into memory, or a mapped snapshot.
pub enum Bytes {
    Owned(Vec<u8>),
    Mapped(Mapping),
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bytes({})", self.len())
    }
}

impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Bytes::Owned(bytes) => bytes,
            Bytes::Mapped(map) => map.bytes(),
        }
    }
}

/// A read-only private mapping of a whole file. Snapshots are replaced by
/// rename, never rewritten in place, so a mapped file never shrinks.
pub struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

// The mapping is read-only and owned; sharing it across threads is safe.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    fn open(path: &Path) -> Option<Mapping> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path).ok()?;
        let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
        if len == 0 {
            return None;
        }
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0)
        };
        (ptr != libc::MAP_FAILED).then_some(Mapping { ptr, len })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

/// The packages of a snapshot made from the database version `key`, or
/// `None` when there is no such snapshot or it does not parse.
pub fn load(path: &Path, key: Key, db: &Arc<str>) -> Option<Vec<Arc<Pkg>>> {
    let map = Mapping::open(path)?;
    let bytes = map.bytes();
    let mut r = Reader { bytes, at: 0 };
    if r.take(8)? != MAGIC || (0..5).map(|_| r.u64()).collect::<Option<Vec<_>>>()? != key {
        return None;
    }
    let count = r.u32()? as usize;
    let index_len = r.u64()? as usize;
    let data_start = r.at.checked_add(index_len)?;
    if data_start > bytes.len() {
        return None;
    }
    let mut parsed = Vec::with_capacity(count);
    for _ in 0..count {
        let name = r.text()?;
        let version = r.text()?;
        let n = r.take(1)?[0] as usize;
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let start = data_start.checked_add(r.u32()? as usize)?;
            let end = start.checked_add(r.u32()? as usize)?;
            if end > bytes.len() {
                return None;
            }
            entries.push(start..end);
        }
        parsed.push((name, version, entries));
    }
    if r.at != data_start {
        return None;
    }
    let data = Arc::new(Bytes::Mapped(map));
    let pkgs = parsed
        .into_iter()
        .map(|(name, version, entries)| {
            let origin = Origin::Sync { db: db.clone(), data: data.clone(), entries };
            Arc::new(Pkg::new(name, version, origin))
        })
        .collect();
    Some(pkgs)
}

/// Record `pkgs`, whose entries index `data`, as the snapshot of database
/// version `key`. Failures are ignored: the snapshot is only an accelerator.
pub fn save(path: &Path, key: Key, pkgs: &[Arc<Pkg>], data: &[u8]) {
    let mut index = Vec::new();
    let mut blob = Vec::new();
    for pkg in pkgs {
        let Origin::Sync { entries, .. } = &pkg.origin else { return };
        for text in [&pkg.name, &pkg.version] {
            let Ok(len) = u16::try_from(text.len()) else { return };
            index.extend_from_slice(&len.to_le_bytes());
            index.extend_from_slice(text.as_bytes());
        }
        let Ok(n) = u8::try_from(entries.len()) else { return };
        index.push(n);
        for range in entries {
            let (Ok(start), Ok(len)) = (u32::try_from(blob.len()), u32::try_from(range.len())) else { return };
            index.extend_from_slice(&start.to_le_bytes());
            index.extend_from_slice(&len.to_le_bytes());
            blob.extend_from_slice(&data[range.clone()]);
        }
    }
    let mut out = Vec::with_capacity(64 + index.len() + blob.len());
    out.extend_from_slice(MAGIC);
    key.iter().for_each(|k| out.extend_from_slice(&k.to_le_bytes()));
    out.extend_from_slice(&(pkgs.len() as u32).to_le_bytes());
    out.extend_from_slice(&(index.len() as u64).to_le_bytes());
    out.extend_from_slice(&index);
    out.extend_from_slice(&blob);
    let _ = write_atomically(path, &out);
}

/// Write through a private temporary file and rename it into place, so a
/// reader sees the old snapshot or the new one, never part of one.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn text(&mut self) -> Option<String> {
        let len = u16::from_le_bytes(self.take(2)?.try_into().ok()?) as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshots_round_trip_and_refuse_other_database_versions() {
        let tmp = crate::testing::TempDir::new();
        let path = tmp.path().join("sub/core.snapshot");
        let db: Arc<str> = Arc::from("core");
        let data = Arc::new(Bytes::Owned(b"%NAME%\nfoo\n\n%DEPENDS%\nbar\n\n".to_vec()));
        let entries: Vec<std::ops::Range<usize>> = vec![0..12, 12..data.len()];
        let origin = Origin::Sync { db: db.clone(), data: data.clone(), entries };
        let pkgs = vec![Arc::new(Pkg::new("foo".into(), "1.0-1".into(), origin))];
        save(&path, [1, 2, 3, 4, 5], &pkgs, &data);

        let loaded = load(&path, [1, 2, 3, 4, 5], &db).unwrap();
        assert_eq!(1, loaded.len());
        assert_eq!(("foo", "1.0-1"), (loaded[0].name.as_str(), loaded[0].version.as_str()));
        assert_eq!("bar", loaded[0].info().depends[0].name);
        assert!(load(&path, [1, 2, 3, 4, 6], &db).is_none());

        // A truncated snapshot is ignored rather than trusted.
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        assert!(load(&path, [1, 2, 3, 4, 5], &db).is_none());
    }
}
