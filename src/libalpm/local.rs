//! Writing the local database, as libalpm's be_local.c: one directory per
//! installed package holding `desc`, `files`, and optionally `mtree` and
//! `install`.
use super::deps::Depend;
use super::log;
use super::package::{self, Files, Info, Origin, Pkg, Reason};
use super::{ErrorKind, Handle, Result};
use std::fmt::Write as _;
use std::sync::Arc;

/// `_alpm_local_db_write` with INFRQ_DESC: the desc file's contents.
pub fn desc_text(name: &str, version: &str, info: &Info) -> String {
    let mut s = String::new();
    let mut field = |key: &str, value: &dyn std::fmt::Display| {
        let _ = write!(s, "%{key}%\n{value}\n\n");
    };
    field("NAME", &name);
    field("VERSION", &version);
    if !info.base.is_empty() {
        field("BASE", &info.base);
    }
    if !info.desc.is_empty() {
        field("DESC", &info.desc);
    }
    if !info.url.is_empty() {
        field("URL", &info.url);
    }
    if !info.arch.is_empty() {
        field("ARCH", &info.arch);
    }
    if info.builddate != 0 {
        field("BUILDDATE", &info.builddate);
    }
    if info.installdate != 0 {
        field("INSTALLDATE", &info.installdate);
    }
    if !info.packager.is_empty() {
        field("PACKAGER", &info.packager);
    }
    if info.isize != 0 {
        field("SIZE", &info.isize);
    }
    if info.reason != Reason::Explicit {
        field("REASON", &reason_number(info.reason));
    }
    let list = |s: &mut String, key: &str, values: &mut dyn Iterator<Item = String>| {
        let values: Vec<String> = values.collect();
        if !values.is_empty() {
            let _ = writeln!(s, "%{key}%");
            for v in values {
                let _ = writeln!(s, "{v}");
            }
            s.push('\n');
        }
    };
    list(&mut s, "GROUPS", &mut info.groups.iter().cloned());
    list(&mut s, "LICENSE", &mut info.licenses.iter().cloned());
    if info.validation != 0 {
        let names = [
            (package::VALIDATION_NONE, "none"),
            (package::VALIDATION_MD5SUM, "md5"),
            (package::VALIDATION_SHA256SUM, "sha256"),
            (package::VALIDATION_SIGNATURE, "pgp"),
        ];
        list(&mut s, "VALIDATION", &mut names.iter().filter(|(bit, _)| info.validation & bit != 0).map(|(_, n)| n.to_string()));
    }
    let deps = |s: &mut String, key: &str, deps: &[Depend]| list(s, key, &mut deps.iter().map(|d| d.to_string()));
    deps(&mut s, "REPLACES", &info.replaces);
    deps(&mut s, "DEPENDS", &info.depends);
    deps(&mut s, "OPTDEPENDS", &info.optdepends);
    deps(&mut s, "CONFLICTS", &info.conflicts);
    deps(&mut s, "PROVIDES", &info.provides);
    list(&mut s, "XDATA", &mut info.xdata.iter().map(|(k, v)| format!("{k}={v}")));
    s
}

fn reason_number(reason: Reason) -> u32 {
    match reason {
        Reason::Explicit => 0,
        Reason::Depend => 1,
        Reason::Unknown => 2,
    }
}

/// `_alpm_local_db_write` with INFRQ_FILES: the files file's contents.
pub fn files_text(files: &Files) -> String {
    let mut s = String::new();
    if !files.files.is_empty() {
        s.push_str("%FILES%\n");
        for f in &files.files {
            s.push_str(&f.name);
            s.push('\n');
        }
        s.push('\n');
    }
    if !files.backup.is_empty() {
        s.push_str("%BACKUP%\n");
        for b in &files.backup {
            let _ = writeln!(s, "{}\t{}", b.name, b.hash);
        }
        s.push('\n');
    }
    s
}

fn write_file(path: &str, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|e| {
        log::error(&format!("could not open file {path}: {}\n", super::strerror(&e)));
        ErrorKind::DbWrite.into()
    })
}

