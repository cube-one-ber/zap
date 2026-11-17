//! The subset of libalpm (pacman 7.1.0) that zap uses, reimplemented in Rust:
//! configuration, the local and sync databases, dependency resolution and
//! transactions. Names follow libalpm's so the two can be read side by side.
pub mod add;
pub mod archive;
pub mod compress;
pub mod conflict;
pub mod db;
pub mod dload;
pub mod deps;
pub mod diskspace;
pub mod hook;
pub mod local;
pub mod log;
pub mod package;
pub mod remove;
pub mod sandbox;
pub mod signing;
pub mod snapshot;
pub mod sync;
pub mod trans;
pub mod util;
pub mod version;

pub use db::Db;
pub use deps::{DepMissing, Depend};
pub use log::{Event, Frontend, Progress, Question};
pub use package::{Pkg, Reason};
pub use version::vercmp;

use std::fmt;
use std::sync::Arc;

pub const LIBALPM_VERSION: &str = "16.0.1";

pub const SIG_PACKAGE: i32 = 1;
pub const SIG_PACKAGE_OPTIONAL: i32 = 1 << 1;
pub const SIG_PACKAGE_MARGINAL_OK: i32 = 1 << 2;
pub const SIG_PACKAGE_UNKNOWN_OK: i32 = 1 << 3;
pub const SIG_DATABASE: i32 = 1 << 10;
pub const SIG_DATABASE_OPTIONAL: i32 = 1 << 11;
pub const SIG_DATABASE_MARGINAL_OK: i32 = 1 << 12;
pub const SIG_DATABASE_UNKNOWN_OK: i32 = 1 << 13;

macro_rules! error_kinds {
    ($($name:ident => $message:literal),* $(,)?) => {
        /// libalpm's error numbers.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum ErrorKind { $($name,)* }
        impl ErrorKind {
            /// `alpm_strerror`.
            pub fn message(self) -> &'static str {
                match self { $(ErrorKind::$name => $message,)* }
            }
        }
    };
}

error_kinds! {
    System => "unexpected system error",
    BadPerms => "permission denied",
    NotAFile => "could not find or read file",
    NotADir => "could not find or read directory",
    WrongArgs => "wrong or NULL argument passed",
    DiskSpace => "not enough free disk space",
    HandleLock => "unable to lock database",
    DbOpen => "could not open database",
    DbNotFound => "could not find database",
    DbInvalidSig => "invalid or corrupted database (PGP signature)",
    DbWrite => "could not update database",
    DbRemove => "could not remove database entry",
    ServerBadUrl => "invalid url for server",
    ServerNone => "no servers configured for repository",
    TransNotNull => "transaction already initialized",
    TransNull => "transaction not initialized",
    TransDupTarget => "duplicate target",
    TransDupFilename => "duplicate filename",
    TransNotPrepared => "transaction not prepared",
    TransAbort => "transaction aborted",
    TransNotLocked => "transaction commit attempt when database is not locked",
    TransHookFailed => "failed to run transaction hooks",
    PkgNotFound => "could not find or read package",
    PkgIgnored => "operation cancelled due to ignorepkg",
    PkgInvalid => "invalid or corrupted package",
    PkgInvalidChecksum => "invalid or corrupted package (checksum)",
    PkgInvalidSig => "invalid or corrupted package (PGP signature)",
    PkgMissingSig => "package missing required signature",
    PkgOpen => "cannot open package file",
    PkgCantRemove => "cannot remove all files for package",
    PkgInvalidName => "package filename is not valid",
    PkgInvalidArch => "package architecture is not valid",
    SigMissing => "missing PGP signature",
    SigInvalid => "invalid PGP signature",
    UnsatisfiedDeps => "could not satisfy dependencies",
    ConflictingDeps => "conflicting dependencies",
    FileConflicts => "conflicting files",
    RetrievePrepare => "failed to initialize download",
    Retrieve => "failed to retrieve some files",
    InvalidRegex => "invalid regular expression",
    Libarchive => "libarchive error",
    Gpgme => "gpgme error",
}

