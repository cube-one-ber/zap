//! Package and file conflicts, as libalpm's conflict.c.
use super::deps::{DepMod, Depend};
use super::log::{self, Progress};
use super::package::{Files, Pkg};
use super::{util, Handle};
use std::sync::Arc;

/// Two packages that cannot be installed together.
#[derive(Clone, Debug)]
pub struct Conflict {
    pub package1: Arc<Pkg>,
    pub package2: Arc<Pkg>,
    /// The conflicts entry that matched.
    pub reason: Depend,
}

impl Conflict {
    /// Whether the reason says more than the second package's name.
    pub fn reason_is_new(&self) -> bool {
        self.reason.modifier != DepMod::Any
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileConflictKind {
    /// Two targets ship the same file.
    Target,
    /// A target would overwrite a file on disk.
    Filesystem,
}

/// A file a target would overwrite. `ctarget` is the other target, or the
/// installed owner (empty when unowned).
#[derive(Clone, Debug)]
pub struct FileConflict {
    pub target: String,
    pub kind: FileConflictKind,
    pub file: String,
    pub ctarget: String,
}

/// `check_conflict`: conflicts entries of `list1` matching packages of
/// `list2`, appended to `out` as (list1, list2), or reversed.
fn check_conflict(list1: &[Arc<Pkg>], list2: &[Arc<Pkg>], out: &mut Vec<Conflict>, reversed: bool) {
    for pkg1 in list1 {
        for conflict in &pkg1.info().conflicts {
            for pkg2 in list2 {
                if pkg1.name == pkg2.name || !conflict.satisfied_by(pkg2) {
                    continue;
                }
                let (a, b) = if reversed { (pkg2, pkg1) } else { (pkg1, pkg2) };
                if !out.iter().any(|c| c.package1.name == a.name && c.package2.name == b.name) {
                    log::debug(&format!("package {} conflicts with {} (by {conflict})\n", a.name, b.name));
                    out.push(Conflict { package1: a.clone(), package2: b.clone(), reason: conflict.clone() });
                }
            }
        }
    }
}

/// `_alpm_innerconflicts`: conflicts among the targets.
pub fn innerconflicts(packages: &[Arc<Pkg>]) -> Vec<Conflict> {
    log::debug("check targets vs targets\n");
    let mut out = Vec::new();
    check_conflict(packages, packages, &mut out, false);
    out
}

/// `_alpm_outerconflicts`: conflicts between the targets and installed
/// packages, each pair as (target, installed).
pub fn outerconflicts(local: &[Arc<Pkg>], packages: &[Arc<Pkg>]) -> Vec<Conflict> {
    let dblist: Vec<Arc<Pkg>> = local.iter().filter(|p| !packages.iter().any(|q| q.name == p.name)).cloned().collect();
    let mut out = Vec::new();
    log::debug("check targets vs db\n");
    check_conflict(packages, &dblist, &mut out, false);
    log::debug("check db vs targets\n");
    check_conflict(&dblist, packages, &mut out, true);
    out
}

/// `_alpm_filelist_pathcmp`: compare paths, a trailing '/' aside.
fn pathcmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let common = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let (ra, rb) = (&a[common..], &b[common..]);
    let (ra, rb) = match (ra, rb) {
        ([], [b'/', rest @ ..]) => (ra, rest),
        ([b'/', rest @ ..], []) => (rest, rb),
        _ => (ra, rb),
    };
    ra.cmp(rb)
}

/// `_alpm_filelist_intersection`: paths in both lists, unless both are directories.
fn intersection<'a>(a: &'a Files, b: &Files) -> Vec<&'a str> {
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < a.files.len() && j < b.files.len() {
        let (sa, sb) = (&a.files[i].name, &b.files[j].name);
        match pathcmp(sa, sb) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                if !sa.ends_with('/') || !sb.ends_with('/') {
                    out.push(sa.as_str());
                }
                i += 1;
                j += 1;
            }
        }
    }
    out
}

