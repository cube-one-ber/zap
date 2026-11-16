//! Fixtures shared by unit tests. Nothing here touches the host database.
use crate::alpm::Alpm;
use crate::config::Config;
use crate::libalpm::Handle;
use std::path::{Path, PathBuf};

/// A scratch directory removed when dropped.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> TempDir {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("zap-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn str(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// An installed package `name 1.0-1` recorded in `root/db/local`, owning
/// `files` (newline-separated, as the files entry lists them).
pub fn installed_fixture(root: &Path, name: &str, files: &str) {
    let dir = root.join(format!("db/local/{name}-1.0-1"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(root.join("db/local/ALPM_DB_VERSION"), "9\n").unwrap();
    let desc = format!(
        "%NAME%\n{name}\n\n%VERSION%\n1.0-1\n\n%DESC%\nInstalled fixture\n\n%ARCH%\nany\n\n%SIZE%\n1\n\n%REASON%\n1\n\n"
    );
    std::fs::write(dir.join("desc"), desc).unwrap();
    std::fs::write(dir.join("files"), format!("%FILES%\n{files}\n\n")).unwrap();
}

/// A handle on `root` with its database at `root/db` and no repositories.
pub fn alpm(root: &Path) -> Alpm {
    let db = root.join("db");
    std::fs::create_dir_all(&db).unwrap();
    let mut h = Handle::new(root.to_str().unwrap(), db.to_str().unwrap()).unwrap();
    h.architectures = vec!["any".into()];
    let mut cfg = Config::default();
    cfg.root = root.to_str().unwrap().into();
    Alpm { h, cfg }
}

/// A minimal package archive `root/{name}-1.0-1-any.pkg.tar` holding only a
/// `.PKGINFO`, with `extra` appended to it.
pub fn package_fixture(root: &Path, name: &str, extra: &str) -> String {
    let info = format!("pkgname = {name}\npkgver = 1.0-1\npkgdesc = isolated transaction fixture\nurl = https://example.org\nbuilddate = 1750000000\npackager = zap tests\nsize = 1\narch = any\n{extra}");
    std::fs::write(root.join(".PKGINFO"), info).unwrap();
    let path = root.join(format!("{name}-1.0-1-any.pkg.tar")).to_string_lossy().into_owned();
    crate::util::run(&["/usr/bin/bsdtar", "-cf", &path, "-C", root.to_str().unwrap(), ".PKGINFO"], None).unwrap();
    path
}
