//! The local database (a directory of `name-version/` entries) and sync
//! databases (compressed tar archives of the same entries).
use super::log;
use super::package::{Origin, Pkg};
use super::snapshot;
use super::{Error, ErrorKind, Result};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

pub const USAGE_SYNC: u32 = 1;
pub const USAGE_SEARCH: u32 = 1 << 1;
pub const USAGE_INSTALL: u32 = 1 << 2;
pub const USAGE_UPGRADE: u32 = 1 << 3;
pub const USAGE_ALL: u32 = (1 << 4) - 1;

#[derive(Default)]
struct Cache {
    /// Sorted by name, as libalpm's pkgcache.
    pkgs: Vec<Arc<Pkg>>,
    index: HashMap<String, usize>,
}

impl Cache {
    fn new(mut pkgs: Vec<Arc<Pkg>>) -> Cache {
        pkgs.sort_by(|a, b| a.name.cmp(&b.name));
        let index = pkgs.iter().enumerate().map(|(i, p)| (p.name.clone(), i)).collect();
        Cache { pkgs, index }
    }
}

pub struct Db {
    pub name: String,
    pub siglevel: i32,
    pub usage: u32,
    pub servers: Vec<String>,
    /// Tried before `servers`; their failures are not reported.
    pub cache_servers: Vec<String>,
    /// The local database directory, or the sync database file.
    pub path: PathBuf,
    local: bool,
    cache: OnceLock<Cache>,
    /// Where sync database snapshots are kept; `None` reads the database
    /// file every time, as libalpm does.
    pub snapshot_dir: Option<PathBuf>,
    /// Its signature was refused, so it lists no packages.
    pub invalid: bool,
}

impl Db {
    pub(crate) fn local(dbpath: &str) -> Db {
        Db {
            name: "local".into(),
            siglevel: 0,
            usage: USAGE_ALL,
            servers: Vec::new(),
            cache_servers: Vec::new(),
            path: PathBuf::from(format!("{dbpath}local/")),
            local: true,
            cache: OnceLock::new(),
            snapshot_dir: None,
            invalid: false,
        }
    }

    pub(crate) fn sync(dbpath: &str, name: &str, siglevel: i32) -> Db {
        Db {
            name: name.into(),
            siglevel,
            usage: USAGE_ALL,
            servers: Vec::new(),
            cache_servers: Vec::new(),
            path: PathBuf::from(format!("{dbpath}sync/{name}.db")),
            local: false,
            cache: OnceLock::new(),
            snapshot_dir: None,
            invalid: false,
        }
    }

    pub fn is_local(&self) -> bool {
        self.local
    }

    fn cache(&self) -> &Cache {
        self.cache.get_or_init(|| {
            let loaded = if self.local {
                self.populate_local()
            } else if self.invalid {
                Ok(Vec::new())
            } else {
                self.populate_sync()
            };
            Cache::new(loaded.unwrap_or_default())
        })
    }

    /// Every package, sorted by name. A database that cannot be read is empty.
    pub fn pkgs(&self) -> &[Arc<Pkg>] {
        &self.cache().pkgs
    }

    pub fn get(&self, name: &str) -> Option<&Arc<Pkg>> {
        let cache = self.cache();
        cache.index.get(name).map(|&i| &cache.pkgs[i])
    }

    /// Drop a package from the loaded cache, after its entry was removed.
    pub(crate) fn cache_remove(&mut self, name: &str) {
        self.cache();
        if let Some(cache) = self.cache.get_mut() {
            if let Some(&at) = cache.index.get(name) {
                cache.pkgs.remove(at);
                *cache = Cache::new(std::mem::take(&mut cache.pkgs));
            }
        }
    }

