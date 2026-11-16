//! `CheckSpace`, as libalpm's diskspace.c: before downloading, the cache's
//! filesystem must hold the packages; before committing, each filesystem a
//! transaction writes to must have room for its largest moment, plus a
//! cushion.
use super::log::{self, Progress};
use super::package::Pkg;
use super::{ErrorKind, Handle, Result};

const USED_REMOVE: u8 = 1;
const USED_INSTALL: u8 = 2;

#[derive(PartialEq)]
enum FsInfo {
    Unloaded,
    Loaded,
    Failed,
}

struct MountPoint {
    dir: String,
    fsinfo: FsInfo,
    block_size: u64,
    blocks: u64,
    available: u64,
    read_only: bool,
    blocks_needed: i64,
    max_blocks_needed: i64,
    used: u8,
}

impl MountPoint {
    /// `mount_point_load_fsinfo`, lazily: false when statvfs fails.
    fn load(&mut self) -> bool {
        match self.fsinfo {
            FsInfo::Loaded => return true,
            FsInfo::Failed => return false,
            FsInfo::Unloaded => {}
        }
        let Ok(path) = std::ffi::CString::new(self.dir.as_str()) else {
            self.fsinfo = FsInfo::Failed;
            return false;
        };
        let mut fs: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut fs) } != 0 {
            let why = super::strerror(&std::io::Error::last_os_error());
            log::warning(&format!("could not get filesystem information for {}: {why}\n", self.dir));
            self.fsinfo = FsInfo::Failed;
            return false;
        }
        log::debug(&format!("loading fsinfo for {}\n", self.dir));
        self.block_size = fs.f_bsize.max(1);
        self.blocks = fs.f_blocks;
        self.available = fs.f_bavail;
        self.read_only = fs.f_flag & libc::ST_RDONLY != 0;
        self.fsinfo = FsInfo::Loaded;
        true
    }

    /// `check_mountpoint`: false when the filesystem is too full. The
    /// cushion is roughly min(5% capacity, 20MiB).
    fn check(&self) -> bool {
        let five_percent = self.blocks / 20 + 1;
        let twenty_mib = 20 * 1024 * 1024 / self.block_size + 1;
        let cushion = five_percent.min(twenty_mib);
        let needed = self.max_blocks_needed + cushion as i64;
        log::debug(&format!(
            "partition {}, needed {}, cushion {cushion}, free {}\n",
            self.dir, self.max_blocks_needed, self.available
        ));
        if needed >= 0 && needed as u64 > self.available {
            log::error(&format!("Partition {} too full: {needed} blocks needed, {} blocks free\n", self.dir, self.available));
            return false;
        }
        true
    }
}

