//! Installing and upgrading packages, as libalpm's sync.c and add.c.
use super::conflict;
use super::deps::{self, DepMissing, Depend};
use super::log::{self, Event, Progress, Question};
use super::package::{Origin, Pkg, Reason};
use super::package::{VALIDATION_MD5SUM, VALIDATION_NONE, VALIDATION_SHA256SUM, VALIDATION_SIGNATURE};
use super::signing::{self, SigResult};
use super::trans::*;
use super::{dload, util, Db, Error, ErrorKind, Handle, Result};
use super::{SIG_DATABASE, SIG_DATABASE_OPTIONAL, SIG_PACKAGE, SIG_PACKAGE_MARGINAL_OK, SIG_PACKAGE_OPTIONAL, SIG_PACKAGE_UNKNOWN_OK};
use std::sync::Arc;

impl Handle {
    /// `check_literal`: whether the sync package should replace the
    /// installed one of the same name.
    fn check_literal(&self, lpkg: &Pkg, spkg: &Pkg, enable_downgrade: bool) -> bool {
        let cmp = super::vercmp(&spkg.version, &lpkg.version);
        let ignored = self.should_ignore(spkg) || self.should_ignore(lpkg);
        if cmp > 0 {
            log::debug(&format!("new version of '{}' found ({} => {})\n", lpkg.name, lpkg.version, spkg.version));
            if ignored {
                log::warning(&format!("{}: ignoring package upgrade ({} => {})\n", lpkg.name, lpkg.version, spkg.version));
            } else {
                log::debug(&format!("adding package {}-{} to the transaction targets\n", spkg.name, spkg.version));
                return true;
            }
        } else if cmp < 0 {
            if !enable_downgrade {
                log::warning(&format!("{}: local ({}) is newer than {} ({})\n", lpkg.name, lpkg.version, spkg.db_name(), spkg.version));
            } else if ignored {
                log::warning(&format!("{}: ignoring package downgrade ({} => {})\n", lpkg.name, lpkg.version, spkg.version));
            } else {
                log::warning(&format!("{}: downgrading from version {} to version {}\n", lpkg.name, lpkg.version, spkg.version));
                return true;
            }
        }
        false
    }

    /// `check_replacers`: packages of database `sdb` whose `replaces`
    /// literally name `lpkg`, once the front end agrees. A replacer already
    /// in the targets takes `lpkg` on its removal list instead.
    fn check_replacers(&mut self, lpkg: &Arc<Pkg>, sdb: usize) -> Vec<AddPkg> {
        log::debug(&format!("searching for replacements for {} in {}\n", lpkg.name, self.syncdbs[sdb].name));
        let mut replacers: Vec<AddPkg> = Vec::new();
        let candidates: Vec<Arc<Pkg>> =
            self.syncdbs[sdb].pkgs().iter().filter(|s| s.info().replaces.iter().any(|r| r.literal(lpkg))).cloned().collect();
        for spkg in candidates {
            if self.should_ignore(&spkg) || self.should_ignore(lpkg) {
                log::warning(&format!(
                    "ignoring package replacement ({}-{} => {}-{})\n",
                    lpkg.name, lpkg.version, spkg.name, spkg.version
                ));
                continue;
            }
            let dbname = self.syncdbs[sdb].name.clone();
            let mut q = Question::Replace { old: lpkg, new: &spkg, db: &dbname, replace: false };
            log::question(&mut q);
            if !matches!(q, Question::Replace { replace: true, .. }) {
                continue;
            }
            let explicit = lpkg.info().reason == Reason::Explicit;
            if let Some(tpkg) = self.trans_mut().add.iter_mut().find(|a| a.pkg.name == spkg.name) {
                if tpkg.pkg.db_name() != dbname {
                    log::warning(&format!("cannot replace {} by {}\n", lpkg.name, spkg.name));
                    continue;
                }
                log::debug(&format!("appending {} to the removes list of {}\n", lpkg.name, tpkg.pkg.name));
                tpkg.removes.push(lpkg.clone());
                if explicit {
                    tpkg.reason = Reason::Explicit;
                }
            } else {
                log::debug(&format!("adding package {}-{} to the transaction targets\n", spkg.name, spkg.version));
                let mut add = AddPkg::new(spkg);
                add.reason = lpkg.info().reason;
                add.removes = vec![lpkg.clone()];
                replacers.push(add);
            }
        }
        replacers
    }