    /// Add a package to the loaded cache, replacing one of the same name.
    pub(crate) fn cache_replace(&mut self, pkg: Arc<Pkg>) {
        self.cache();
        if let Some(cache) = self.cache.get_mut() {
            match cache.index.get(&pkg.name) {
                Some(&at) => cache.pkgs[at] = pkg,
                None => {
                    cache.pkgs.push(pkg);
                    *cache = Cache::new(std::mem::take(&mut cache.pkgs));
                }
            }
        }
    }

    /// Forget the package cache, after the database file changed.
    pub(crate) fn invalidate(&mut self) {
        self.cache = OnceLock::new();
    }

    /// Parse every package's metadata now, in parallel, rather than one at a
    /// time on first use.
    pub fn preload(&self) {
        parallel(self.pkgs(), |p| {
            p.info();
        });
    }

    /// Parse every installed package's file list in parallel.
    pub fn preload_files(&self) {
        parallel(self.pkgs(), |p| {
            p.files();
        });
    }

    /// Search names, descriptions, provisions and groups with POSIX extended
    /// regular expressions; each needle narrows the previous result, as
    /// `alpm_db_search`.
    pub fn search(&self, needles: &[&str]) -> Result<Vec<Arc<Pkg>>> {
        if self.usage & USAGE_SEARCH == 0 {
            return Ok(Vec::new());
        }
        self.preload();
        let mut list: Vec<Arc<Pkg>> = self.pkgs().to_vec();
        for needle in needles {
            let reg = Regex::new(needle)?;
            let keep: Vec<bool> = parallel_map(&list, |pkg| {
                let info = pkg.info();
                (reg.is_match(&pkg.name) || pkg.name.contains(needle))
                    || (!info.desc.is_empty() && reg.is_match(&info.desc))
                    || info.provides.iter().any(|p| reg.is_match(&p.name))
                    || info.groups.iter().any(|g| reg.is_match(g))
            });
            list = list.into_iter().zip(keep).filter_map(|(p, k)| k.then_some(p)).collect();
        }
        Ok(list)
    }

    fn populate_local(&self) -> Result<Vec<Arc<Pkg>>> {
        let entries = std::fs::read_dir(&self.path).map_err(|_| Error::new(ErrorKind::DbOpen))?;
        let mut seen: HashMap<String, ()> = HashMap::new();
        let mut pkgs = Vec::new();
        for entry in entries.flatten() {
            let Ok(name) = entry.file_name().into_string() else { continue };
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let Some((pkgname, version)) = split_name(&name) else {
                log::error(&format!("invalid name for database entry '{name}'\n"));
                continue;
            };
            if seen.insert(pkgname.to_string(), ()).is_some() {
                log::error(&format!("duplicated database entry '{pkgname}'\n"));
                continue;
            }
            // Installed packages with bad metadata are kept, so they can still be removed.
            check_meta(pkgname, version);
            let dir = entry.path();
            pkgs.push(Arc::new(Pkg::new(pkgname.into(), version.into(), Origin::Local { dir })));
        }
        Ok(pkgs)
    }

