//! Transactions, as libalpm's trans.c: one lock, a set of packages to add
//! and remove, a prepare step that resolves and checks them, and a commit.
use super::deps;
use super::log::{self, Event};
use super::package::{Pkg, Reason};
use super::{ErrorKind, Handle, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const FLAG_NODEPS: u32 = 1;
pub const FLAG_NOSAVE: u32 = 1 << 2;
pub const FLAG_NODEPVERSION: u32 = 1 << 3;
pub const FLAG_CASCADE: u32 = 1 << 4;
pub const FLAG_RECURSE: u32 = 1 << 5;
pub const FLAG_DBONLY: u32 = 1 << 6;
pub const FLAG_NOHOOKS: u32 = 1 << 7;
pub const FLAG_ALLDEPS: u32 = 1 << 8;
pub const FLAG_DOWNLOADONLY: u32 = 1 << 9;
pub const FLAG_NOSCRIPTLET: u32 = 1 << 10;
pub const FLAG_NOCONFLICTS: u32 = 1 << 11;
pub const FLAG_NEEDED: u32 = 1 << 13;
pub const FLAG_ALLEXPLICIT: u32 = 1 << 14;
pub const FLAG_UNNEEDED: u32 = 1 << 15;
pub const FLAG_RECURSEALL: u32 = 1 << 16;
pub const FLAG_NOLOCK: u32 = 1 << 17;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Initialized,
    Prepared,
    Committing,
    Committed,
}

/// A package to install, with what libalpm keeps on its transaction copy.
#[derive(Clone, Debug)]
pub struct AddPkg {
    pub pkg: Arc<Pkg>,
    /// The reason it will be installed with.
    pub reason: Reason,
    /// Installed packages this one replaces or conflicts out of the way.
    pub removes: Vec<Arc<Pkg>>,
    /// The archive to extract: the package itself for `-U`, the cached
    /// download for sync packages once `sync_load` has found it.
    pub file: Option<PathBuf>,
    /// The installed version this one upgrades, set by `sync_prepare`.
    pub old: Option<Arc<Pkg>>,
    /// How the archive was verified, for the local database.
    pub validation: u32,
}

impl AddPkg {
    pub fn new(pkg: Arc<Pkg>) -> AddPkg {
        let reason = pkg.info().reason;
        let file = pkg.path().map(PathBuf::from);
        AddPkg { pkg, reason, removes: Vec::new(), file, old: None, validation: 0 }
    }
}

pub struct Trans {
    pub flags: u32,
    pub state: State,
    pub add: Vec<AddPkg>,
    pub remove: Vec<Arc<Pkg>>,
    /// Sync targets that could not be resolved and were skipped.
    pub unresolvable: Vec<Arc<Pkg>>,
    /// Files an upgrade moves between packages: never removed with the old one.
    pub skip_remove: Vec<String>,
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// `alpm_trans_interrupt`: stop the commit between packages. Async-signal-safe.
pub fn interrupt() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

pub(crate) fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

impl Trans {
    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }
}

impl Handle {
    pub fn trans(&self) -> &Trans {
        self.trans.as_ref().expect("transaction not initialized")
    }

    pub(crate) fn trans_mut(&mut self) -> &mut Trans {
        self.trans.as_mut().expect("transaction not initialized")
    }

    /// `alpm_trans_init`: take the database lock and start a transaction.
    pub fn trans_init(&mut self, flags: u32) -> Result<()> {
        if self.trans.is_some() {
            return Err(ErrorKind::TransNotNull.into());
        }
        if flags & FLAG_NOLOCK == 0 {
            self.lock()?;
        }
        INTERRUPTED.store(false, Ordering::SeqCst);
        self.trans = Some(Trans {
            flags,
            state: State::Initialized,
            add: Vec::new(),
            remove: Vec::new(),
            unresolvable: Vec::new(),
            skip_remove: Vec::new(),
        });
        Ok(())
    }

    /// `alpm_trans_release`: end the transaction and release the lock.
    pub fn trans_release(&mut self) {
        if let Some(trans) = self.trans.take() {
            if !trans.has(FLAG_NOLOCK) {
                self.unlock();
            }
        }
    }