/// `_alpm_filelist_difference`: paths of `a` not in `b`.
fn difference<'a>(a: &'a Files, b: &Files) -> Vec<&'a str> {
    a.files.iter().map(|f| f.name.as_str()).filter(|name| b.contains(name).is_none()).collect()
}

/// `lstat` without following a final symlink even with a trailing slash.
pub(crate) fn llstat(path: &str) -> Option<std::fs::Metadata> {
    let trimmed = if path.len() > 1 { path.strip_suffix('/').unwrap_or(path) } else { path };
    std::fs::symlink_metadata(trimmed).ok()
}

/// The backup entry for `file` in `pkg`, as `_alpm_needbackup`.
pub(crate) fn needbackup<'a>(file: &str, pkg: &'a Pkg) -> Option<&'a super::package::Backup> {
    pkg.files().backup.iter().find(|b| b.name == file)
}

impl Handle {
    fn can_overwrite_file(&self, path: &str, rooted: &str) -> bool {
        util::fnmatch_patterns(&self.overwrite, path) == Some(true)
            || util::fnmatch_patterns(&self.overwrite, rooted) == Some(true)
    }

    /// `dir_belongsto_pkgs`: whether everything under `dirpath` is owned by `pkgs`.
    fn dir_belongsto_pkgs(&self, dirpath: &str, pkgs: &[Arc<Pkg>]) -> bool {
        let Ok(entries) = std::fs::read_dir(format!("{}{dirpath}", self.root)) else { return false };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let full = format!("{}{dirpath}{name}", self.root);
            let Ok(meta) = std::fs::symlink_metadata(&full) else {
                log::debug(&format!("could not stat {full}\n"));
                return false;
            };
            let path = format!("{dirpath}{name}{}", if meta.is_dir() { "/" } else { "" });
            let mut owned = pkgs.iter().any(|p| p.files().contains(&path).is_some());
            if owned && meta.is_dir() {
                owned = self.dir_belongsto_pkgs(&path, pkgs);
            }
            if !owned {
                log::debug(&format!("unowned file {path} found in directory\n"));
                return false;
            }
        }
        true
    }

    fn file_conflict(&self, file: &str, target: &Pkg, other: Option<&Pkg>) -> FileConflict {
        log::debug(&format!(
            "found file conflict {file}, packages {} and {}\n",
            target.name,
            other.map_or("(filesystem)", |p| p.name.as_str())
        ));
        let kind = match other {
            Some(p) if !p.is_local() => FileConflictKind::Target,
            _ => FileConflictKind::Filesystem,
        };
        FileConflict { target: target.name.clone(), kind, file: file.into(), ctarget: other.map(|p| p.name.clone()).unwrap_or_default() }
    }

    /// `_alpm_db_find_fileconflicts`: files the targets would overwrite,
    /// among themselves and on disk. Files that move between packages are
    /// recorded in the transaction's skip-remove list.
    pub(crate) fn find_fileconflicts(&mut self, upgrade: &[Arc<Pkg>], rem: &[Arc<Pkg>]) -> Vec<FileConflict> {
        let mut conflicts = Vec::new();
        let mut skip_remove = Vec::new();
        let count = upgrade.len();
        for (current, p1) in upgrade.iter().enumerate() {
            log::progress(Progress::ConflictsStart, "", (current * 100 / count.max(1)) as i32, count, current);
            log::debug(&format!("searching for file conflicts: {}\n", p1.name));
            for p2 in &upgrade[current + 1..] {
                for filename in intersection(p1.files(), p2.files()) {
                    let path = format!("{}{filename}", self.root);
                    if self.can_overwrite_file(filename, &path) && p2.files().contains(filename).is_some() {
                        log::debug(&format!("{filename} exists in both '{}' and '{}'\n", p1.name, p2.name));
                        log::debug("file-file conflict being forced\n");
                        continue;
                    }
                    conflicts.push(self.file_conflict(&path, p1, Some(p2)));
                }
            }

            log::debug(&format!("searching for filesystem conflicts: {}\n", p1.name));
            let dbpkg = self.local.get(&p1.name).cloned();
            let newfiles: Vec<&str> = match &dbpkg {
                Some(db) => difference(p1.files(), db.files()),
                None => p1.files().files.iter().map(|f| f.name.as_str()).collect(),
            };
            // Skip the files under a directory that replaces a file.
            let skip_under = |j: &mut usize, filestr: &str| {
                while *j + 1 < newfiles.len() && newfiles[*j + 1].starts_with(filestr) {
                    *j += 1;
                }
            };
            let mut j = 0;
            while j < newfiles.len() {
                let filestr = newfiles[j];
                let mut path = format!("{}{filestr}", self.root);
                let Some(meta) = llstat(&path) else {
                    j += 1;
                    continue;
                };
                log::debug(&format!("checking possible conflict: {path}\n"));
                let mut resolved = false;
                let pfile_isdir = path.ends_with('/');
                if pfile_isdir {
                    if meta.is_dir() {
                        log::debug("file is a directory, not a conflict\n");
                        j += 1;
                        continue;
                    }
                    // Compare without the slash so a directory may replace a file.
                    path.pop();
                    let relative = &path[self.root.len()..];
                    if dbpkg.as_ref().is_some_and(|db| db.files().contains(relative).is_some()) {
                        log::debug("replacing package file with a directory, not a conflict\n");
                        resolved = true;
                        skip_under(&mut j, filestr);
                    }
                }
                let relative = path[self.root.len()..].to_string();
                if !resolved && rem.iter().any(|r| r.files().contains(&relative).is_some()) {
                    log::debug("local file will be removed, not a conflict\n");
                    resolved = true;
                    if pfile_isdir {
                        skip_under(&mut j, filestr);
                    }
                }
                if !resolved {
                    for p2 in upgrade {
                        if Arc::ptr_eq(p1, p2) {
                            continue;
                        }
                        let moved = self.local.get(&p2.name).is_some_and(|l| l.files().contains(&relative).is_some());
                        if moved {
                            skip_remove.push(relative.clone());
                            log::debug("file changed packages, adding to remove skiplist\n");
                            resolved = true;
                            if filestr.ends_with('/') {
                                skip_under(&mut j, filestr);
                            }
                            break;
                        }
                    }
                }
                if !resolved && meta.is_dir() {
                    let dir = format!("{relative}/");
                    let owners: Vec<Arc<Pkg>> =
                        self.local.pkgs().iter().filter(|p| p.files().contains(&dir).is_some()).cloned().collect();
                    if !owners.is_empty() {
                        let removed = |p: &Pkg| dbpkg.as_ref().is_some_and(|d| d.name == p.name) || rem.iter().any(|r| r.name == p.name);
                        if owners.iter().all(|o| removed(o)) {
                            log::debug(&format!("checking if all files in {dir} belong to removed packages\n"));
                            resolved = self.dir_belongsto_pkgs(&dir, &owners);
                        }
                    }
                }
                if !resolved && needbackup(&relative, p1).is_some() {
                    let owned = self.local.pkgs().iter().any(|p| p.files().contains(&relative).is_some());
                    if !owned {
                        log::debug("file was unowned but in new backup list\n");
                        resolved = true;
                    }
                }
                if !meta.is_dir() && self.can_overwrite_file(filestr, &path) {
                    log::debug("conflict with file on filesystem being forced\n");
                    resolved = true;
                }
                if !resolved {
                    let owner = self.local.pkgs().iter().find(|p| p.files().contains(&relative).is_some()).cloned();
                    conflicts.push(self.file_conflict(&path, p1, owner.as_deref()));
                }
                j += 1;
            }
        }
        log::progress(Progress::ConflictsStart, "", 100, count, count);
        self.trans_mut().skip_remove.extend(skip_remove);
        conflicts
    }
}