    /// `alpm_sync_sysupgrade`: add every available upgrade, and replacement,
    /// of installed packages to the transaction.
    pub fn sync_sysupgrade(&mut self, enable_downgrade: bool) -> Result<()> {
        let trans = self.trans.as_ref().ok_or(ErrorKind::TransNull)?;
        if trans.state != State::Initialized {
            return Err(ErrorKind::TransNull.into());
        }
        log::debug("checking for package upgrades\n");
        let local: Vec<Arc<Pkg>> = self.local.pkgs().to_vec();
        for lpkg in &local {
            let trans = self.trans();
            if trans.remove.iter().any(|p| p.name == lpkg.name) {
                log::debug(&format!("{} is marked for removal -- skipping\n", lpkg.name));
                continue;
            }
            if trans.add.iter().any(|a| a.pkg.name == lpkg.name) {
                log::debug(&format!("{} is already in the target list -- skipping\n", lpkg.name));
                continue;
            }
            for sdb in 0..self.syncdbs.len() {
                if self.syncdbs[sdb].usage & super::db::USAGE_UPGRADE == 0 {
                    continue;
                }
                let replacers = self.check_replacers(lpkg, sdb);
                if !replacers.is_empty() {
                    self.trans_mut().add.extend(replacers);
                    break;
                }
                if let Some(spkg) = self.syncdbs[sdb].get(&lpkg.name).cloned() {
                    if self.check_literal(lpkg, &spkg, enable_downgrade) {
                        self.trans_mut().add.push(AddPkg::new(spkg));
                    }
                    break;
                }
            }
        }
        Ok(())
    }

    /// `_alpm_pkg_validate_internal`: check the archive at `path` against
    /// the sync entry's checksums and the signature `level` asks for.
    /// Returns the validation methods used, or why it failed with any
    /// signature verdicts.
    pub(crate) fn pkg_validate(&self, path: &str, syncpkg: Option<&Pkg>, level: i32) -> std::result::Result<u32, (ErrorKind, Vec<SigResult>)> {
        if let Err(errno) = util::access(path, libc::R_OK) {
            let kind = match errno {
                libc::ENOENT => ErrorKind::PkgNotFound,
                libc::EACCES => ErrorKind::BadPerms,
                _ => ErrorKind::PkgOpen,
            };
            return Err((kind, Vec::new()));
        }
        let base64_sig = syncpkg.map(|p| p.info().base64_sig.as_str()).filter(|s| !s.is_empty());
        let has_sig = level & SIG_PACKAGE != 0 && (base64_sig.is_some() || util::access(&format!("{path}.sig"), libc::R_OK).is_ok());
        let mut validation = 0;
        if let Some(spkg) = syncpkg.filter(|_| !has_sig || base64_sig.is_none()) {
            let info = spkg.info();
            if !info.md5sum.is_empty() && info.sha256sum.is_empty() {
                log::debug(&format!("md5sum: {}\n", info.md5sum));
                log::debug(&format!("checking md5sum for {path}\n"));
                if util::md5_file(path).as_deref() != Some(info.md5sum.as_str()) {
                    return Err((ErrorKind::PkgInvalidChecksum, Vec::new()));
                }
                validation |= VALIDATION_MD5SUM;
            }
            if !info.sha256sum.is_empty() {
                log::debug(&format!("sha256sum: {}\n", info.sha256sum));
                log::debug(&format!("checking sha256sum for {path}\n"));
                if util::sha256_file(path).as_deref() != Some(info.sha256sum.as_str()) {
                    return Err((ErrorKind::PkgInvalidChecksum, Vec::new()));
                }
                validation |= VALIDATION_SHA256SUM;
            }
        }
        if level & SIG_PACKAGE != 0 {
            log::debug(&format!("sig data: {}\n", base64_sig.unwrap_or("<from .sig>")));
            if !has_sig && level & SIG_PACKAGE_OPTIONAL == 0 {
                return Err((ErrorKind::PkgMissingSig, Vec::new()));
            }
            let (ok, results) = self.check_pgp(
                path,
                base64_sig,
                level & SIG_PACKAGE_OPTIONAL != 0,
                level & SIG_PACKAGE_MARGINAL_OK != 0,
                level & SIG_PACKAGE_UNKNOWN_OK != 0,
            );
            if !ok {
                return Err((ErrorKind::PkgInvalidSig, results));
            }
            if has_sig {
                validation |= VALIDATION_SIGNATURE;
            }
        }
        Ok(if validation == 0 { VALIDATION_NONE } else { validation })
    }