    fn populate_sync(&self) -> Result<Vec<Arc<Pkg>>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(Error::new(ErrorKind::DbNotFound)),
            Err(_) => return Err(Error::new(ErrorKind::DbOpen)),
        };
        let db_name: Arc<str> = Arc::from(self.name.as_str());
        // The key comes from the open file, so it names exactly the bytes read.
        let snapshot = match (&self.snapshot_dir, file.metadata()) {
            (Some(dir), Ok(meta)) => Some((snapshot::file(dir, &self.name, &self.path), snapshot::key(&meta))),
            _ => None,
        };
        if let Some((path, key)) = &snapshot {
            if let Some(pkgs) = snapshot::load(path, *key, &db_name) {
                return Ok(pkgs);
            }
        }
        let mut compressed = Vec::new();
        file.read_to_end(&mut compressed).map_err(|_| Error::new(ErrorKind::DbOpen))?;
        let data = Arc::new(snapshot::Bytes::Owned(decompress(&compressed).map_err(|_| {
            log::error(&format!("could not open file {}: Unrecognized archive format\n", self.path.display()));
            Error::new(ErrorKind::DbOpen)
        })?));
        // A database that logs problems is not snapshotted, so every load
        // reports them again.
        let mut clean = true;
        let mut pkgs: Vec<Arc<Pkg>> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        // Entries of one package are adjacent; group them by directory.
        let mut current: Option<(String, String, Vec<std::ops::Range<usize>>)> = None;
        let flush = |current: &mut Option<(String, String, Vec<std::ops::Range<usize>>)>,
                     pkgs: &mut Vec<Arc<Pkg>>,
                     index: &mut HashMap<String, usize>| {
            if let Some((name, version, entries)) = current.take() {
                if index.contains_key(&name) {
                    return;
                }
                index.insert(name.clone(), pkgs.len());
                let origin = Origin::Sync { db: db_name.clone(), data: data.clone(), entries };
                pkgs.push(Arc::new(Pkg::new(name, version, origin)));
            }
        };
        for entry in tar_entries(&data).map_err(|_| {
            log::error(&format!("could not read db '{}' (Truncated tar archive)\n", self.name));
            Error::new(ErrorKind::Libarchive)
        })? {
            if entry.dir {
                continue;
            }
            let (dir, file) = match entry.path.find('/') {
                Some(at) => (&entry.path[..at], Some(&entry.path[at + 1..])),
                None => (&entry.path[..], None),
            };
            let Some((name, version)) = split_name(dir) else {
                clean = false;
                log::error(&format!("invalid name for database entry '{}'\n", entry.path));
                log::error(&format!(
                    "could not parse package description file '{}' from db '{}'\n",
                    entry.path, self.name
                ));
                continue;
            };
            let Some(file) = file else {
                clean = false;
                log::warning(&format!("unknown database file: {}\n", entry.path));
                continue;
            };
            if !matches!(file, "desc" | "depends" | "files") {
                continue;
            }
            if current.as_ref().is_none_or(|(n, _, _)| n != name) {
                if !index.contains_key(name) && !check_meta(name, version) {
                    clean = false;
                    log::error(&format!(
                        "could not parse package description file '{}' from db '{}'\n",
                        entry.path, self.name
                    ));
                    continue;
                }
                flush(&mut current, &mut pkgs, &mut index);
                current = Some((name.into(), version.into(), Vec::new()));
            }
            current.as_mut().unwrap().2.push(entry.data);
        }
        flush(&mut current, &mut pkgs, &mut index);
        if let (Some((path, key)), true) = (&snapshot, clean) {
            snapshot::save(path, *key, &pkgs, &data);
        }
        Ok(pkgs)
    }
}

/// `name-pkgver-pkgrel`, split at the second hyphen from the end.
pub fn split_name(target: &str) -> Option<(&str, &str)> {
    let end = target.find('/').unwrap_or(target.len());
    let s = &target[..end];
    let last = s.rfind('-')?;
    let second = s[..last].rfind('-')?;
    if second == 0 {
        return None;
    }
    Some((&s[..second], &s[second + 1..]))
}

/// Metadata problems libalpm reports but tolerates for installed packages.
/// `_alpm_pkg_check_meta`: log metadata that could make database entries
/// overlap. Returns whether the package is acceptable.
pub(crate) fn check_meta(name: &str, version: &str) -> bool {
    let mut ok = true;
    let mut bad = |why: &str| {
        ok = false;
        log::error(&format!("invalid metadata for package {name}-{version} ({why})\n"));
    };
    if name.starts_with(['-', '.']) {
        bad("package name cannot start with '.' or '-'");
    }
    // libalpm passes the name as the pattern here, so this only fires for
    // names that are globs matching that literal text.
    if super::fnmatch(name, "[![:alnum:]+_.@-]") {
        bad("package name contains invalid characters");
    }
    if version.find('-').is_some_and(|at| version[at + 1..].contains('-')) {
        bad("package version contains invalid characters");
    }
    if version.contains('/') {
        bad("package version contains invalid characters");
    }
    ok
}