/// A libalpm failure, with the details a front end explains for the kinds
/// that carry them.
#[derive(Clone, Debug)]
pub struct Error {
    pub kind: ErrorKind,
    pub missing: Vec<DepMissing>,
    pub conflicts: Vec<conflict::Conflict>,
    pub file_conflicts: Vec<conflict::FileConflict>,
    /// Packages that failed verification, for the integrity errors.
    pub names: Vec<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: ErrorKind) -> Error {
        Error { kind, missing: Vec::new(), conflicts: Vec::new(), file_conflicts: Vec::new(), names: Vec::new() }
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Error {
        Error::new(kind)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.message())
    }
}

/// `strerror` wording for an I/O error, as libalpm's messages quote it.
pub fn strerror(e: &std::io::Error) -> String {
    let text = e.to_string();
    match text.find(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// Shell-style matching, as `_alpm_fnmatch`.
pub fn fnmatch(pattern: &str, string: &str) -> bool {
    let (Ok(p), Ok(s)) = (std::ffi::CString::new(pattern), std::ffi::CString::new(string)) else { return false };
    unsafe { libc::fnmatch(p.as_ptr(), s.as_ptr(), 0) == 0 }
}

/// Ensure a directory path ends with a slash, as `canonicalize_path`.
fn canonicalize(path: &str) -> String {
    if path.ends_with('/') {
        path.to_string()
    } else {
        format!("{path}/")
    }
}

/// Options and databases: the state of one libalpm handle.
pub struct Handle {
    pub root: String,
    pub dbpath: String,
    pub lockfile: String,
    pub logfile: String,
    pub gpgdir: String,
    pub architectures: Vec<String>,
    pub siglevel: i32,
    pub local_file_siglevel: i32,
    pub remote_file_siglevel: i32,
    pub checkspace: bool,
    pub parallel_downloads: u32,
    pub sandboxuser: Option<String>,
    pub disable_sandbox_filesystem: bool,
    pub disable_sandbox_syscalls: bool,
    /// Failing download servers, skipped for the rest of the session.
    pub(crate) server_errors: dload::ServerErrors,
    /// Keys found in the keyring this session, as libalpm's `known_keys`.
    pub(crate) known_keys: std::sync::Mutex<Vec<String>>,
    pub cachedirs: Vec<String>,
    pub hookdirs: Vec<String>,
    pub ignorepkg: Vec<String>,
    pub ignoregroup: Vec<String>,
    pub noupgrade: Vec<String>,
    pub noextract: Vec<String>,
    /// Files an install may overwrite (`--overwrite`).
    pub overwrite: Vec<String>,
    /// Provisions treated as installed (`--assume-installed`).
    pub assumeinstalled: Vec<Depend>,
    pub local: Db,
    pub syncdbs: Vec<Db>,
    pub trans: Option<trans::Trans>,
    lockfd: Option<std::fs::File>,
}

impl Handle {
    /// `alpm_initialize`: the root and database directories must exist.
    pub fn new(root: &str, dbpath: &str) -> Result<Handle> {
        let real = |path: &str| -> Result<String> {
            let meta = std::fs::metadata(path).map_err(|_| ErrorKind::NotADir)?;
            if !meta.is_dir() {
                return Err(ErrorKind::NotADir.into());
            }
            let resolved = std::fs::canonicalize(path).map_err(|_| ErrorKind::NotADir)?;
            Ok(canonicalize(&resolved.to_string_lossy()))
        };
        let root = real(root)?;
        let dbpath = real(dbpath)?;
        Ok(Handle {
            hookdirs: vec![format!("{root}usr/share/libalpm/hooks/")],
            lockfile: format!("{dbpath}db.lck"),
            local: Db::local(&dbpath),
            root,
            dbpath,
            logfile: String::new(),
            gpgdir: String::new(),
            architectures: Vec::new(),
            siglevel: 0,
            local_file_siglevel: 0,
            remote_file_siglevel: 0,
            checkspace: false,
            parallel_downloads: 1,
            sandboxuser: None,
            disable_sandbox_filesystem: false,
            disable_sandbox_syscalls: false,
            server_errors: Default::default(),
            known_keys: Default::default(),
            cachedirs: Vec::new(),
            ignorepkg: Vec::new(),
            ignoregroup: Vec::new(),
            noupgrade: Vec::new(),
            noextract: Vec::new(),
            overwrite: Vec::new(),
            assumeinstalled: Vec::new(),
            syncdbs: Vec::new(),
            trans: None,
            lockfd: None,
        })
    }

    /// `sync_db_validate`: check a sync database's signature as its
    /// SigLevel asks, offering to import unknown keys and checking again
    /// after an import. A refused database is marked invalid; a missing one
    /// is reported.
    pub(crate) fn sync_db_validate(&mut self, index: usize) -> bool {
        let db = &self.syncdbs[index];
        let path = db.path.to_string_lossy().into_owned();
        if !db.path.exists() {
            log::event(&Event::DatabaseMissing);
            self.syncdbs[index].invalid = false;
            return true;
        }
        let level = db.siglevel;
        let mut valid = true;
        if level & SIG_DATABASE != 0 {
            let (optional, marginal, unknown) =
                (level & SIG_DATABASE_OPTIONAL != 0, level & SIG_DATABASE_MARGINAL_OK != 0, level & SIG_DATABASE_UNKNOWN_OK != 0);
            loop {
                let (ok, results) = self.check_pgp(&path, None, optional, marginal, unknown);
                if ok {
                    break;
                }
                if !self.process_siglist(&self.syncdbs[index].name, &results, optional, marginal, unknown) {
                    valid = false;
                    break;
                }
            }
        }
        self.syncdbs[index].invalid = !valid;
        valid
    }

    pub fn add_hookdir(&mut self, dir: &str) {
        self.hookdirs.push(canonicalize(dir));
    }

    pub fn add_cachedir(&mut self, dir: &str) {
        self.cachedirs.push(canonicalize(dir));
    }

    pub fn set_gpgdir(&mut self, dir: &str) {
        self.gpgdir = canonicalize(dir);
    }

    /// `alpm_register_syncdb`.
    pub fn register_syncdb(&mut self, name: &str, siglevel: i32) -> Result<&mut Db> {
        if name == "local" || self.syncdbs.iter().any(|d| d.name == name) {
            return Err(ErrorKind::DbOpen.into());
        }
        let db = Db::sync(&self.dbpath, name, siglevel);
        self.syncdbs.push(db);
        let index = self.syncdbs.len() - 1;
        self.sync_db_validate(index);
        Ok(self.syncdbs.last_mut().unwrap())
    }

    /// `alpm_pkg_should_ignore`: IgnorePkg and IgnoreGroup.
    pub fn should_ignore(&self, pkg: &Pkg) -> bool {
        self.ignorepkg.iter().any(|p| fnmatch(p, &pkg.name))
            || pkg.info().groups.iter().any(|g| self.ignoregroup.iter().any(|p| fnmatch(p, g)))
    }

    /// `_alpm_filecache_find`: the first regular file of this name in a cache
    /// directory.
    pub fn filecache_find(&self, filename: &str) -> Option<String> {
        for dir in &self.cachedirs {
            let path = format!("{dir}{filename}");
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => return Some(path),
                Ok(meta) => {
                    use std::os::unix::fs::MetadataExt;
                    log::warning(&format!("cached pkg '{path}' is not a regular file: mode={}\n", meta.mode()));
                }
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    log::warning(&format!("could not open '{path}'\n: {}", strerror(&e)));
                }
                Err(_) => {}
            }
        }
        None
    }

    /// `alpm_pkg_download_size`: what fetching a sync package would still
    /// transfer, given the package cache and any partial download.
    pub fn download_size(&self, pkg: &Pkg) -> i64 {
        if !matches!(pkg.origin, package::Origin::Sync { .. }) {
            return 0;
        }
        let filename = &pkg.info().filename;
        if self.filecache_find(filename).is_some() {
            return 0;
        }
        match self.filecache_find(&format!("{filename}.part")) {
            Some(part) => std::fs::metadata(part).map_or(0, |meta| (pkg.info().size - meta.len() as i64).max(0)),
            None => pkg.info().size,
        }
    }

    /// `alpm_sync_get_new_version`: the first sync package of this name, if
    /// it is newer.
    pub fn sync_new_version(&self, pkg: &Pkg) -> Option<&Arc<Pkg>> {
        let spkg = self.syncdbs.iter().find_map(|db| db.get(&pkg.name))?;
        (vercmp(&spkg.version, &pkg.version) > 0).then_some(spkg)
    }

    /// `alpm_pkg_compute_requiredby` (or `optionalfor` when `optional`).
    pub fn compute_requiredby(&self, pkg: &Pkg, optional: bool) -> Vec<String> {
        let mut reqs: Vec<String> = Vec::new();
        let mut scan = |db: &Db| {
            for cachepkg in db.pkgs() {
                let info = cachepkg.info();
                let list = if optional { &info.optdepends } else { &info.depends };
                if list.iter().any(|d| d.satisfied_by(pkg)) && !reqs.contains(&cachepkg.name) {
                    reqs.push(cachepkg.name.clone());
                }
            }
        };
        match &pkg.origin {
            package::Origin::Sync { .. } => {
                self.syncdbs.iter().for_each(&mut scan);
                reqs.sort();
            }
            _ => scan(&self.local),
        }
        reqs
    }

    /// `alpm_find_dbs_satisfier` over the sync databases, asking the front end
    /// about ignored packages and between several providers.
    pub fn find_dbs_satisfier(&self, depstring: &str) -> Option<Arc<Pkg>> {
        let dep = Depend::parse(depstring);
        self.resolvedep(&dep, &self.syncdbs.iter().collect::<Vec<_>>(), &[], true).ok()
    }

    /// `resolvedep`: a literal match first, then providers.
    pub fn resolvedep(&self, dep: &Depend, dbs: &[&Db], excluding: &[Arc<Pkg>], prompt: bool) -> Result<Arc<Pkg>> {
        let mut ignored = false;
        let usable = |db: &&&Db| db.usage & (db::USAGE_INSTALL | db::USAGE_UPGRADE) != 0;
        let excluded = |p: &Pkg| excluding.iter().any(|e| e.name == p.name);
        let allow = |pkg: &Pkg, ignored: &mut bool| -> bool {
            if !self.should_ignore(pkg) {
                return true;
            }
            let mut install = false;
            if prompt {
                let mut q = Question::InstallIgnorePkg { pkg, install: false };
                log::question(&mut q);
                if let Question::InstallIgnorePkg { install: answer, .. } = q {
                    install = answer;
                }
            } else {
                log::warning(&format!("ignoring package {}-{}\n", pkg.name, pkg.version));
            }
            if !install {
                *ignored = true;
            }
            install
        };
        for db in dbs.iter().filter(usable) {
            if let Some(pkg) = db.get(&dep.name) {
                if dep.literal(pkg) && !excluded(pkg) && allow(pkg, &mut ignored) {
                    return Ok(pkg.clone());
                }
            }
        }
        let mut providers: Vec<Arc<Pkg>> = Vec::new();
        for db in dbs.iter().filter(usable) {
            for pkg in db.pkgs() {
                if pkg.name != dep.name
                    && dep.provided_by(&pkg.info().provides)
                    && !excluded(pkg)
                    && allow(pkg, &mut ignored)
                {
                    // An installed provider is used without asking.
                    if self.local.get(&pkg.name).is_some() {
                        return Ok(pkg.clone());
                    }
                    providers.push(pkg.clone());
                }
            }
        }
        if !providers.is_empty() {
            let mut index = 0;
            if providers.len() > 1 {
                let mut q = Question::SelectProvider { depend: dep, providers: &providers, use_index: 0 };
                log::question(&mut q);
                if let Question::SelectProvider { use_index, .. } = q {
                    index = use_index;
                }
            }
            if index >= 0 && (index as usize) < providers.len() {
                return Ok(providers[index as usize].clone());
            }
        }
        Err(if ignored { ErrorKind::PkgIgnored } else { ErrorKind::PkgNotFound }.into())
    }
}