    /// `alpm_pkg_load`: read and validate a package archive.
    pub fn pkg_load(&self, path: &str, full: bool, level: i32) -> Result<Arc<Pkg>> {
        let sigpath = format!("{path}.sig");
        if level & SIG_PACKAGE != 0 && util::access(&sigpath, libc::R_OK).is_ok() {
            let Ok(sig) = std::fs::read(&sigpath) else {
                log::error(&format!("failed to read signature file: {sigpath}\n"));
                return Err(ErrorKind::SigInvalid.into());
            };
            let mut failed = false;
            for key in self.missing_keys(path, &sig).unwrap_or_default() {
                failed |= !self.key_import(None, &key);
            }
            if failed {
                log::error("required key missing from keyring\n");
                return Err(ErrorKind::PkgInvalidSig.into());
            }
        }
        let validation = match self.pkg_validate(path, None, level) {
            Ok(v) => v,
            Err((kind, results)) => {
                if kind == ErrorKind::PkgInvalidSig {
                    self.process_siglist(path, &results, level & SIG_PACKAGE_OPTIONAL != 0, level & SIG_PACKAGE_MARGINAL_OK != 0, level & SIG_PACKAGE_UNKNOWN_OK != 0);
                }
                return Err(kind.into());
            }
        };
        let pkg = super::archive::load(path, full)?;
        Ok(Arc::new(with_validation(pkg, validation)))
    }

    /// `_alpm_filecache_setup`: the first usable cache directory, created if need be.
    fn filecache_setup(&self) -> String {
        for dir in &self.cachedirs {
            if !std::path::Path::new(dir).exists() && util::makepath_mode(dir, 0o755).is_err() {
                log::warning(&format!("could not create directory {dir}\n"));
                continue;
            }
            if util::access(dir, libc::W_OK).is_ok() {
                log::debug(&format!("using cachedir: {dir}\n"));
                return dir.clone();
            }
            log::debug(&format!("skipping cachedir, not writable: {dir}\n"));
        }
        log::warning("couldn't find or create package cache, using /tmp instead\n");
        "/tmp/".into()
    }

    /// `alpm_fetch_pkgurl`: the cached copy of a package URL, downloading
    /// it (and its signature) when it is not cached.
    pub fn fetch_pkgurl(&mut self, url: &str) -> Result<String> {
        let siglevel = self.remote_file_siglevel;
        let base = url.rsplit('/').next().unwrap_or("").to_string();
        if !base.is_empty() {
            if let Some(path) = self.filecache_find(&base) {
                if siglevel & SIG_PACKAGE == 0 || self.filecache_find(&format!("{base}.sig")).is_some() {
                    return Ok(path);
                }
            }
        }
        if base.is_empty() {
            log::error(&format!("url '{url}' is invalid\n"));
            return Err(ErrorKind::ServerBadUrl.into());
        }
        let cachedir = self.filecache_setup();
        let payload = dload::Payload {
            remote_name: base.clone(),
            fileurl: Some(url.to_string()),
            servers: Vec::new(),
            cache_servers: Vec::new(),
            max_size: 0,
            download_signature: siglevel & SIG_PACKAGE != 0,
            signature_optional: siglevel & SIG_PACKAGE_OPTIONAL != 0,
            force: false,
            allow_resume: base.contains(".pkg"),
        };
        log::event(&Event::PkgRetrieveStart { num: 1, total_size: 0 });
        if let Err(e) = self.download(std::slice::from_ref(&payload), &cachedir, false) {
            log::warning("failed to retrieve some files\n");
            log::event(&Event::PkgRetrieveFailed);
            return Err(e);
        }
        log::event(&Event::PkgRetrieveDone);
        self.filecache_find(&base).ok_or_else(|| {
            log::warning("download completed successfully but no file in the cache\n");
            ErrorKind::Retrieve.into()
        })
    }

    /// `alpm_db_update`: refresh every sync database marked for syncing,
    /// skipping those already as new as the server's unless `force`.
    /// Returns whether each was already up to date.
    pub fn db_update(&mut self, force: bool) -> Result<Vec<bool>> {
        let syncdir = format!("{}sync/", self.dbpath);
        if util::makepath_mode(&syncdir, 0o755).is_err() {
            return Err(ErrorKind::System.into());
        }
        let old = unsafe { libc::umask(0o022) };
        let locked = self.trans.is_some() || self.lock().is_ok();
        if !locked {
            unsafe { libc::umask(old) };
            return Err(ErrorKind::HandleLock.into());
        }
        let mut payloads = Vec::new();
        let mut result = Ok(());
        for db in self.syncdbs.iter().filter(|db| db.usage & super::db::USAGE_SYNC != 0) {
            if db.servers.is_empty() {
                result = Err(ErrorKind::ServerNone.into());
                break;
            }
            payloads.push(dload::Payload {
                remote_name: format!("{}.db", db.name),
                fileurl: None,
                servers: db.servers.clone(),
                cache_servers: db.cache_servers.clone(),
                max_size: 128 * 1024 * 1024,
                download_signature: db.siglevel & SIG_DATABASE != 0,
                signature_optional: db.siglevel & SIG_DATABASE_OPTIONAL != 0,
                force,
                allow_resume: false,
            });
        }
        let mut uptodate = Vec::new();
        if result.is_ok() && !payloads.is_empty() {
            match self.download(&payloads, &syncdir, true) {
                Ok(fetched) => uptodate = fetched.iter().map(|f| *f == dload::Fetched::UpToDate).collect(),
                Err(e) => result = Err(e),
            }
            for index in 0..self.syncdbs.len() {
                if self.syncdbs[index].usage & super::db::USAGE_SYNC == 0 {
                    continue;
                }
                self.syncdbs[index].invalidate();
                // After a failed download the download error stands.
                if result.is_ok() && !self.sync_db_validate(index) {
                    log::debug(&format!("failed to validate db: {}\n", self.syncdbs[index].name));
                    if result.is_ok() {
                        result = Err(ErrorKind::DbInvalidSig.into());
                    }
                }
            }
        }
        if self.trans.is_none() {
            self.unlock();
        }
        unsafe { libc::umask(old) };
        result.map(|()| uptodate)
    }

