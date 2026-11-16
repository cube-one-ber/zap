//! Packages from the local database, a sync database or an archive. Metadata
//! is parsed on first use: listing names never pays for descriptions.
use super::deps::Depend;
use super::log;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Reason {
    #[default]
    Explicit,
    Depend,
    Unknown,
}

pub const VALIDATION_NONE: u32 = 1;
pub const VALIDATION_MD5SUM: u32 = 1 << 1;
pub const VALIDATION_SHA256SUM: u32 = 1 << 2;
pub const VALIDATION_SIGNATURE: u32 = 1 << 3;

/// Everything a desc file can say about a package. Empty strings stand for
/// fields that are absent.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub base: String,
    pub desc: String,
    pub url: String,
    pub arch: String,
    pub packager: String,
    pub builddate: i64,
    pub installdate: i64,
    pub reason: Reason,
    pub validation: u32,
    /// Compressed (download) size.
    pub size: i64,
    pub isize: i64,
    pub md5sum: String,
    pub sha256sum: String,
    pub base64_sig: String,
    pub filename: String,
    pub licenses: Vec<String>,
    pub groups: Vec<String>,
    pub depends: Vec<Depend>,
    pub optdepends: Vec<Depend>,
    pub makedepends: Vec<Depend>,
    pub checkdepends: Vec<Depend>,
    pub conflicts: Vec<Depend>,
    pub provides: Vec<Depend>,
    pub replaces: Vec<Depend>,
    pub xdata: Vec<(String, String)>,
    pub has_scriptlet: bool,
}

#[derive(Clone, Debug, Default)]
pub struct File {
    pub name: String,
    pub size: i64,
    pub mode: u32,
}

/// A configuration file and the MD5 digest it had when installed.
#[derive(Clone, Debug, Default)]
pub struct Backup {
    pub name: String,
    pub hash: String,
}

#[derive(Clone, Debug, Default)]
pub struct Files {
    /// Sorted by name, as libalpm's filelist.
    pub files: Vec<File>,
    pub backup: Vec<Backup>,
}

impl Files {
    /// Binary search, as `alpm_filelist_contains`.
    pub fn contains(&self, path: &str) -> Option<&File> {
        self.files.binary_search_by(|f| f.name.as_str().cmp(path)).ok().map(|i| &self.files[i])
    }
}

#[derive(Clone, Debug)]
pub enum Origin {
    /// An entry in the local database directory.
    Local { dir: PathBuf },
    /// A sync database entry: the ranges of its files in the decompressed database.
    Sync { db: Arc<str>, data: Arc<super::snapshot::Bytes>, entries: Vec<std::ops::Range<usize>> },
    /// A package archive on disk.
    File { path: PathBuf },
}

#[derive(Debug)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub origin: Origin,
    // Boxed: a package cache holds tens of thousands of packages, most of
    // which are never examined.
    info: OnceLock<Box<Info>>,
    files: OnceLock<Files>,
}

impl Pkg {
    pub(crate) fn new(name: String, version: String, origin: Origin) -> Pkg {
        Pkg { name, version, origin, info: OnceLock::new(), files: OnceLock::new() }
    }

    pub(crate) fn with_info(name: String, version: String, origin: Origin, info: Info, files: Option<Files>) -> Pkg {
        let pkg = Pkg::new(name, version, origin);
        let _ = pkg.info.set(Box::new(info));
        if let Some(files) = files {
            let _ = pkg.files.set(files);
        }
        pkg
    }

    /// The package's metadata, read on first use.
    pub fn info(&self) -> &Info {
        self.info.get_or_init(|| Box::new(match &self.origin {
            Origin::Local { dir } => read_local_desc(self, dir),
            Origin::Sync { db, data, entries } => {
                let mut info = Info::default();
                for range in entries {
                    parse_desc(&mut info, &data[range.clone()], &self.name, &self.version, db, false);
                }
                info
            }
            Origin::File { .. } => Info::default(),
        }))
    }

    /// Installed files and backup entries; only local and archive packages have them.
    pub fn files(&self) -> &Files {
        self.files.get_or_init(|| match &self.origin {
            Origin::Local { dir } => read_local_files(dir),
            _ => Files::default(),
        })
    }

