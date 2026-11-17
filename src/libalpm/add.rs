//! Installing package archives, as libalpm's add.c: queueing targets, then
//! extracting each one over the root with the backup-file rules, and
//! recording it in the local database.
use super::conflict::{llstat, needbackup};
use super::log::{self, Event, Progress};
use super::package::{Files, Pkg, Reason};
use super::trans::*;
use super::{util, ErrorKind, Handle, Result};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// What installing a package does to the installed version, for pacman.log.
enum Operation {
    Install,
    Upgrade,
    Reinstall,
    Downgrade,
}

type Entry<'a> = tar::Entry<'a, Box<dyn Read>>;

/// The entry's mode with its file type, as `archive_entry_mode`.
fn entry_mode(entry: &Entry) -> u32 {
    let header = entry.header();
    let kind = header.entry_type();
    let file_type = if kind.is_dir() {
        libc::S_IFDIR
    } else if kind.is_symlink() {
        libc::S_IFLNK
    } else {
        libc::S_IFREG
    };
    file_type | (header.mode().unwrap_or(0) & 0o7777)
}

fn is_dir_mode(mode: u32) -> bool {
    mode & libc::S_IFMT == libc::S_IFDIR
}

/// `perform_extraction`: write the entry to `filename`, replacing whatever
/// non-directory is there, with its owner, mode, times and xattrs.
fn perform_extraction(handle: &Handle, entry: &mut Entry, filename: &str) -> bool {
    // As ARCHIVE_EXTRACT_UNLINK; a symlink is replaced, never followed.
    if llstat(filename).is_some_and(|m| !m.is_dir()) {
        let _ = std::fs::remove_file(filename.trim_end_matches('/'));
    }
    if let Err(e) = entry.unpack(filename) {
        let why = super::strerror(&e);
        log::error(&format!("could not extract {filename} ({why})\n"));
        handle.logaction("ALPM", &format!("error: could not extract {filename} ({why})\n"));
        return false;
    }
    true
}

fn try_rename(handle: &Handle, src: &str, dest: &str) -> bool {
    match std::fs::rename(src, dest) {
        Ok(()) => true,
        Err(e) => {
            let why = super::strerror(&e);
            log::error(&format!("could not rename {src} to {dest} ({why})\n"));
            handle.logaction("ALPM", &format!("error: could not rename {src} to {dest} ({why})\n"));
            false
        }
    }
}

impl Handle {
    /// `alpm_add_pkg`: queue an archive or sync package for installation.
    pub fn add_pkg(&mut self, pkg: Arc<Pkg>) -> Result<()> {
        if pkg.is_local() {
            return Err(ErrorKind::WrongArgs.into());
        }
        let trans = self.trans.as_ref().ok_or(ErrorKind::TransNull)?;
        if trans.state != State::Initialized {
            return Err(ErrorKind::TransNull.into());
        }
        log::debug(&format!("adding package '{}'\n", pkg.name));
        if let Some(dup) = trans.add.iter().find(|a| a.pkg.name == pkg.name) {
            if Arc::ptr_eq(&dup.pkg, &pkg) {
                log::debug(&format!("skipping duplicate target: {}\n", pkg.name));
                return Ok(());
            }
            return Err(ErrorKind::TransDupTarget.into());
        }
        if let Some(local) = self.local.get(&pkg.name) {
            let cmp = super::vercmp(&pkg.version, &local.version);
            if cmp == 0 {
                if trans.has(FLAG_NEEDED) {
                    log::warning(&format!("{}-{} is up to date -- skipping\n", local.name, local.version));
                    return Ok(());
                } else if !trans.has(FLAG_DOWNLOADONLY) {
                    log::warning(&format!("{}-{} is up to date -- reinstalling\n", local.name, local.version));
                }
            } else if cmp < 0 && !trans.has(FLAG_DOWNLOADONLY) {
                log::warning(&format!("downgrading package {} ({} => {})\n", local.name, local.version, pkg.version));
            }
        }
        log::debug(&format!("adding package {}-{} to the transaction add list\n", pkg.name, pkg.version));
        let mut add = AddPkg::new(pkg);
        add.reason = Reason::Explicit;
        self.trans_mut().add.push(add);
        Ok(())
    }