    /// `alpm_find_group_pkgs`: the first package of each name in the group,
    /// over the given sync databases.
    pub fn find_group_pkgs(&self, dbs: &[usize], group: &str) -> Vec<Arc<Pkg>> {
        let mut found: Vec<Arc<Pkg>> = Vec::new();
        for &i in dbs {
            for pkg in self.syncdbs[i].pkgs() {
                if pkg.info().groups.iter().any(|g| g == group) && !found.iter().any(|p| p.name == pkg.name) {
                    if self.should_ignore(pkg) {
                        let mut q = super::Question::InstallIgnorePkg { pkg, install: false };
                        super::log::question(&mut q);
                        if !matches!(q, super::Question::InstallIgnorePkg { install: true, .. }) {
                            continue;
                        }
                    }
                    found.push(pkg.clone());
                }
            }
        }
        found
    }

    /// `_alpm_resolvedeps`: add `pkg` and, recursively, providers of its
    /// missing dependencies to `packages`. On failure `packages` is left as
    /// it was and the unsatisfiable dependencies are appended to `missing`.
    fn resolvedeps(
        &self,
        localpkgs: &[Arc<Pkg>],
        pkg: &Arc<Pkg>,
        preferred: &[Arc<Pkg>],
        packages: &mut Vec<Arc<Pkg>>,
        rem: &[Arc<Pkg>],
        missing: &mut Vec<DepMissing>,
    ) -> bool {
        if packages.iter().any(|p| p.name == pkg.name) {
            return true;
        }
        let saved = packages.len();
        packages.push(pkg.clone());
        log::debug("started resolving dependencies\n");
        let nodepversion = self.trans().has(FLAG_NODEPVERSION);
        let deps = deps::checkdeps_with(localpkgs, rem, std::slice::from_ref(pkg), false, &self.assumeinstalled, nodepversion);
        let mut ok = true;
        for miss in deps {
            if deps::find_satisfier(packages.iter(), &miss.depend).is_some() {
                continue;
            }
            let spkg = deps::find_satisfier(preferred, &miss.depend).cloned().or_else(|| {
                let dbs: Vec<&Db> = self.syncdbs.iter().collect();
                self.resolvedep(&miss.depend, &dbs, packages, false).ok()
            });
            if let Some(spkg) = spkg.filter(|s| self.resolvedeps(localpkgs, s, preferred, packages, rem, missing)) {
                log::debug(&format!("pulling dependency {} (needed by {})\n", spkg.name, pkg.name));
            } else if self.resolvedep(&miss.depend, &[&self.local], rem, false).is_ok() {
            } else {
                log::warning(&format!("cannot resolve \"{}\", a dependency of \"{}\"\n", miss.depend, pkg.name));
                missing.push(miss);
                ok = false;
            }
        }
        if !ok {
            packages.truncate(saved);
        }
        log::debug("finished resolving dependencies\n");
        ok
    }