// ── Archives ────────────────────────────────────────────────────────────────

/// Decompress gzip, zstd, xz or bzip2 by their magic bytes; anything else is
/// taken to be an uncompressed tar archive.
pub fn decompress(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len() * 5);
    if bytes.starts_with(&[0x1f, 0x8b]) {
        flate2::read::MultiGzDecoder::new(bytes).read_to_end(&mut out)?;
    } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::stream::read::Decoder::new(bytes)?.read_to_end(&mut out)?;
    } else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        liblzma::read::XzDecoder::new_multi_decoder(bytes).read_to_end(&mut out)?;
    } else if bytes.starts_with(b"BZh") {
        bzip2::read::MultiBzDecoder::new(bytes).read_to_end(&mut out)?;
    } else if bytes.len() >= 512 && &bytes[257..262] == b"ustar" || bytes.is_empty() {
        out.extend_from_slice(bytes);
    } else {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(out)
}

pub struct TarEntry {
    pub path: String,
    pub dir: bool,
    pub data: std::ops::Range<usize>,
}

pub(crate) fn octal(field: &[u8]) -> Option<usize> {
    // GNU base-256 for large values.
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        return Some(field[1..].iter().fold(0usize, |acc, &b| (acc << 8) | b as usize));
    }
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if digits.is_empty() {
        return Some(0);
    }
    usize::from_str_radix(digits, 8).ok()
}

fn cstring(field: &[u8]) -> &[u8] {
    &field[..field.iter().position(|&b| b == 0).unwrap_or(field.len())]
}