    /// `extract_db_file`: `.INSTALL`, `.CHANGELOG` and `.MTREE` go into the
    /// package's database entry; other metadata is skipped.
    fn extract_db_file(&self, entry: &mut Entry, newpkg: &Pkg, entryname: &str) -> usize {
        let dbfile = match entryname {
            ".INSTALL" => "install",
            ".CHANGELOG" => "changelog",
            ".MTREE" => "mtree",
            _ => {
                log::debug(&format!("skipping extraction of '{entryname}'\n"));
                return 0;
            }
        };
        let filename = self.local_pkgpath(newpkg, dbfile);
        if !perform_extraction(self, entry, &filename) {
            return 1;
        }
        let _ = std::fs::set_permissions(&filename, std::fs::Permissions::from_mode(0o644));
        0
    }

    /// `extract_single_file`: install one entry, keeping NoUpgrade files and
    /// modified backup files and putting the new version beside them as
    /// `.pacnew`. Backup digests in `files` are updated. Returns the error count.
    fn extract_single_file(
        &self,
        entry: &mut Entry,
        entryname: &str,
        newpkg: &Pkg,
        files: &mut Files,
        oldpkg: Option<&Pkg>,
    ) -> usize {
        if entryname.starts_with('.') {
            return self.extract_db_file(entry, newpkg, entryname);
        }
        if files.contains(entryname).is_none() {
            log::warning(&format!(
                "file not found in file list for package {}. skipping extraction of {entryname}\n",
                newpkg.name
            ));
            return 0;
        }
        let entrymode = entry_mode(entry);
        let mut filename = format!("{}{entryname}", self.root);
        if util::fnmatch_patterns(&self.noextract, entryname) == Some(true) {
            log::debug(&format!("{entryname} is in NoExtract, skipping extraction of {filename}\n"));
            return 0;
        }
        let is_backup = files.backup.iter().any(|b| b.name == entryname);
        let (mut needbackup_, mut notouch) = (false, false);
        let mut hash_orig: Option<String> = None;
        let existing = llstat(&filename);
        let mut isnewfile = existing.is_none();
        match &existing {
            None => {}
            Some(meta) if meta.is_dir() && is_dir_mode(entrymode) => {
                if meta.mode() != entrymode {
                    let (fs, pkg) = (meta.mode() & 0o7777, entrymode & 0o7777);
                    log::warning(&format!(
                        "directory permissions differ on {filename}\nfilesystem: {fs:o}  package: {pkg:o}\n"
                    ));
                    self.logaction(
                        "ALPM",
                        &format!("warning: directory permissions differ on {filename}, filesystem: {fs:o}  package: {pkg:o}\n"),
                    );
                }
                log::debug(&format!("extract: skipping dir extraction of {filename}\n"));
                return 0;
            }
            Some(meta) if meta.is_dir() => {
                log::error(&format!("extract: not overwriting dir with file {filename}\n"));
                return 1;
            }
            Some(_) if is_dir_mode(entrymode) => {
                log::debug(&format!("extract: overwriting file with dir {filename}\n"));
            }
            Some(_) => {
                if util::fnmatch_patterns(&self.noupgrade, entryname) == Some(true) {
                    notouch = true;
                } else if let Some(old) = oldpkg.and_then(|o| needbackup(entryname, o)) {
                    hash_orig = Some(old.hash.clone());
                    needbackup_ = true;
                } else if is_backup {
                    // Backup files may be added retroactively.
                    needbackup_ = true;
                }
            }
        }
        let filename_len = filename.len();
        if notouch || needbackup_ {
            filename.push_str(".pacnew");
            isnewfile = matches!(std::fs::symlink_metadata(&filename), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        }
        log::debug(&format!("extracting {filename}\n"));
        if !perform_extraction(self, entry, &filename) {
            return 1;
        }
        let mut hash_pkg = None;
        if let Some(backup) = files.backup.iter_mut().find(|b| b.name == entryname) {
            backup.hash = util::md5_file(&filename).unwrap_or_default();
            hash_pkg = Some(backup.hash.clone());
        }
        let origfile = filename[..filename_len].to_string();
        let mut errors = 0;
        if notouch {
            log::event(&Event::PacnewCreated { file: &origfile });
            self.logaction("ALPM", &format!("warning: {origfile} installed as {origfile}.pacnew\n"));
        } else if needbackup_ {
            let hash_local = util::md5_file(&origfile);
            let hash_pkg = hash_pkg.or_else(|| util::md5_file(&filename));
            let show = |h: &Option<String>| h.clone().unwrap_or_else(|| "(null)".into());
            log::debug(&format!("checking hashes for {origfile}\n"));
            log::debug(&format!("current:  {}\n", show(&hash_local)));
            log::debug(&format!("new:      {}\n", show(&hash_pkg)));
            log::debug(&format!("original: {}\n", show(&hash_orig)));
            let same = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
            if same(&hash_local, &hash_pkg) {
                // Identical: install anyway for the timestamps.
                log::debug(&format!("action: installing new file: {origfile}\n"));
                errors += !try_rename(self, &filename, &origfile) as usize;
            } else if same(&hash_orig, &hash_pkg) {
                // The package did not change it: keep the user's version.
                log::debug("action: leaving existing file in place\n");
                if isnewfile {
                    let _ = std::fs::remove_file(&filename);
                }
            } else if same(&hash_orig, &hash_local) {
                // The user did not change it: take the new version.
                log::debug(&format!("action: installing new file: {origfile}\n"));
                errors += !try_rename(self, &filename, &origfile) as usize;
            } else {
                log::debug("action: keeping current file and installing new one with .pacnew ending\n");
                log::event(&Event::PacnewCreated { file: &origfile });
                self.logaction("ALPM", &format!("warning: {origfile} installed as {filename}\n"));
            }
        }
        errors
    }

    /// `commit_single_pkg`: replace the old version (if any) with the
    /// target, then record it in the local database.
    fn commit_single_pkg(&mut self, target: &AddPkg, current: usize, count: usize) -> Result<()> {
        let newpkg = &target.pkg;
        let oldpkg = target.old.clone().filter(|_| self.local.get(&newpkg.name).is_some());
        let mut reason = target.reason;
        let (operation, progress, log_msg) = match &oldpkg {
            Some(old) => {
                reason = old.info().reason;
                match super::vercmp(&newpkg.version, &old.version) {
                    c if c < 0 => (Operation::Downgrade, Progress::DowngradeStart, "downgrading"),
                    0 => (Operation::Reinstall, Progress::ReinstallStart, "reinstalling"),
                    _ => (Operation::Upgrade, Progress::UpgradeStart, "upgrading"),
                }
            }
            None => (Operation::Install, Progress::AddStart, "adding"),
        };
        let is_upgrade = oldpkg.is_some();
        log::event(&Event::PackageOperationStart);
        let Some(pkgfile) = target.file.as_ref().map(|p| p.to_string_lossy().into_owned()) else {
            return Err(ErrorKind::PkgNotFound.into());
        };
        log::debug(&format!("{log_msg} package {}-{}\n", newpkg.name, newpkg.version));
        let scriptlets = newpkg.info().has_scriptlet && !self.trans().has(FLAG_NOSCRIPTLET);
        let oldver = oldpkg.as_ref().map(|o| o.version.clone());
        if scriptlets {
            let name = if is_upgrade { "pre_upgrade" } else { "pre_install" };
            self.runscriptlet(&pkgfile, name, &newpkg.version, oldver.as_deref(), true);
        }
        if self.trans().has(FLAG_ALLDEPS) {
            reason = Reason::Depend;
        } else if self.trans().has(FLAG_ALLEXPLICIT) {
            reason = Reason::Explicit;
        }
        if let Some(old) = &oldpkg {
            self.remove_single_package(old, Some(newpkg), 0, 0);
        }
        if self.local_db_prepare(newpkg).is_err() {
            self.logaction("ALPM", &format!("error: could not create database entry {}-{}\n", newpkg.name, newpkg.version));
            return Err(ErrorKind::DbWrite.into());
        }
        let (mut archive, read) = super::archive::open_tar(&pkgfile)?;
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.set_preserve_ownerships(true);
        archive.set_unpack_xattrs(true);
        archive.set_overwrite(true);
        // Hard links in the archive are relative to the root.
        let cwd = std::env::current_dir().ok();
        if let Err(e) = std::env::set_current_dir(&self.root) {
            log::error(&format!("could not change directory to {} ({})\n", self.root, super::strerror(&e)));
            return Err(ErrorKind::System.into());
        }
        let mut files = newpkg.files().clone();
        let mut errors = 0;
        let dbonly = self.trans().has(FLAG_DBONLY);
        let size = newpkg.info().size;
        if dbonly {
            log::debug("extracting db files\n");
        } else {
            log::debug("extracting files\n");
            log::progress(progress, &newpkg.name, 0, count, current);
        }
        if let Ok(entries) = archive.entries() {
            for entry in entries {
                let Ok(mut entry) = entry else { break };
                let mut name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
                if entry.header().entry_type().is_dir() && !name.ends_with('/') {
                    name.push('/');
                }
                if dbonly {
                    if name.starts_with('.') {
                        errors += self.extract_db_file(&mut entry, newpkg, &name);
                    }
                    continue;
                }
                let percent = if size != 0 { (read.load(Ordering::Relaxed) as i64 * 100 / size).min(100) as i32 } else { 0 };
                log::progress(progress, &newpkg.name, percent, count, current);
                errors += self.extract_single_file(&mut entry, &name, newpkg, &mut files, oldpkg.as_deref());
            }
        }
        if let Some(cwd) = cwd {
            if let Err(e) = std::env::set_current_dir(cwd) {
                log::error(&format!("could not restore working directory ({})\n", super::strerror(&e)));
            }
        }
        let mut result = Ok(());
        if errors > 0 {
            result = Err(ErrorKind::TransAbort.into());
            let what = if is_upgrade { "upgrading" } else { "installing" };
            log::error(&format!("problem occurred while {what} {}\n", newpkg.name));
            self.logaction("ALPM", &format!("error: problem occurred while {what} {}\n", newpkg.name));
        }
        let mut info = newpkg.info().clone();
        info.installdate = unsafe { libc::time(std::ptr::null_mut()) };
        info.reason = reason;
        log::debug("updating database\n");
        log::debug(&format!("adding database entry '{}'\n", newpkg.name));
        if self.local_db_write(&newpkg.name, &newpkg.version, &info, Some(&files)).is_err() {
            log::error(&format!("could not update database entry {}-{}\n", newpkg.name, newpkg.version));
            self.logaction("ALPM", &format!("error: could not update database entry {}-{}\n", newpkg.name, newpkg.version));
            return Err(ErrorKind::DbWrite.into());
        }
        self.local_cache_add(&newpkg.name, &newpkg.version);
        log::progress(progress, &newpkg.name, 100, count, current);
        let (name, version) = (&newpkg.name, &newpkg.version);
        let old = oldver.as_deref().unwrap_or("");
        self.logaction(
            "ALPM",
            &match operation {
                Operation::Install => format!("installed {name} ({version})\n"),
                Operation::Downgrade => format!("downgraded {name} ({old} -> {version})\n"),
                Operation::Reinstall => format!("reinstalled {name} ({version})\n"),
                Operation::Upgrade => format!("upgraded {name} ({old} -> {version})\n"),
            },
        );
        if scriptlets {
            let script = self.local_pkgpath(newpkg, "install");
            let name = if is_upgrade { "post_upgrade" } else { "post_install" };
            self.runscriptlet(&script, name, &newpkg.version, oldver.as_deref(), false);
        }
        log::event(&Event::PackageOperationDone);
        result
    }

    /// `_alpm_upgrade_packages`: install every target in order. A failed
    /// package stops the rest and skips ldconfig.
    pub(crate) fn upgrade_packages(&mut self) -> Result<()> {
        let targets = self.trans().add.clone();
        for (i, target) in targets.iter().enumerate() {
            if self.commit_single_pkg(target, i + 1, targets.len()).is_err() {
                return Err(ErrorKind::TransAbort.into());
            }
        }
        self.ldconfig();
        Ok(())
    }
}