    /// `_alpm_sync_prepare`: pull in dependencies, settle conflicts between
    /// targets and with installed packages, and build the removal list.
    pub(crate) fn sync_prepare(&mut self) -> Result<()> {
        let from_sync = self.trans().add.iter().any(|a| matches!(a.pkg.origin, Origin::Sync { .. }));
        if from_sync && self.syncdbs.iter().any(|db| !db.path.exists()) {
            return Err(ErrorKind::DbNotFound.into());
        }
        let flags = self.trans().flags;
        if flags & FLAG_NODEPS == 0 {
            log::event(&Event::ResolvedepsStart);
            log::debug("resolving target's dependencies\n");
            let trans = self.trans();
            let mut remove = trans.remove.clone();
            remove.extend(trans.add.iter().flat_map(|a| a.removes.iter().cloned()));
            let targets: Vec<Arc<Pkg>> = trans.add.iter().map(|a| a.pkg.clone()).collect();
            let localpkgs: Vec<Arc<Pkg>> =
                self.local.pkgs().iter().filter(|p| !targets.iter().any(|t| t.name == p.name)).cloned().collect();
            let (mut resolved, mut unresolvable, mut missing) = (Vec::new(), Vec::new(), Vec::new());
            for pkg in &targets {
                if !self.resolvedeps(&localpkgs, pkg, &targets, &mut resolved, &remove, &mut missing) {
                    unresolvable.push(pkg.clone());
                }
            }
            if !unresolvable.is_empty() {
                let mut q = Question::RemovePkgs { packages: &unresolvable, skip: false };
                log::question(&mut q);
                if !matches!(q, Question::RemovePkgs { skip: true, .. }) {
                    let mut e = Error::new(ErrorKind::UnsatisfiedDeps);
                    e.missing = missing;
                    return Err(e);
                }
            }
            let mut duplicate = false;
            for (i, a) in resolved.iter().enumerate() {
                for b in &resolved[i + 1..] {
                    let (fa, fb) = (&a.info().filename, &b.info().filename);
                    if !fa.is_empty() && fa == fb {
                        log::error(&format!("packages {} and {} have the same filename: {fa}\n", a.name, b.name));
                        duplicate = true;
                    }
                }
            }
            if duplicate {
                return Err(ErrorKind::TransDupFilename.into());
            }
            let trans = self.trans_mut();
            let mut old = std::mem::take(&mut trans.add);
            trans.add = resolved
                .into_iter()
                .map(|pkg| match old.iter().position(|a| Arc::ptr_eq(&a.pkg, &pkg)) {
                    Some(at) => old.swap_remove(at),
                    None => {
                        let mut add = AddPkg::new(pkg);
                        add.reason = Reason::Depend;
                        add
                    }
                })
                .collect();
            trans.unresolvable = unresolvable;
            log::event(&Event::ResolvedepsDone);
        }

        if flags & FLAG_NOCONFLICTS == 0 {
            log::event(&Event::InterconflictsStart);
            log::debug("looking for conflicts\n");
            log::debug("check targets vs targets\n");
            let targets: Vec<Arc<Pkg>> = self.trans().add.iter().map(|a| a.pkg.clone()).collect();
            for c in conflict::innerconflicts(&targets) {
                let trans = self.trans();
                let find = |name: &str| trans.add.iter().find(|a| a.pkg.name == name).map(|a| a.pkg.clone());
                let (Some(sync1), Some(sync2)) = (find(&c.package1.name), find(&c.package2.name)) else { continue };
                log::debug(&format!("conflicting packages in the sync list: '{}' <-> '{}'\n", sync1.name, sync2.name));
                let (rsync, sync) = if Depend::parse(&sync2.name).satisfied_by(&sync1) {
                    (sync2, sync1)
                } else if Depend::parse(&sync1.name).satisfied_by(&sync2) {
                    (sync1, sync2)
                } else {
                    log::error("unresolvable package conflicts detected\n");
                    let mut e = Error::new(ErrorKind::ConflictingDeps);
                    e.conflicts.push(c);
                    return Err(e);
                };
                log::warning(&format!(
                    "removing '{}-{}' from target list because it conflicts with '{}-{}'\n",
                    rsync.name, rsync.version, sync.name, sync.version
                ));
                let trans = self.trans_mut();
                trans.add.retain(|a| a.pkg.name != rsync.name);
                trans.unresolvable.push(rsync);
            }

            log::debug("check targets vs db and db vs targets\n");
            let targets: Vec<Arc<Pkg>> = self.trans().add.iter().map(|a| a.pkg.clone()).collect();
            for c in conflict::outerconflicts(self.local.pkgs(), &targets) {
                let (name1, name2) = (c.package1.name.clone(), c.package2.name.clone());
                let trans = self.trans();
                if trans.remove.iter().any(|p| p.name == name2)
                    || trans.add.iter().any(|a| a.removes.iter().any(|p| p.name == name2))
                {
                    continue;
                }
                log::debug(&format!("package '{name1}-{}' conflicts with '{name2}-{}'\n", c.package1.version, c.package2.version));
                let mut q = Question::Conflict { package1: &name1, package2: &name2, reason: &c.reason, remove: false };
                log::question(&mut q);
                if matches!(q, Question::Conflict { remove: true, .. }) {
                    log::debug(&format!("electing '{name2}' for removal\n"));
                    let local = self.local.get(&name2).cloned();
                    let trans = self.trans_mut();
                    if let (Some(local), Some(add)) = (local, trans.add.iter_mut().find(|a| a.pkg.name == name1)) {
                        add.removes.push(local);
                    }
                } else {
                    log::error("unresolvable package conflicts detected\n");
                    let mut e = Error::new(ErrorKind::ConflictingDeps);
                    e.conflicts.push(c);
                    return Err(e);
                }
            }
            log::event(&Event::InterconflictsDone);
        }

        let trans = self.trans_mut();
        let removes: Vec<Arc<Pkg>> = trans.add.iter().flat_map(|a| a.removes.iter().cloned()).collect();
        for rpkg in removes {
            if !trans.remove.iter().any(|p| p.name == rpkg.name) {
                log::debug(&format!("adding '{}' to remove list\n", rpkg.name));
                trans.remove.push(rpkg);
            }
        }

        if flags & FLAG_NODEPS == 0 {
            log::debug("checking dependencies\n");
            let trans = self.trans();
            let add: Vec<Arc<Pkg>> = trans.add.iter().map(|a| a.pkg.clone()).collect();
            let missing = deps::checkdeps_with(
                self.local.pkgs(),
                &trans.remove,
                &add,
                true,
                &self.assumeinstalled,
                flags & FLAG_NODEPVERSION != 0,
            );
            if !missing.is_empty() {
                let mut e = Error::new(ErrorKind::UnsatisfiedDeps);
                e.missing = missing;
                return Err(e);
            }
        }
        let olds: Vec<Option<Arc<Pkg>>> = self.trans().add.iter().map(|a| self.local.get(&a.pkg.name).cloned()).collect();
        for (add, old) in self.trans_mut().add.iter_mut().zip(olds) {
            add.old = old;
        }
        Ok(())
    }