    /// Whether the file list records `path`, as a file or (with a trailing
    /// slash) a directory; `None` when the list cannot be read. Scanning the
    /// raw list avoids building it for every package searched.
    pub fn records_file(&self, path: &str) -> Option<bool> {
        let directory = format!("{path}/");
        if let Some(files) = self.files.get() {
            return Some(files.contains(path).is_some() || files.contains(&directory).is_some());
        }
        let Origin::Local { dir } = &self.origin else { return Some(false) };
        let bytes = std::fs::read(dir.join("files")).ok()?;
        let mut listing = false;
        for line in bytes.split(|&b| b == b'\n') {
            if line.is_empty() {
                listing = false;
            } else if line == b"%FILES%" && !listing {
                listing = true;
            } else if listing && (line == path.as_bytes() || line == directory.as_bytes()) {
                return Some(true);
            }
        }
        Some(false)
    }

    /// Whether the files list could be read, for callers that must tell an
    /// unreadable list from an empty one.
    pub fn files_readable(&self) -> bool {
        match &self.origin {
            Origin::Local { dir } => dir.join("files").exists(),
            _ => true,
        }
    }

    /// The repository this package belongs to: `local` for installed
    /// packages, empty for archives.
    pub fn db_name(&self) -> &str {
        match &self.origin {
            Origin::Local { .. } => "local",
            Origin::Sync { db, .. } => db,
            Origin::File { .. } => "",
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self.origin, Origin::Local { .. })
    }

    /// The archive path, for archive packages.
    pub fn path(&self) -> Option<&std::path::Path> {
        match &self.origin {
            Origin::File { path } => Some(path),
            _ => None,
        }
    }

    pub fn explicit(&self) -> bool {
        self.info().reason == Reason::Explicit
    }
}

// ── desc parsing ────────────────────────────────────────────────────────────

pub(crate) fn parsedate(line: &str) -> i64 {
    line.parse::<i64>().unwrap_or(0)
}

/// Sizes are bare digits; anything else is -1, as `_alpm_strtoofft`.
pub(crate) fn strtoofft(line: &str) -> i64 {
    if !line.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return -1;
    }
    line.parse::<u64>().map(|v| v as i64).unwrap_or(-1)
}

fn reason_of(name: &str, line: &str) -> Reason {
    match line {
        "0" => Reason::Explicit,
        "1" => Reason::Depend,
        _ => {
            log::error(&format!("unknown install reason for package {name}: {line}\n"));
            Reason::Unknown
        }
    }
}