/// Undo the octal escapes the mount table uses for spaces and the like.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() && bytes[i + 1..i + 4].iter().all(|b| (b'0'..=b'7').contains(b)) {
            out.push((bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `mount_point_list`: every mount point, children before their parents.
fn mount_points() -> Option<Vec<MountPoint>> {
    let table = match std::fs::read_to_string("/proc/self/mounts") {
        Ok(table) => table,
        Err(e) => {
            log::error(&format!("could not open file: /proc/self/mounts: {}\n", super::strerror(&e)));
            return None;
        }
    };
    let mut points: Vec<MountPoint> = table
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .map(|dir| MountPoint {
            dir: unescape(dir),
            fsinfo: FsInfo::Unloaded,
            block_size: 1,
            blocks: 0,
            available: 0,
            read_only: false,
            blocks_needed: 0,
            max_blocks_needed: 0,
            used: 0,
        })
        .collect();
    // Reverse byte order puts each mount point before its parent. The
    // sort is stable, so of duplicate entries the first mounted stays first.
    points.sort_by(|a, b| b.dir.as_bytes().cmp(a.dir.as_bytes()));
    for point in &points {
        log::debug(&format!("discovered mountpoint: {}\n", point.dir));
    }
    Some(points)
}

/// `match_mount_point`: the mount point holding `path`.
fn match_mount_point(points: &[MountPoint], path: &str) -> Option<usize> {
    points.iter().position(|p| {
        path.starts_with(&p.dir) && (p.dir.ends_with('/') || matches!(path.as_bytes().get(p.dir.len()), None | Some(b'/')))
    })
}

fn ceil_blocks(size: i64, block_size: u64) -> i64 {
    (size + block_size as i64 - 1) / block_size as i64
}

impl Handle {
    /// `calculate_removed_size`: the blocks freed by removing `pkg`'s files.
    fn removed_size(&self, points: &mut [MountPoint], pkg: &Pkg) {
        for file in &pkg.files().files {
            let path = format!("{}{}", self.root, file.name);
            let meta = match std::fs::symlink_metadata(path.trim_end_matches('/')) {
                Ok(meta) => meta,
                Err(_) => {
                    if super::util::fnmatch_patterns(&self.noextract, &file.name) == Some(true) {
                        log::warning(&format!("could not get file information for {}\n", file.name));
                    }
                    continue;
                }
            };
            // Directories and symlinks count as nothing, as in archives.
            if meta.is_dir() || meta.file_type().is_symlink() {
                continue;
            }
            let Some(i) = match_mount_point(points, &path) else {
                log::warning(&format!("could not determine mount point for file {}\n", file.name));
                continue;
            };
            let point = &mut points[i];
            if !point.load() {
                continue;
            }
            point.blocks_needed -= ceil_blocks(meta.len() as i64, point.block_size);
            point.used |= USED_REMOVE;
        }
    }

    /// `calculate_installed_size`: the blocks `pkg`'s files will take.
    fn installed_size(&self, points: &mut [MountPoint], pkg: &Pkg) {
        for file in &pkg.files().files {
            let kind = file.mode & libc::S_IFMT;
            if kind == libc::S_IFDIR || kind == libc::S_IFLNK {
                continue;
            }
            // Database entries (.PKGINFO and the like) approximate the
            // space their database entry takes.
            let name = if file.name.starts_with('.') { &self.dbpath } else { &file.name };
            let path = format!("{}{name}", self.root);
            let Some(i) = match_mount_point(points, &path) else {
                log::warning(&format!("could not determine mount point for file {name}\n"));
                continue;
            };
            let point = &mut points[i];
            if !point.load() {
                continue;
            }
            point.blocks_needed += ceil_blocks(file.size, point.block_size);
            point.used |= USED_INSTALL;
        }
    }

    /// `_alpm_check_downloadspace`: room in the cache for `sizes` bytes.
    pub(crate) fn check_downloadspace(&self, cachedir: &str, sizes: &[i64]) -> Result<()> {
        let resolved = std::fs::canonicalize(cachedir).map_or_else(|_| cachedir.to_string(), |p| p.to_string_lossy().into_owned());
        let Some(mut points) = mount_points() else {
            log::error("could not determine filesystem mount points\n");
            return Err(ErrorKind::DiskSpace.into());
        };
        let Some(i) = match_mount_point(&points, &resolved) else {
            log::error(&format!("could not determine cachedir mount point {resolved}\n"));
            return Err(ErrorKind::DiskSpace.into());
        };
        let point = &mut points[i];
        if !point.load() {
            return Err(ErrorKind::DiskSpace.into());
        }
        // As libalpm, which rounds with `+ bsize + 1` here.
        for size in sizes {
            point.max_blocks_needed += (size + point.block_size as i64 + 1) / point.block_size as i64;
        }
        if point.check() {
            Ok(())
        } else {
            Err(ErrorKind::DiskSpace.into())
        }
    }

    /// `_alpm_check_diskspace`: room on every filesystem the transaction
    /// writes to, at its fullest moment.
    pub(crate) fn check_diskspace(&self) -> Result<()> {
        let Some(mut points) = mount_points() else {
            log::error("could not determine filesystem mount points\n");
            return Err(ErrorKind::DiskSpace.into());
        };
        if match_mount_point(&points, &self.root).is_none() {
            log::error(&format!("could not determine root mount point {}\n", self.root));
            return Err(ErrorKind::DiskSpace.into());
        }
        let trans = self.trans.as_ref().expect("a transaction");
        let total = trans.add.len() + trans.remove.len();
        let mut current = 0;
        for pkg in &trans.remove {
            log::progress(Progress::DiskspaceStart, "", (current * 100 / total) as i32, total, current);
            self.removed_size(&mut points, pkg);
            current += 1;
        }
        for add in &trans.add {
            log::progress(Progress::DiskspaceStart, "", (current * 100 / total) as i32, total, current);
            if let Some(local) = self.local.get(&add.pkg.name) {
                self.removed_size(&mut points, local);
            }
            self.installed_size(&mut points, &add.pkg);
            for point in points.iter_mut() {
                point.max_blocks_needed = point.max_blocks_needed.max(point.blocks_needed);
            }
            current += 1;
        }
        log::progress(Progress::DiskspaceStart, "", 100, total, current);
        let mut ok = true;
        for point in &points {
            if point.used != 0 && point.read_only {
                log::error(&format!("Partition {} is mounted read only\n", point.dir));
                ok = false;
            } else if point.used & USED_INSTALL != 0 && !point.check() {
                ok = false;
            }
        }
        if ok {
            Ok(())
        } else {
            Err(ErrorKind::DiskSpace.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(dir: &str) -> MountPoint {
        MountPoint {
            dir: dir.into(),
            fsinfo: FsInfo::Loaded,
            block_size: 4096,
            blocks: 1000,
            available: 100,
            read_only: false,
            blocks_needed: 0,
            max_blocks_needed: 0,
            used: 0,
        }
    }

    #[test]
    fn files_map_to_their_deepest_mount_point_by_whole_components() {
        let points = vec![point("/var/cache"), point("/var"), point("/")];
        assert_eq!(Some(0), match_mount_point(&points, "/var/cache/pacman/pkg/"));
        assert_eq!(Some(1), match_mount_point(&points, "/var/cachet/file"));
        assert_eq!(Some(1), match_mount_point(&points, "/var"));
        assert_eq!(Some(2), match_mount_point(&points, "/usr/bin/ls"));
        assert_eq!("/mnt/my disk", unescape("/mnt/my\\040disk"));
    }

    #[test]
    fn a_partition_needs_its_peak_usage_plus_a_cushion() {
        // A 1000-block filesystem's cushion is 5% + 1 = 51 blocks.
        let mut p = point("/");
        p.max_blocks_needed = 49;
        assert!(p.check());
        p.max_blocks_needed = 50;
        assert!(!p.check());
    }
}