    /// The siglevel for packages from `pkg`'s repository.
    fn pkg_siglevel(&self, pkg: &Pkg) -> i32 {
        self.syncdbs.iter().find(|d| d.name == pkg.db_name()).map_or(self.siglevel, |d| d.siglevel)
    }

    /// `download_files`: fetch the sync targets missing from the cache (or
    /// whose signature is), from their repository's servers.
    fn download_files(&mut self) -> Result<()> {
        let cachedir = self.filecache_setup();
        let mut payloads = Vec::new();
        let mut total_size = 0;
        let mut sizes = Vec::new();
        for add in &self.trans().add {
            let pkg = &add.pkg;
            if add.file.is_some() {
                continue;
            }
            let Some(db) = self.syncdbs.iter().find(|d| d.name == pkg.db_name()) else { continue };
            if db.servers.is_empty() && db.cache_servers.is_empty() {
                log::error(&format!("{}: {}\n", ErrorKind::ServerNone.message(), db.name));
                return Err(ErrorKind::ServerNone.into());
            }
            let filename = &pkg.info().filename;
            if filename.is_empty() {
                return Err(ErrorKind::PkgInvalidName.into());
            }
            let siglevel = db.siglevel;
            let mut need = self.filecache_find(filename).is_none();
            if !need && siglevel & SIG_PACKAGE != 0 {
                need = self.filecache_find(&format!("{filename}.sig")).is_none();
            }
            if need {
                let size = self.download_size(pkg);
                total_size += size;
                sizes.push(size);
                payloads.push(dload::Payload {
                    remote_name: filename.clone(),
                    fileurl: None,
                    servers: db.servers.clone(),
                    cache_servers: db.cache_servers.clone(),
                    max_size: pkg.info().size.max(0) as u64,
                    download_signature: siglevel & SIG_PACKAGE != 0,
                    signature_optional: siglevel & SIG_PACKAGE_OPTIONAL != 0,
                    force: false,
                    allow_resume: true,
                });
            }
        }
        if payloads.is_empty() {
            return Ok(());
        }
        if self.checkspace {
            log::debug("checking available disk space for download\n");
            self.check_downloadspace(&cachedir, &sizes)?;
        }
        log::event(&Event::PkgRetrieveStart { num: payloads.len(), total_size });
        if let Err(e) = self.download(&payloads, &cachedir, false) {
            log::event(&Event::PkgRetrieveFailed);
            log::warning("failed to retrieve some files\n");
            return Err(e);
        }
        log::event(&Event::PkgRetrieveDone);
        Ok(())
    }

