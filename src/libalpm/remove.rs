//! Removing packages, as libalpm's remove.c.
use super::deps::{self, DepMissing};
use super::log::{self, Event, Operation, Progress};
use super::package::Pkg;
use super::trans::*;
use super::{util, Error, ErrorKind, Handle, Result};
use std::sync::Arc;

impl Handle {
    fn local_checkdeps(&self, remove: &[Arc<Pkg>]) -> Vec<DepMissing> {
        let nodepversion = self.trans().has(FLAG_NODEPVERSION);
        deps::checkdeps_with(self.local.pkgs(), remove, &[], true, &self.assumeinstalled, nodepversion)
    }

    /// `_alpm_remove_prepare`: pull in or drop packages as the flags say, and
    /// refuse removals that break installed packages.
    pub(crate) fn remove_prepare(&mut self) -> Result<()> {
        let flags = self.trans().flags;
        if flags & FLAG_RECURSE != 0 && flags & FLAG_CASCADE == 0 {
            log::debug("finding removable dependencies\n");
            let mut remove = std::mem::take(&mut self.trans_mut().remove);
            deps::recursedeps(self.local.pkgs(), &mut remove, flags & FLAG_RECURSEALL != 0);
            self.trans_mut().remove = remove;
        }
        if flags & FLAG_NODEPS == 0 {
            log::event(&Event::CheckdepsStart);
            log::debug("looking for unsatisfied dependencies\n");
            let mut lp = self.local_checkdeps(&self.trans().remove);
            if !lp.is_empty() {
                if flags & FLAG_CASCADE != 0 {
                    // Remove everything that depends on a target, too.
                    while !lp.is_empty() {
                        for miss in &lp {
                            match self.local.get(&miss.target).cloned() {
                                Some(info) => {
                                    if !self.trans().remove.iter().any(|p| p.name == info.name) {
                                        log::debug(&format!("pulling {} in target list\n", info.name));
                                        self.trans_mut().remove.push(info);
                                    }
                                }
                                None => {
                                    log::error(&format!("could not find {} in database -- skipping\n", miss.target))
                                }
                            }
                        }
                        lp = self.local_checkdeps(&self.trans().remove);
                    }
                } else if flags & FLAG_UNNEEDED != 0 {
                    // Keep targets that installed packages still need.
                    while !lp.is_empty() {
                        for miss in &lp {
                            let Some(causing) = &miss.causing else { continue };
                            let remove = &mut self.trans_mut().remove;
                            if let Some(at) = remove.iter().position(|p| &p.name == causing) {
                                let pkg = remove.remove(at);
                                log::warning(&format!("removing {} from target list\n", pkg.name));
                            }
                        }
                        lp = self.local_checkdeps(&self.trans().remove);
                    }
                } else {
                    let mut e = Error::new(ErrorKind::UnsatisfiedDeps);
                    e.missing = lp;
                    return Err(e);
                }
            }
        }
        // -Rcs is -Rc, then -Rs.
        if flags & FLAG_CASCADE != 0 && flags & FLAG_RECURSE != 0 {
            log::debug("finding removable dependencies\n");
            let mut remove = std::mem::take(&mut self.trans_mut().remove);
            deps::recursedeps(self.local.pkgs(), &mut remove, flags & FLAG_RECURSEALL != 0);
            self.trans_mut().remove = remove;
        }
        if flags & FLAG_NODEPS == 0 {
            self.notify_needed_optdepends();
            log::event(&Event::CheckdepsDone);
        }
        Ok(())
    }

    /// `remove_notify_needed_optdepends`.
    fn notify_needed_optdepends(&self) {
        let remove = &self.trans().remove;
        for pkg in self.local.pkgs() {
            if remove.iter().any(|p| p.name == pkg.name) {
                continue;
            }
            for optdep in &pkg.info().optdepends {
                // The description is part of the string libalpm matches.
                let dep = deps::Depend::parse(&optdep.to_string());
                if deps::find_satisfier(remove, &dep).is_some() {
                    log::event(&Event::OptdepRemoval { pkg, optdep });
                }
            }
        }
    }

    /// `can_remove_file`: false only when an existing file sits on a
    /// read-only file system or similar.
    fn can_remove_file(&self, name: &str) -> bool {
        let filepath = format!("{}{}", self.root, name);
        if name.ends_with('/') && dir_is_mountpoint(&filepath, None) {
            return true;
        }
        if let Err(errno) = util::access(&filepath, libc::W_OK) {
            if errno != libc::EACCES && errno != libc::ETXTBSY && util::access(&filepath, libc::F_OK).is_ok() {
                log::error(&format!("cannot remove file '{filepath}': {}\n", util::errno_str(errno)));
                return false;
            }
        }
        true
    }

