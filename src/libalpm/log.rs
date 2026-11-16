//! The front end libalpm reports to: log messages, events, questions and
//! progress. One process has one front end, so it is global.
use super::deps::Depend;
use super::package::Pkg;
use std::sync::{Arc, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Debug,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Install,
    Upgrade,
    Reinstall,
    Downgrade,
    Remove,
}

/// Something that happened during a transaction that the front end may show.
#[derive(Debug)]
pub enum Event<'a> {
    CheckdepsStart,
    CheckdepsDone,
    ResolvedepsStart,
    ResolvedepsDone,
    InterconflictsStart,
    InterconflictsDone,
    TransactionStart,
    TransactionDone,
    PackageOperationStart { operation: Operation, oldpkg: Option<&'a Pkg>, newpkg: Option<&'a Pkg> },
    PackageOperationDone { operation: Operation, oldpkg: Option<&'a Pkg>, newpkg: Option<&'a Pkg> },
    IntegrityStart,
    IntegrityDone,
    LoadStart,
    LoadDone,
    FileconflictsStart,
    FileconflictsDone,
    DiskspaceStart,
    DiskspaceDone,
    OptdepRemoval { pkg: &'a Pkg, optdep: &'a Depend },
    DatabaseMissing { name: &'a str },
    KeyringStart,
    KeyringDone,
    KeyDownloadStart,
    KeyDownloadDone,
    RetrieveStart,
    RetrieveDone,
    RetrieveFailed,
    PkgRetrieveDone,
    PkgRetrieveFailed,
    HookStart { post: bool },
    HookDone { post: bool },
    HookRunDone { name: &'a str, desc: &'a str, position: usize, total: usize },
    ScriptletInfo { line: &'a str },
    HookRunStart { name: &'a str, desc: &'a str, position: usize, total: usize },
    PacnewCreated { file: &'a str },
    PacsaveCreated { file: &'a str },
    PkgRetrieveStart { num: usize, total_size: i64 },
}

/// A decision libalpm needs. Every answer starts as "no".
#[derive(Debug)]
pub enum Question<'a> {
    InstallIgnorePkg { pkg: &'a Pkg, install: bool },
    Replace { old: &'a Pkg, new: &'a Pkg, db: &'a str, replace: bool },
    Conflict { package1: &'a str, package2: &'a str, reason: &'a Depend, remove: bool },
    SelectProvider { depend: &'a Depend, providers: &'a [Arc<Pkg>], use_index: i32 },
    ImportKey { fingerprint: &'a str, uid: &'a str, import: bool },
    CorruptedPkg { filepath: &'a str, remove: bool },
    RemovePkgs { packages: &'a [Arc<Pkg>], skip: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Progress {
    AddStart,
    UpgradeStart,
    DowngradeStart,
    ReinstallStart,
    RemoveStart,
    ConflictsStart,
    DiskspaceStart,
    IntegrityStart,
    LoadStart,
    KeyringStart,
}

pub trait Frontend: Send + Sync {
    fn log(&self, _level: Level, _message: &str) {}
    fn event(&self, _event: &Event) {}
    fn question(&self, _question: &mut Question) {}
    fn progress(&self, _kind: Progress, _name: &str, _percent: i32, _total: usize, _current: usize) {}
}

static FRONTEND: RwLock<Option<Arc<dyn Frontend>>> = RwLock::new(None);

pub fn set_frontend(frontend: Arc<dyn Frontend>) {
    *FRONTEND.write().unwrap_or_else(|e| e.into_inner()) = Some(frontend);
}

fn with(f: impl FnOnce(&dyn Frontend)) {
    let guard = FRONTEND.read().unwrap_or_else(|e| e.into_inner());
    if let Some(frontend) = guard.as_ref() {
        f(frontend.as_ref());
    }
}

pub fn error(message: &str) {
    with(|f| f.log(Level::Error, message));
}

pub fn debug(message: &str) {
    with(|f| f.log(Level::Debug, message));
}

pub fn warning(message: &str) {
    with(|f| f.log(Level::Warning, message));
}

pub fn event(e: &Event) {
    with(|f| f.event(e));
}

pub fn question(q: &mut Question) {
    with(|f| f.question(q));
}

pub fn progress(kind: Progress, name: &str, percent: i32, total: usize, current: usize) {
    with(|f| f.progress(kind, name, percent, total, current));
}