    /// `_alpm_handle_lock`: create the lock file, failing if it exists.
    pub(crate) fn lock(&mut self) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = std::path::Path::new(&self.lockfile).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o000)
            .open(&self.lockfile)
            .map_err(|_| ErrorKind::HandleLock)?;
        self.lockfd = Some(file);
        Ok(())
    }

    /// `alpm_unlock`.
    pub(crate) fn unlock(&mut self) {
        if self.lockfd.take().is_some() {
            let _ = std::fs::remove_file(&self.lockfile);
        }
    }

    /// `alpm_remove_pkg`: queue an installed package for removal.
    pub fn remove_pkg(&mut self, pkg: &Arc<Pkg>) -> Result<()> {
        let trans = self.trans.as_mut().ok_or(ErrorKind::TransNull)?;
        if trans.state != State::Initialized || !pkg.is_local() {
            return Err(ErrorKind::WrongArgs.into());
        }
        if trans.remove.iter().any(|p| p.name == pkg.name) {
            log::debug(&format!("skipping duplicate target: {}\n", pkg.name));
            return Ok(());
        }
        log::debug(&format!("adding package {} to the transaction remove list\n", pkg.name));
        trans.remove.push(pkg.clone());
        Ok(())
    }

    /// `alpm_trans_prepare`: resolve and check the transaction. Failures
    /// carry what blocked them.
    pub fn trans_prepare(&mut self) -> Result<()> {
        let trans = self.trans.as_ref().ok_or(ErrorKind::TransNull)?;
        if trans.state != State::Initialized {
            return Err(ErrorKind::TransNotPrepared.into());
        }
        if trans.add.is_empty() && trans.remove.is_empty() {
            return Ok(());
        }
        let invalid = self.check_arch();
        if !invalid.is_empty() {
            let mut e = super::Error::new(ErrorKind::PkgInvalidArch);
            e.names = invalid;
            return Err(e);
        }
        if self.trans().add.is_empty() {
            self.remove_prepare()?;
        } else {
            self.sync_prepare()?;
        }
        if !self.trans().has(FLAG_NODEPS) {
            log::debug("sorting by dependencies\n");
            let trans = self.trans.as_ref().unwrap();
            let add: Vec<Arc<Pkg>> = trans.add.iter().map(|a| a.pkg.clone()).collect();
            let sorted_add = deps::sortbydeps(self.local.pkgs(), &add, &trans.remove, false);
            let sorted_remove = deps::sortbydeps(self.local.pkgs(), &trans.remove, &[], true);
            let trans = self.trans.as_mut().unwrap();
            let mut add = std::mem::take(&mut trans.add);
            trans.add = sorted_add
                .iter()
                .map(|p| {
                    let at = add.iter().position(|a| Arc::ptr_eq(&a.pkg, p)).unwrap();
                    add.swap_remove(at)
                })
                .collect();
            trans.remove = sorted_remove;
        }
        self.trans_mut().state = State::Prepared;
        Ok(())
    }

    /// `check_arch`: packages built for an architecture not configured.
    fn check_arch(&self) -> Vec<String> {
        if self.architectures.is_empty() {
            log::debug("skipping architecture checks\n");
            return Vec::new();
        }
        self.trans()
            .add
            .iter()
            .filter_map(|a| {
                let arch = &a.pkg.info().arch;
                (!arch.is_empty() && arch != "any" && !self.architectures.contains(arch))
                    .then(|| format!("{}-{}-{}", a.pkg.name, a.pkg.version, arch))
            })
            .collect()
    }

    /// `alpm_trans_commit`: apply the prepared transaction.
    pub fn trans_commit(&mut self) -> Result<()> {
        let trans = self.trans.as_ref().ok_or(ErrorKind::TransNull)?;
        if trans.state != State::Prepared {
            return Err(ErrorKind::TransNotPrepared.into());
        }
        if trans.has(FLAG_NOLOCK) {
            return Err(ErrorKind::TransNotLocked.into());
        }
        if trans.add.is_empty() && trans.remove.is_empty() {
            return Ok(());
        }
        if !trans.add.is_empty() {
            self.sync_load()?;
            if self.trans().has(FLAG_DOWNLOADONLY) {
                return Ok(());
            }
            self.sync_check()?;
        }
        let hooks = !self.trans().has(FLAG_NOHOOKS);
        if hooks && self.hook_run(super::hook::When::PreTransaction).is_err() {
            return Err(ErrorKind::TransHookFailed.into());
        }
        self.trans_mut().state = State::Committing;
        self.logaction("ALPM", "transaction started\n");
        log::event(&Event::TransactionStart);
        let result = if self.trans().add.is_empty() { self.remove_packages(true) } else { self.sync_commit() };
        if let Err(e) = result {
            self.logaction("ALPM", "transaction failed\n");
            return Err(e);
        }
        if interrupted() {
            self.logaction("ALPM", "transaction interrupted\n");
        } else {
            log::event(&Event::TransactionDone);
            self.logaction("ALPM", "transaction completed\n");
            if hooks {
                let _ = self.hook_run(super::hook::When::PostTransaction);
            }
        }
        self.trans_mut().state = State::Committed;
        Ok(())
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.trans_release();
    }
}