    /// `unlink_file`: remove one file of `oldpkg`, keeping modified
    /// configuration as .pacsave. Ok(false) when it was skipped or missing.
    fn unlink_file(&self, oldpkg: &Pkg, newpkg: Option<&Pkg>, name: &str, nosave: bool) -> std::result::Result<bool, ()> {
        let mut file = format!("{}{}", self.root, name);
        if file.ends_with('/') {
            // A directory replaced by a symlink must not be followed.
            file.pop();
        }
        let Ok(meta) = std::fs::symlink_metadata(&file) else {
            log::debug(&format!("file {file} does not exist\n"));
            return Ok(false);
        };
        if meta.is_dir() {
            file.push('/');
            match util::files_in_directory(&file) {
                Some(n) if n > 0 => log::debug(&format!("keeping directory {file} (contains files)\n")),
                None => log::debug(&format!("keeping directory {file} (could not count files)\n")),
                _ if newpkg.is_some_and(|n| n.files().contains(name).is_some()) => {
                    log::debug(&format!("keeping directory {file} (in new package)\n"))
                }
                _ if dir_is_mountpoint(&file, Some(&meta)) => {
                    log::debug(&format!("keeping directory {file} (mountpoint)\n"))
                }
                _ => {
                    let owner = self
                        .local
                        .pkgs()
                        .iter()
                        .find(|p| p.name != oldpkg.name && p.files().contains(name).is_some());
                    match owner {
                        Some(owner) => log::debug(&format!("keeping directory {file} (owned by {})\n", owner.name)),
                        None => match std::fs::remove_dir(&file) {
                            Ok(()) => log::debug(&format!("removed directory {file} (no remaining owners)\n")),
                            Err(e) => {
                                log::debug(&format!("directory removal of {file} failed: {}\n", super::strerror(&e)));
                                return Err(());
                            }
                        },
                    }
                }
            }
            return Ok(true);
        }
        if let Some(backup) = oldpkg.files().backup.iter().find(|b| b.name == name) {
            if nosave {
                log::debug(&format!("transaction is set to NOSAVE, not backing up '{file}'\n"));
            } else if util::md5_file(&file).is_some_and(|hash| hash != backup.hash) {
                shift_pacsave(&file);
                let newpath = format!("{file}.pacsave");
                if let Err(e) = std::fs::rename(&file, &newpath) {
                    let msg = format!("could not rename {file} to {newpath} ({})\n", super::strerror(&e));
                    log::error(&msg);
                    self.logaction("ALPM", &format!("error: {msg}"));
                    return Err(());
                }
                log::event(&Event::PacsaveCreated { file: &file });
                self.logaction("ALPM", &format!("warning: {file} saved as {newpath}\n"));
                return Ok(true);
            }
        }
        log::debug(&format!("unlinking {file}\n"));
        if let Err(e) = std::fs::remove_file(&file) {
            let msg = format!("cannot remove {file} ({})\n", super::strerror(&e));
            log::error(&msg);
            self.logaction("ALPM", &format!("error: {msg}"));
            return Err(());
        }
        Ok(true)
    }

    /// `should_skip_file`: NoUpgrade files, files moving to another package,
    /// and configuration the replacing package also ships.
    fn should_skip_file(&self, newpkg: Option<&Pkg>, path: &str) -> bool {
        util::fnmatch_patterns(&self.noupgrade, path) == Some(true)
            || self.trans().skip_remove.iter().any(|s| s == path)
            || newpkg.is_some_and(|n| {
                n.files().backup.iter().any(|b| b.name == path) && n.files().contains(path).is_some()
            })
    }

    /// `remove_package_files`: the number of files that could not be removed.
    fn remove_package_files(&self, oldpkg: &Pkg, newpkg: Option<&Pkg>, targ_count: usize, pkg_count: usize) -> Result<usize> {
        let nosave = self.trans().has(FLAG_NOSAVE);
        let files = &oldpkg.files().files;
        for file in files {
            if !self.should_skip_file(newpkg, &file.name) && !self.can_remove_file(&file.name) {
                log::debug(&format!("not removing package '{}', can't remove all files\n", oldpkg.name));
                return Err(ErrorKind::PkgCantRemove.into());
            }
        }
        log::debug(&format!("removing {} files\n", files.len()));
        let progress = |percent: usize| {
            if newpkg.is_none() {
                log::progress(Progress::RemoveStart, &oldpkg.name, percent as i32, pkg_count, targ_count);
            }
        };
        progress(0);
        let mut errors = 0;
        // Backwards, so directories empty before they are reached.
        for (done, file) in files.iter().rev().enumerate() {
            if self.should_skip_file(newpkg, &file.name) {
                log::debug(&format!("{} is in skip_remove, skipping removal\n", file.name));
                continue;
            }
            if self.unlink_file(oldpkg, newpkg, &file.name, nosave).is_err() {
                errors += 1;
            }
            progress(done * 100 / files.len());
        }
        progress(100);
        Ok(errors)
    }