impl Handle {
    /// `_alpm_local_db_pkgpath`: the entry directory, or a file in it.
    pub fn local_pkgpath(&self, pkg: &Pkg, file: &str) -> String {
        format!("{}{}-{}/{}", self.local.path.display(), pkg.name, pkg.version, file)
    }

    /// `_alpm_local_db_prepare`: create the package's entry directory.
    pub(crate) fn local_db_prepare(&self, pkg: &Pkg) -> Result<()> {
        let dir = self.local.path.to_string_lossy().into_owned();
        match std::fs::metadata(&dir) {
            Err(_) => {
                log::debug(&format!("database dir '{dir}' does not exist, creating it\n"));
                super::util::makepath_mode(&dir, 0o755).map_err(|_| ErrorKind::System)?;
            }
            Ok(meta) if !meta.is_dir() => {
                log::warning(&format!("removing invalid database: {dir}\n"));
                std::fs::remove_file(&dir)
                    .and_then(|_| super::util::makepath_mode(&dir, 0o755))
                    .map_err(|_| ErrorKind::System)?;
            }
            Ok(_) => {}
        }
        let path = self.local_pkgpath(pkg, "");
        let old = unsafe { libc::umask(0) };
        let made = std::fs::create_dir(&path);
        unsafe { libc::umask(old) };
        made.map_err(|e| {
            log::error(&format!("could not create directory {path}: {}\n", super::strerror(&e)));
            ErrorKind::DbWrite.into()
        })
    }

    /// `_alpm_local_db_write`: write desc and, with `files`, the file list.
    pub(crate) fn local_db_write(&self, name: &str, version: &str, info: &Info, files: Option<&Files>) -> Result<()> {
        let dir = format!("{}{}-{}/", self.local.path.display(), name, version);
        let old = unsafe { libc::umask(0o022) };
        log::debug(&format!("writing {name}-{version} DESC information back to db\n"));
        let mut result = write_file(&format!("{dir}desc"), &desc_text(name, version, info));
        if let (Ok(()), Some(files)) = (&result, files) {
            log::debug(&format!("writing {name}-{version} FILES information back to db\n"));
            result = write_file(&format!("{dir}files"), &files_text(files));
        }
        unsafe { libc::umask(old) };
        result
    }

    /// `_alpm_local_db_remove`: delete the package's entry directory.
    pub(crate) fn local_db_remove(&self, pkg: &Pkg) -> Result<()> {
        let path = self.local_pkgpath(pkg, "");
        let mut failed = false;
        let entries = std::fs::read_dir(&path).map_err(|_| ErrorKind::DbRemove)?;
        for entry in entries.flatten() {
            failed |= std::fs::remove_file(entry.path()).is_err();
        }
        failed |= std::fs::remove_dir(&path).is_err();
        if failed {
            Err(ErrorKind::DbRemove.into())
        } else {
            Ok(())
        }
    }

    /// `alpm_pkg_set_reason`: record a new install reason for an installed package.
    pub fn pkg_set_reason(&mut self, pkg: &Arc<Pkg>, reason: Reason) -> Result<()> {
        let Origin::Local { dir } = &pkg.origin else { return Err(ErrorKind::WrongArgs.into()) };
        log::debug(&format!("setting install reason {} for {}\n", reason_number(reason), pkg.name));
        if pkg.info().reason == reason {
            return Ok(());
        }
        let mut info = pkg.info().clone();
        info.reason = reason;
        self.local_db_write(&pkg.name, &pkg.version, &info, None).map_err(|_| ErrorKind::DbWrite)?;
        let updated = Pkg::with_info(pkg.name.clone(), pkg.version.clone(), Origin::Local { dir: dir.clone() }, info, None);
        self.local.cache_replace(Arc::new(updated));
        Ok(())
    }

    /// Read a freshly written entry into the package cache.
    pub(crate) fn local_cache_add(&mut self, name: &str, version: &str) {
        let dir = std::path::PathBuf::from(format!("{}{}-{}", self.local.path.display(), name, version));
        self.local.cache_replace(Arc::new(Pkg::new(name.into(), version.into(), Origin::Local { dir })));
    }
}