    /// `check_keyring`: every key that signed a sync target must be in the
    /// keyring; offer to import those that are not.
    fn check_keyring(&mut self) -> Result<()> {
        use base64::Engine;
        log::event(&Event::KeyringStart);
        let add: Vec<Arc<Pkg>> = self.trans().add.iter().map(|a| a.pkg.clone()).collect();
        let total = add.len();
        // (packager, key id)
        let mut missing: Vec<(String, String)> = Vec::new();
        for (current, pkg) in add.iter().enumerate() {
            log::progress(Progress::KeyringStart, "", (current * 100 / total.max(1)) as i32, total, current);
            // Archives from -U were checked when they were loaded.
            if matches!(pkg.origin, super::package::Origin::File { .. }) || self.pkg_siglevel(pkg) & SIG_PACKAGE == 0 {
                continue;
            }
            let info = pkg.info();
            let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(info.base64_sig.trim()) else { continue };
            if sig.is_empty() {
                continue;
            }
            for key in signing::extract_keyid(&pkg.name, &sig).unwrap_or_default() {
                log::debug(&format!("found signature key: {key}\n"));
                if !missing.iter().any(|(_, k)| *k == key) && self.key_in_keychain(&key) == Some(false) {
                    missing.push((info.packager.clone(), key));
                }
            }
        }
        log::progress(Progress::KeyringStart, "", 100, total, total);
        log::event(&Event::KeyringDone);
        if missing.is_empty() {
            return Ok(());
        }
        log::event(&Event::KeyDownloadStart);
        let mut failed = false;
        for (uid, key) in &missing {
            failed |= !self.key_import(Some(uid), key);
        }
        log::event(&Event::KeyDownloadDone);
        if failed {
            log::error("required key missing from keyring\n");
            return Err(ErrorKind::PkgInvalidSig.into());
        }
        Ok(())
    }

    /// `prompt_to_delete`: offer to delete a corrupt download and its signature.
    fn prompt_to_delete(&self, path: &str) {
        let mut q = Question::CorruptedPkg { filepath: path, remove: false };
        log::question(&mut q);
        if matches!(q, Question::CorruptedPkg { remove: true, .. }) {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_file(format!("{path}.sig"));
        }
    }

    /// `check_validity`: verify every cached sync target's checksum and
    /// signature before anything is unpacked.
    fn check_validity(&mut self) -> Result<()> {
        log::event(&Event::IntegrityStart);
        let targets: Vec<Arc<Pkg>> = self.trans().add.iter().filter(|a| a.file.is_none()).map(|a| a.pkg.clone()).collect();
        let total = self.trans().add.len();
        let mut errors: Vec<(Arc<Pkg>, String, i32, ErrorKind, Vec<SigResult>)> = Vec::new();
        let mut validations = Vec::new();
        for (current, pkg) in targets.iter().enumerate() {
            log::progress(Progress::IntegrityStart, "", (current * 100 / total.max(1)) as i32, total, current);
            let Some(path) = self.filecache_find(&pkg.info().filename) else {
                log::error(&format!("{}: could not find package in cache\n", pkg.name));
                return Err(ErrorKind::PkgNotFound.into());
            };
            let level = self.pkg_siglevel(pkg);
            match self.pkg_validate(&path, Some(pkg), level) {
                Ok(validation) => validations.push((pkg.clone(), path, validation)),
                Err((kind, results)) => errors.push((pkg.clone(), path, level, kind, results)),
            }
        }
        log::progress(Progress::IntegrityStart, "", 100, total, total);
        log::event(&Event::IntegrityDone);
        let Some(last) = errors.last().map(|e| e.3) else {
            for (pkg, path, validation) in validations {
                if let Some(add) = self.trans_mut().add.iter_mut().find(|a| Arc::ptr_eq(&a.pkg, &pkg)) {
                    add.validation = validation;
                    add.file = Some(path.into());
                }
            }
            return Ok(());
        };
        for (pkg, path, level, kind, results) in &errors {
            match kind {
                ErrorKind::PkgMissingSig => log::error(&format!("{}: missing required signature\n", pkg.name)),
                ErrorKind::PkgInvalidSig | ErrorKind::PkgInvalidChecksum => {
                    if *kind == ErrorKind::PkgInvalidSig {
                        self.process_siglist(
                            &pkg.name,
                            results,
                            level & SIG_PACKAGE_OPTIONAL != 0,
                            level & SIG_PACKAGE_MARGINAL_OK != 0,
                            level & SIG_PACKAGE_UNKNOWN_OK != 0,
                        );
                    }
                    self.prompt_to_delete(path);
                }
                ErrorKind::PkgNotFound | ErrorKind::BadPerms | ErrorKind::PkgOpen => {
                    log::error(&format!("failed to read file {path}: {}\n", kind.message()))
                }
                _ => {}
            }
        }
        Err(last.into())
    }