    /// `_alpm_remove_single_package`: remove `oldpkg`, or with `newpkg`, the
    /// old files of an upgrade.
    pub(crate) fn remove_single_package(&mut self, oldpkg: &Arc<Pkg>, newpkg: Option<&Pkg>, targ_count: usize, pkg_count: usize) {
        let scriptlets = oldpkg.info().has_scriptlet && !self.trans().has(FLAG_NOSCRIPTLET);
        let operation = Event::PackageOperationStart { operation: Operation::Remove, oldpkg: Some(oldpkg), newpkg: None };
        if newpkg.is_some() {
            log::debug(&format!("removing old package first ({}-{})\n", oldpkg.name, oldpkg.version));
        } else {
            log::event(&operation);
            log::debug(&format!("removing package {}-{}\n", oldpkg.name, oldpkg.version));
            if scriptlets {
                let script = self.local_pkgpath(oldpkg, "install");
                self.runscriptlet(&script, "pre_remove", &oldpkg.version, None, false);
            }
        }
        if !self.trans().has(FLAG_DBONLY) {
            let _ = self.remove_package_files(oldpkg, newpkg, targ_count, pkg_count);
        }
        if newpkg.is_none() {
            self.logaction("ALPM", &format!("removed {} ({})\n", oldpkg.name, oldpkg.version));
            if scriptlets {
                let script = self.local_pkgpath(oldpkg, "install");
                self.runscriptlet(&script, "post_remove", &oldpkg.version, None, false);
            }
            log::event(&Event::PackageOperationDone { operation: Operation::Remove, oldpkg: Some(oldpkg), newpkg: None });
        }
        log::debug(&format!("removing database entry '{}'\n", oldpkg.name));
        if self.local_db_remove(oldpkg).is_err() {
            log::error(&format!("could not remove database entry {}-{}\n", oldpkg.name, oldpkg.version));
        }
        self.local.cache_remove(&oldpkg.name);
    }

    /// `_alpm_remove_packages`.
    pub(crate) fn remove_packages(&mut self, run_ldconfig: bool) -> Result<()> {
        let remove = self.trans().remove.clone();
        for (i, pkg) in remove.iter().enumerate() {
            if interrupted() {
                return Ok(());
            }
            self.remove_single_package(pkg, None, i + 1, remove.len());
        }
        if run_ldconfig {
            self.ldconfig();
        }
        Ok(())
    }
}

/// `dir_is_mountpoint`: `directory` ends with a slash.
fn dir_is_mountpoint(directory: &str, meta: Option<&std::fs::Metadata>) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev = match meta {
        Some(meta) => meta.dev(),
        None => match std::fs::metadata(directory) {
            Ok(meta) => meta.dev(),
            Err(_) => return false,
        },
    };
    match std::fs::metadata(format!("{directory}..")) {
        Ok(parent) => parent.dev() != dev,
        Err(_) => false,
    }
}

/// `shift_pacsave`: file.pacsave becomes file.pacsave.1, and older numbered
/// copies move up one.
fn shift_pacsave(file: &str) {
    let path = std::path::Path::new(file);
    let (Some(dir), Some(base)) = (path.parent(), path.file_name().and_then(|b| b.to_str())) else { return };
    let prefix = format!("{base}.pacsave.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::error(&format!("could not open directory: {}: \n", dir.display()));
        return;
    };
    let mut max = 0u64;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(&prefix)) else { continue };
        if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
            max = max.max(rest.parse().unwrap_or(0));
        }
    }
    for i in (2..=max + 1).rev() {
        let _ = std::fs::rename(format!("{file}.pacsave.{}", i - 1), format!("{file}.pacsave.{i}"));
    }
    let old = format!("{file}.pacsave");
    if std::path::Path::new(&old).exists() {
        let _ = std::fs::rename(&old, format!("{old}.1"));
    }
}