/// Parse a desc (or depends, or files) entry of the local or a sync database
/// into `info`. Warnings name the database as libalpm does.
pub(crate) fn parse_desc(info: &mut Info, bytes: &[u8], name: &str, version: &str, db: &str, local: bool) {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split('\n').map(|l| l.trim_end_matches('\n'));
    // A block of values, ending at an empty line.
    fn block<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Vec<&'a str> {
        let mut out = Vec::new();
        for line in lines.by_ref() {
            if line.is_empty() {
                break;
            }
            out.push(line);
        }
        out
    }
    fn deps<'a>(lines: &mut impl Iterator<Item = &'a str>) -> Vec<Depend> {
        block(lines).into_iter().map(Depend::parse).collect()
    }
    let kind = if local { "local" } else { "sync" };
    while let Some(line) = lines.next() {
        if line.is_empty() {
            continue;
        }
        macro_rules! next {
            () => {
                lines.next().unwrap_or("").to_string()
            };
        }
        match line {
            "%NAME%" => {
                if next!() != name {
                    log::error(&format!("{db} database is inconsistent: name mismatch on package {name}\n"));
                }
            }
            "%VERSION%" => {
                if next!() != version {
                    log::error(&format!("{db} database is inconsistent: version mismatch on package {name}\n"));
                }
            }
            "%FILENAME%" if !local => info.filename = next!(),
            "%BASE%" => info.base = next!(),
            "%DESC%" => info.desc = next!(),
            "%URL%" => info.url = next!(),
            "%ARCH%" => info.arch = next!(),
            "%PACKAGER%" => info.packager = next!(),
            "%BUILDDATE%" => info.builddate = parsedate(&next!()),
            "%INSTALLDATE%" if local => info.installdate = parsedate(&next!()),
            "%REASON%" if local => info.reason = reason_of(name, &next!()),
            "%SIZE%" if local => info.isize = strtoofft(&next!()),
            "%CSIZE%" if !local => info.size = strtoofft(&next!()),
            "%ISIZE%" if !local => info.isize = strtoofft(&next!()),
            "%MD5SUM%" if !local => info.md5sum = next!(),
            "%SHA256SUM%" if !local => info.sha256sum = next!(),
            "%PGPSIG%" if !local => info.base64_sig = next!(),
            "%GROUPS%" => info.groups = block(&mut lines).into_iter().map(String::from).collect(),
            "%LICENSE%" => info.licenses = block(&mut lines).into_iter().map(String::from).collect(),
            "%VALIDATION%" if local => {
                for v in block(&mut lines) {
                    info.validation |= match v {
                        "none" => VALIDATION_NONE,
                        "md5" => VALIDATION_MD5SUM,
                        "sha256" => VALIDATION_SHA256SUM,
                        "pgp" => VALIDATION_SIGNATURE,
                        _ => {
                            log::warning(&format!("unknown validation type for package {name}: {v}\n"));
                            0
                        }
                    };
                }
            }
            "%REPLACES%" => info.replaces = deps(&mut lines),
            "%DEPENDS%" => info.depends = deps(&mut lines),
            "%OPTDEPENDS%" => info.optdepends = deps(&mut lines),
            "%MAKEDEPENDS%" => info.makedepends = deps(&mut lines),
            "%CHECKDEPENDS%" => info.checkdepends = deps(&mut lines),
            "%CONFLICTS%" => info.conflicts = deps(&mut lines),
            "%PROVIDES%" => info.provides = deps(&mut lines),
            "%FILES%" if !local => {
                block(&mut lines);
            }
            "%XDATA%" if local => xdata(info, block(&mut lines)),
            "%DATA%" if !local => xdata(info, block(&mut lines)),
            _ => {
                log::warning(&format!("{name}: unknown key '{line}' in {kind} database\n"));
                block(&mut lines);
            }
        }
    }
}

fn xdata(info: &mut Info, entries: Vec<&str>) {
    for entry in entries {
        if let Some((key, value)) = entry.split_once('=') {
            info.xdata.push((key.to_string(), value.to_string()));
        }
    }
}

fn read_local_desc(pkg: &Pkg, dir: &std::path::Path) -> Info {
    let path = dir.join("desc");
    let mut info = Info::default();
    match std::fs::read(&path) {
        Ok(bytes) => parse_desc(&mut info, &bytes, &pkg.name, &pkg.version, "local", true),
        Err(e) => log::error(&format!("could not open file {}: {}\n", path.display(), super::strerror(&e))),
    }
    info.has_scriptlet = dir.join("install").exists();
    info
}

/// Parse a local `files` entry: %FILES% and %BACKUP%.
pub(crate) fn parse_files(bytes: &[u8]) -> Files {
    let text = String::from_utf8_lossy(bytes);
    let mut files = Files::default();
    let mut lines = text.split('\n');
    while let Some(line) = lines.next() {
        match line {
            "%FILES%" => {
                for entry in lines.by_ref() {
                    if entry.is_empty() {
                        break;
                    }
                    files.files.push(File { name: entry.to_string(), size: 0, mode: 0 });
                }
            }
            "%BACKUP%" => {
                for entry in lines.by_ref() {
                    if entry.is_empty() {
                        break;
                    }
                    let (name, hash) = entry.split_once('\t').unwrap_or((entry, ""));
                    files.backup.push(Backup { name: name.to_string(), hash: hash.to_string() });
                }
            }
            _ => {}
        }
    }
    sort_files(&mut files.files);
    files
}

pub(crate) fn sort_files(files: &mut [File]) {
    // Usually already sorted; checking is cheaper than sorting.
    if !files.windows(2).all(|w| w[0].name <= w[1].name) {
        files.sort_by(|a, b| a.name.cmp(&b.name));
    }
}

fn read_local_files(dir: &std::path::Path) -> Files {
    let path = dir.join("files");
    match std::fs::read(&path) {
        Ok(bytes) => parse_files(&bytes),
        Err(e) => {
            log::error(&format!("could not open file {}: {}\n", path.display(), super::strerror(&e)));
            Files::default()
        }
    }
}