    /// `load_packages`: replace each sync target by its archive, which must
    /// match what the database said about it.
    fn load_packages(&mut self) -> Result<()> {
        log::event(&Event::LoadStart);
        let total = self.trans().add.len();
        let mut invalid = Vec::new();
        for i in 0..total {
            log::progress(Progress::LoadStart, "", (i * 100 / total.max(1)) as i32, total, i);
            let add = &self.trans().add[i];
            if add.pkg.path().is_some() {
                continue;
            }
            let spkg = add.pkg.clone();
            let path = add.file.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            log::debug(&format!("replacing pkgcache entry with package file for target {}\n", spkg.name));
            let loaded = match super::archive::load(&path, true) {
                Ok(pkgfile) if pkg_matches_db(&spkg, &pkgfile) => Some(pkgfile),
                Ok(_) => None,
                Err(_) => {
                    log::debug("failed to load pkgfile internal\n");
                    None
                }
            };
            match loaded {
                Some(pkgfile) => {
                    let add = &mut self.trans_mut().add[i];
                    add.pkg = Arc::new(with_validation(pkgfile, add.validation));
                }
                None => invalid.push((spkg.info().filename.clone(), path)),
            }
        }
        log::progress(Progress::LoadStart, "", 100, total, total);
        log::event(&Event::LoadDone);
        if invalid.is_empty() {
            return Ok(());
        }
        for (_, path) in &invalid {
            self.prompt_to_delete(path);
        }
        let mut e = Error::new(ErrorKind::PkgInvalid);
        e.names = invalid.into_iter().map(|(name, _)| name).collect();
        Err(e)
    }

    /// `_alpm_sync_load`: download what is missing, verify it, and load the
    /// archives (stopping after verification for DOWNLOADONLY).
    pub(crate) fn sync_load(&mut self) -> Result<()> {
        self.download_files()?;
        self.check_keyring()?;
        self.check_validity()?;
        if self.trans().has(FLAG_DOWNLOADONLY) {
            return Ok(());
        }
        self.load_packages()
    }

    /// `_alpm_sync_check`: refuse file conflicts.
    pub(crate) fn sync_check(&mut self) -> Result<()> {
        if self.trans().has(FLAG_DBONLY) {
            return Ok(());
        }
        log::event(&Event::FileconflictsStart);
        log::debug("looking for file conflicts\n");
        let add: Vec<Arc<Pkg>> = self.trans().add.iter().map(|a| a.pkg.clone()).collect();
        let remove = self.trans().remove.clone();
        let conflicts = self.find_fileconflicts(&add, &remove);
        if !conflicts.is_empty() {
            let mut e = Error::new(ErrorKind::FileConflicts);
            e.file_conflicts = conflicts;
            return Err(e);
        }
        log::event(&Event::FileconflictsDone);
        if self.checkspace {
            log::event(&Event::DiskspaceStart);
            log::debug("checking available disk space\n");
            if let Err(e) = self.check_diskspace() {
                log::error("not enough free disk space\n");
                return Err(e);
            }
            log::event(&Event::DiskspaceDone);
        }
        Ok(())
    }

    /// `_alpm_sync_commit`: remove what the targets replace, then install them.
    pub(crate) fn sync_commit(&mut self) -> Result<()> {
        if !self.trans().remove.is_empty() {
            log::debug("removing conflicting and to-be-replaced packages\n");
            if let Err(e) = self.remove_packages(false) {
                log::error("could not commit removal transaction\n");
                return Err(e);
            }
        }
        log::debug("installing packages\n");
        if let Err(e) = self.upgrade_packages() {
            log::error("could not commit transaction\n");
            return Err(e);
        }
        Ok(())
    }
}

/// The package with its validation methods recorded.
fn with_validation(pkg: Pkg, validation: u32) -> Pkg {
    let mut info = pkg.info().clone();
    info.validation = validation;
    let files = pkg.files().clone();
    Pkg::with_info(pkg.name, pkg.version, pkg.origin, info, Some(files))
}

/// `check_pkg_matches_db`: whether an archive is the package the sync
/// database described.
fn pkg_matches_db(spkg: &Pkg, pkgfile: &Pkg) -> bool {
    let (s, f) = (spkg.info(), pkgfile.info());
    let mut ok = true;
    let mut check = |field: &str, same: bool| {
        if !same {
            log::debug(&format!("internal package {field} mismatch\n"));
            ok = false;
        }
    };
    check("name", spkg.name == pkgfile.name);
    check("version", spkg.version == pkgfile.version);
    check("install size", s.isize == f.isize);
    let deps = |a: &[Depend], b: &[Depend]| {
        let key = |d: &Depend| (d.name.clone(), d.modifier as u8, d.version.clone());
        let (mut a, mut b): (Vec<_>, Vec<_>) = (a.iter().map(key).collect(), b.iter().map(key).collect());
        a.sort();
        b.sort();
        a == b
    };
    check("depends", deps(&s.depends, &f.depends));
    check("conflicts", deps(&s.conflicts, &f.conflicts));
    check("replaces", deps(&s.replaces, &f.replaces));
    check("provides", deps(&s.provides, &f.provides));
    let (mut g1, mut g2) = (s.groups.clone(), f.groups.clone());
    g1.sort();
    g2.sort();
    check("groups", g1 == g2);
    ok
}