/// The entries of an in-memory tar archive, with the byte range of each
/// entry's data. Handles ustar prefixes, GNU long names and pax paths.
pub fn tar_entries(data: &[u8]) -> std::result::Result<Vec<TarEntry>, ()> {
    let mut entries = Vec::new();
    let mut at = 0;
    let mut long_name: Option<String> = None;
    while at + 512 <= data.len() {
        let header = &data[at..at + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let size = octal(&header[124..136]).ok_or(())?;
        let start = at + 512;
        let end = start.checked_add(size).ok_or(())?;
        if end > data.len() {
            return Err(());
        }
        let kind = header[156];
        at = start + size.div_ceil(512) * 512;
        match kind {
            b'L' => {
                long_name = Some(String::from_utf8_lossy(cstring(&data[start..end])).into_owned());
                continue;
            }
            b'x' => {
                for record in String::from_utf8_lossy(&data[start..end]).split('\n') {
                    if let Some((_, kv)) = record.split_once(' ') {
                        if let Some(path) = kv.strip_prefix("path=") {
                            long_name = Some(path.to_string());
                        }
                    }
                }
                continue;
            }
            b'g' => continue,
            _ => {}
        }
        let path = long_name.take().unwrap_or_else(|| {
            let name = cstring(&header[0..100]);
            let prefix = if &header[257..262] == b"ustar" { cstring(&header[345..500]) } else { &[] };
            if prefix.is_empty() {
                String::from_utf8_lossy(name).into_owned()
            } else {
                format!("{}/{}", String::from_utf8_lossy(prefix), String::from_utf8_lossy(name))
            }
        });
        let dir = kind == b'5' || path.ends_with('/');
        entries.push(TarEntry { path, dir, data: start..end });
    }
    Ok(entries)
}

// ── Regular expressions ─────────────────────────────────────────────────────

/// A POSIX extended, case-insensitive regular expression, as libalpm's search.
pub struct Regex(libc::regex_t);

// regexec on a compiled expression does not modify it.
unsafe impl Sync for Regex {}
unsafe impl Send for Regex {}

impl Regex {
    pub fn new(pattern: &str) -> Result<Regex> {
        let c = std::ffi::CString::new(pattern).map_err(|_| Error::new(ErrorKind::InvalidRegex))?;
        let mut reg: libc::regex_t = unsafe { std::mem::zeroed() };
        let flags = libc::REG_EXTENDED | libc::REG_NOSUB | libc::REG_ICASE | libc::REG_NEWLINE;
        if unsafe { libc::regcomp(&mut reg, c.as_ptr(), flags) } != 0 {
            return Err(Error::new(ErrorKind::InvalidRegex));
        }
        Ok(Regex(reg))
    }

    pub fn is_match(&self, text: &str) -> bool {
        let Ok(c) = std::ffi::CString::new(text) else { return false };
        unsafe { libc::regexec(&self.0, c.as_ptr(), 0, std::ptr::null_mut(), 0) == 0 }
    }
}

impl Drop for Regex {
    fn drop(&mut self) {
        unsafe { libc::regfree(&mut self.0) };
    }
}

// ── Parallelism ─────────────────────────────────────────────────────────────

fn workers(items: usize) -> usize {
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    cpus.min(items / 64).max(1)
}

/// Run `f` on every item across the available cores.
pub fn parallel<T: Sync>(items: &[T], f: impl Fn(&T) + Sync) {
    let n = workers(items.len());
    if n == 1 {
        return items.iter().for_each(f);
    }
    let chunk = items.len().div_ceil(n);
    std::thread::scope(|s| {
        for part in items.chunks(chunk) {
            let f = &f;
            s.spawn(move || part.iter().for_each(f));
        }
    });
}

/// Map every item across the available cores, keeping order.
/// The index of the first item, in order, for which `f` holds. Workers take
/// small blocks front to back and stop once a match earlier than anything
/// left is known, so an early match costs little.
pub fn find_first<T: Sync>(items: &[T], f: impl Fn(&T) -> bool + Sync) -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    const BLOCK: usize = 8;
    let n = workers(items.len());
    if n == 1 {
        return items.iter().position(f);
    }
    let next = AtomicUsize::new(0);
    let best = AtomicUsize::new(usize::MAX);
    std::thread::scope(|s| {
        for _ in 0..n {
            s.spawn(|| loop {
                let start = next.fetch_add(BLOCK, Relaxed);
                if start >= items.len() || start > best.load(Relaxed) {
                    return;
                }
                let end = (start + BLOCK).min(items.len());
                if let Some(at) = items[start..end].iter().position(&f) {
                    best.fetch_min(start + at, Relaxed);
                    return;
                }
            });
        }
    });
    let found = best.into_inner();
    (found != usize::MAX).then_some(found)
}

pub fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let n = workers(items.len());
    if n == 1 {
        return items.iter().map(f).collect();
    }
    let chunk = items.len().div_ceil(n);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(chunk)
            .map(|part| {
                let f = &f;
                s.spawn(move || part.iter().map(f).collect::<Vec<R>>())
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("worker panicked")).collect()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn find_first_returns_the_earliest_match_in_order() {
        let items: Vec<usize> = (0..10_000).collect();
        assert_eq!(Some(4321), super::find_first(&items, |&i| i >= 4321 && i % 7 == 2));
        assert_eq!(None, super::find_first(&items, |&i| i > 20_000));
    }

    use super::*;

    #[test]
    fn database_entries_split_at_the_second_hyphen_from_the_end() {
        assert_eq!(Some(("foo-bar", "1.0-2")), split_name("foo-bar-1.0-2"));
        assert_eq!(Some(("foo", "1:2.0-1")), split_name("foo-1:2.0-1/desc"));
        assert_eq!(None, split_name("foo-1"));
        assert_eq!(None, split_name("-1-2"));
    }

    #[test]
    fn search_matches_regex_names_descriptions_and_plain_text() {
        let reg = Regex::new("^LIN.x$").unwrap();
        assert!(reg.is_match("linux"));
        assert!(!reg.is_match("linux-lts"));
        assert!(Regex::new("(").is_err());
    }
}
