//! Package archives on disk, as `alpm_pkg_load` with no signature checks:
//! `.PKGINFO` metadata, the file list from `.MTREE` (or from the archive
//! itself when there is none) and whether an install scriptlet is present.
//!
//! Formats: tar, plain or compressed with gzip, zstd, xz, bzip2, lz4, lzip
//! or compress (.Z), and lrzip or lzop through their programs, as
//! libarchive on Arch reads them.
use super::deps::Depend;
use super::package::{sort_files, Backup, File, Files, Info, Origin, Pkg};
use super::{log, ErrorKind, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

const S_IFMT: u32 = libc::S_IFMT;

/// Load the package archive at `path`. With `full`, the file list is built
/// too, which for archives without an mtree means reading every entry.
pub fn load(path: &str, full: bool) -> Result<Pkg> {
    if path.is_empty() {
        return Err(ErrorKind::WrongArgs.into());
    }
    let (file, size) = open(path)?;
    let reader = match decoder(BufReader::with_capacity(64 * 1024, file)) {
        Ok(Some(reader)) => reader,
        Ok(None) => {
            log::error(&format!("could not open file {path}: Unrecognized archive format\n"));
            return Err(ErrorKind::PkgOpen.into());
        }
        Err(e) => {
            log::error(&format!("could not open file {path}: {}\n", super::strerror(&e)));
            return Err(ErrorKind::PkgOpen.into());
        }
    };
    let invalid = |why: &str| {
        log::error(&format!("{why} in {path}\n"));
        Err(ErrorKind::PkgInvalid.into())
    };
    let read_error = |e: std::io::Error| {
        log::error(&format!("error while reading package {path}: {}\n", super::strerror(&e)));
        super::Error::from(ErrorKind::Libarchive)
    };

    let mut archive = tar::Archive::new(reader);
    let mut desc: Option<Desc> = None;
    let mut files = Files::default();
    let mut scriptlet = false;
    let mut hit_mtree = false;
    for entry in archive.entries().map_err(read_error)? {
        let mut entry = entry.map_err(read_error)?;
        let name = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if name == ".PKGINFO" {
            let mut text = Vec::new();
            entry.read_to_end(&mut text).map_err(read_error)?;
            // libalpm reads .PKGINFO with a 512 KiB line buffer.
            if text.split(|&b| b == b'\n').any(|line| line.len() + 1 > 512 * 1024) {
                log::debug("error parsing package descfile\n");
                log::error(&format!("could not parse package description file in {path}\n"));
                return Err(ErrorKind::PkgInvalid.into());
            }
            let parsed = parse_pkginfo(&text);
            if parsed.name.is_empty() {
                return invalid("missing package name");
            }
            if parsed.version.is_empty() {
                return invalid("missing package version");
            }
            if !parsed.version.contains('-') {
                return invalid("invalid package version");
            }
            desc = Some(parsed);
            continue;
        } else if full && name == ".MTREE" {
            let mut data = Vec::new();
            entry.read_to_end(&mut data).map_err(read_error)?;
            if let Some(list) = mtree_files(&data, &mut scriptlet) {
                files.files = list;
                hit_mtree = true;
            }
            continue;
        } else if simple_path(&name, &mut scriptlet) {
            continue;
        } else if full && !hit_mtree {
            files.files.push(archive_file(&entry, name));
        }
        if (!full || hit_mtree) && desc.is_some() {
            break;
        }
    }
    let Some(mut desc) = desc else { return invalid("missing package metadata") };
    if full {
        sort_files(&mut files.files);
    }
    if !super::db::check_meta(&desc.name, &desc.version) {
        return Err(ErrorKind::PkgInvalid.into());
    }
    files.backup = std::mem::take(&mut desc.backup);
    desc.info.size = size;
    desc.info.has_scriptlet = scriptlet;
    desc.info.validation = super::package::VALIDATION_NONE;
    let origin = Origin::File { path: path.into() };
    Ok(Pkg::with_info(desc.name, desc.version, origin, desc.info, Some(files)))
}

/// `_alpm_unpack_single`: extract the entry `name` of the archive at `path`
/// to the file `dest`.
pub fn unpack_single(path: &str, name: &str, dest: &str) -> bool {
    let Ok((file, _)) = open(path) else { return false };
    let Ok(Some(reader)) = decoder(BufReader::with_capacity(64 * 1024, file)) else { return false };
    let mut archive = tar::Archive::new(reader);
    let Ok(entries) = archive.entries() else { return false };
    for entry in entries {
        let Ok(mut entry) = entry else { return false };
        if entry.path_bytes().as_ref() == name.as_bytes() {
            let Ok(mut out) = std::fs::File::create(dest) else { return false };
            return std::io::copy(&mut entry, &mut out).is_ok();
        }
    }
    false
}

/// Counts the compressed bytes read, for extraction progress.
struct Counting {
    inner: std::fs::File,
    read: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

/// A tar stream and a counter of the compressed bytes consumed so far.
pub(crate) type TarStream = (tar::Archive<Box<dyn Read>>, std::sync::Arc<std::sync::atomic::AtomicU64>);

/// `_alpm_open_archive`: the archive at `path` as a tar stream.
pub(crate) fn open_tar(path: &str) -> Result<TarStream> {
    let (file, _) = open(path)?;
    let read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counting = Counting { inner: file, read: read.clone() };
    match decoder(BufReader::with_capacity(64 * 1024, counting)) {
        Ok(Some(reader)) => Ok((tar::Archive::new(reader), read)),
        Ok(None) => {
            log::error(&format!("could not open file {path}: Unrecognized archive format\n"));
            Err(ErrorKind::PkgOpen.into())
        }
        Err(e) => {
            log::error(&format!("could not open file {path}: {}\n", super::strerror(&e)));
            Err(ErrorKind::PkgOpen.into())
        }
    }
}

/// Open for reading, mapping failures to libalpm's errors.
fn open(path: &str) -> Result<(std::fs::File, i64)> {
    let failed = |e: std::io::Error| {
        log::error(&format!("could not open file {path}: {}\n", super::strerror(&e)));
        let kind = match e.raw_os_error() {
            Some(libc::ENOENT) => ErrorKind::PkgNotFound,
            Some(libc::EACCES) => ErrorKind::BadPerms,
            _ => ErrorKind::PkgOpen,
        };
        super::Error::from(kind)
    };
    let file = std::fs::File::open(Path::new(path)).map_err(failed)?;
    let size = file.metadata().map_err(failed)?.len() as i64;
    Ok((file, size))
}

/// A tar stream over the decompressed contents, or `None` when the data is
/// neither tar nor a compressed tar.
fn decoder<R: BufRead + Send + 'static>(mut input: R) -> std::io::Result<Option<Box<dyn Read>>> {
    use super::compress::{Lzw, Program};
    let head = input.fill_buf()?;
    let mut reader: Box<dyn BufRead> = if head.starts_with(&[0x1f, 0x8b]) {
        Box::new(BufReader::new(flate2::bufread::MultiGzDecoder::new(input)))
    } else if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        Box::new(BufReader::new(zstd::stream::read::Decoder::with_buffer(input)?))
    } else if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
        Box::new(BufReader::new(liblzma::bufread::XzDecoder::new_multi_decoder(input)))
    } else if head.starts_with(b"BZh") {
        Box::new(BufReader::new(bzip2::bufread::MultiBzDecoder::new(input)))
    } else if head.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
        Box::new(BufReader::new(lz4_flex::frame::FrameDecoder::new(input)))
    } else if head.starts_with(b"LZIP") {
        let stream = liblzma::stream::Stream::new_lzip_decoder(u64::MAX, liblzma::stream::CONCATENATED)?;
        Box::new(BufReader::new(liblzma::bufread::XzDecoder::new_stream(input, stream)))
    } else if head.starts_with(&[0x1f, 0x9d]) {
        Box::new(BufReader::new(Lzw::new(input)?))
    } else if head.starts_with(b"LRZI") {
        Box::new(BufReader::new(Program::spawn("lrzip", &["-d", "-q"], input)?))
    } else if head.starts_with(&[0x89, b'L', b'Z', b'O', 0, b'\r', b'\n', 0x1a, b'\n']) {
        Box::new(BufReader::new(Program::spawn("lzop", &["-d"], input)?))
    } else {
        Box::new(input)
    };
    // Peek at the first header block. An empty stream is an empty archive.
    let mut block = Vec::with_capacity(512);
    (&mut reader).take(512).read_to_end(&mut block)?;
    if !(block.is_empty() || is_tar_header(&block)) {
        return Ok(None);
    }
    Ok(Some(Box::new(std::io::Cursor::new(block).chain(reader))))
}

/// A complete header block with a valid checksum, or the zero block that
/// ends an empty archive.
fn is_tar_header(block: &[u8]) -> bool {
    if block.len() < 512 {
        return false;
    }
    if block.iter().all(|&b| b == 0) {
        return true;
    }
    let stored = super::db::octal(&block[148..156]);
    let sum: usize = block.iter().enumerate().map(|(i, &b)| if (148..156).contains(&i) { b' ' as usize } else { b as usize }).sum();
    stored == Some(sum)
}

/// `handle_simple_path`: `.INSTALL` marks a scriptlet, and other dot-files
/// at the top level are package metadata, not files.
fn simple_path(path: &str, scriptlet: &mut bool) -> bool {
    if path == ".INSTALL" {
        *scriptlet = true;
    }
    path.starts_with('.')
}

/// A file list entry from a tar header. Directories always end in '/'.
fn archive_file<R: Read>(entry: &tar::Entry<R>, mut name: String) -> File {
    let header = entry.header();
    let kind = header.entry_type();
    let dir = kind.is_dir() || (kind.is_file() && name.ends_with('/'));
    let file_type = if dir {
        libc::S_IFDIR
    } else if kind.is_symlink() {
        libc::S_IFLNK
    } else if kind.is_character_special() {
        libc::S_IFCHR
    } else if kind.is_block_special() {
        libc::S_IFBLK
    } else if kind.is_fifo() {
        libc::S_IFIFO
    } else {
        libc::S_IFREG
    };
    if dir && !name.ends_with('/') {
        name.push('/');
    }
    let perm = header.mode().unwrap_or(0) & 0o7777;
    File { name, size: entry.size() as i64, mode: file_type | perm }
}

// ── .PKGINFO ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Desc {
    name: String,
    version: String,
    info: Info,
    backup: Vec<Backup>,
}

/// `parse_descfile`: one `key = value` per line. Lines in another shape are
/// skipped; unknown keys are warned about.
fn parse_pkginfo(bytes: &[u8]) -> Desc {
    let text = String::from_utf8_lossy(bytes);
    let mut d = Desc::default();
    for line in text.split('\n') {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.find(' ').and_then(|at| Some((&line[..at], line[at..].strip_prefix(" = ")?))) else {
            continue;
        };
        let dep = || Depend::parse(value);
        let info = &mut d.info;
        match key {
            "pkgname" => d.name = value.into(),
            "pkgbase" => info.base = value.into(),
            "pkgver" => d.version = value.into(),
            "basever" => {}
            "pkgdesc" => info.desc = value.into(),
            "group" => info.groups.push(value.into()),
            "url" => info.url = value.into(),
            "license" => info.licenses.push(value.into()),
            "builddate" => info.builddate = super::package::parsedate(value),
            "packager" => info.packager = value.into(),
            "arch" => info.arch = value.into(),
            "size" => info.isize = super::package::strtoofft(value),
            "depend" => info.depends.push(dep()),
            "optdepend" => info.optdepends.push(dep()),
            "makedepend" => info.makedepends.push(dep()),
            "checkdepend" => info.checkdepends.push(dep()),
            "conflict" => info.conflicts.push(dep()),
            "replaces" => info.replaces.push(dep()),
            "provides" => info.provides.push(dep()),
            "backup" => d.backup.push(Backup { name: value.into(), hash: String::new() }),
            "xdata" => match value.split_once('=') {
                Some((k, v)) => info.xdata.push((k.into(), v.into())),
                // libalpm fails the whole description; an empty name makes
                // the caller report it the same way.
                None => {
                    d.name.clear();
                    return d;
                }
            },
            _ => {
                let name = if d.name.is_empty() { "error" } else { &d.name };
                log::warning(&format!("{name}: unknown key '{key}' in package description\n"));
            }
        }
    }
    d
}

// ── .MTREE ──────────────────────────────────────────────────────────────────

/// The file list from a (usually gzipped) mtree, as libarchive's mtree
/// reader and `build_filelist_from_mtree` produce it, or `None` when it does
/// not parse and the archive itself must be listed instead.
///
/// libarchive also stats each path relative to the working directory to fill
/// in unspecified sizes; makepkg's mtrees specify everything that matters.
fn mtree_files(data: &[u8], scriptlet: &mut bool) -> Option<Vec<File>> {
    let text = super::db::decompress(data).ok()?;
    if !text.starts_with(b"#mtree") {
        return None;
    }
    let text = String::from_utf8_lossy(&text);
    // Keyword defaults from /set.
    let (mut set_type, mut set_mode, mut set_size) = (None, None, None);
    let mut list = Vec::new();
    let mut logical = String::new();
    for raw in text.split('\n') {
        // A trailing backslash continues the line.
        if let Some(head) = raw.strip_suffix('\\') {
            logical.push_str(head);
            continue;
        }
        logical.push_str(raw);
        let line = std::mem::take(&mut logical);
        let mut words = line.split([' ', '\t']).filter(|w| !w.is_empty());
        let Some(first) = words.next() else { continue };
        if first.starts_with('#') {
            continue;
        }
        if first == "/set" || first == "/unset" {
            for word in words {
                let (key, value) = word.split_once('=').unwrap_or((word, ""));
                let set = first == "/set";
                match key {
                    "type" => set_type = set.then(|| file_type(value)).flatten(),
                    "mode" => set_mode = if set { Some(octal_mode(value)?) } else { None },
                    "size" => set_size = set.then(|| number(value)),
                    "all" if !set => (set_type, set_mode, set_size) = (None, None, None),
                    _ => {}
                }
            }
            continue;
        }
        let (mut kind, mut mode, mut size) = (set_type, set_mode, set_size);
        for word in words {
            let (key, value) = word.split_once('=').unwrap_or((word, ""));
            match key {
                "type" => kind = Some(file_type(value)?),
                "mode" => mode = Some(octal_mode(value)?),
                "size" => size = Some(number(value)),
                _ => {}
            }
        }
        let path = unescape(first);
        let path = path.strip_prefix("./").unwrap_or(&path);
        if simple_path(path, scriptlet) {
            continue;
        }
        let kind = kind.unwrap_or(libc::S_IFREG);
        let mut name = path.to_string();
        if kind == libc::S_IFDIR && !name.ends_with('/') {
            name.push('/');
        }
        list.push(File { name, size: size.unwrap_or(0), mode: kind | mode.unwrap_or(0) });
    }
    Some(list)
}

fn file_type(value: &str) -> Option<u32> {
    Some(match value {
        "file" => libc::S_IFREG,
        "dir" => libc::S_IFDIR,
        "link" => libc::S_IFLNK,
        "block" => libc::S_IFBLK,
        "char" => libc::S_IFCHR,
        "fifo" => libc::S_IFIFO,
        "socket" => libc::S_IFSOCK,
        _ => return None,
    })
}

/// A decimal number, read up to the first non-digit as `mtree_atol`.
fn number(value: &str) -> i64 {
    value.bytes().take_while(u8::is_ascii_digit).fold(0i64, |n, d| n.saturating_mul(10).saturating_add((d - b'0') as i64))
}

/// libarchive reads only octal modes and refuses symbolic ones.
fn octal_mode(value: &str) -> Option<u32> {
    if !value.starts_with(|c: char| ('0'..='7').contains(&c)) {
        return None;
    }
    let digits = value.bytes().take_while(|b| (b'0'..=b'7').contains(b)).count();
    Some(u32::from_str_radix(&value[..digits], 8).ok()? & !S_IFMT)
}

/// Undo mtree's vis-style escapes, as libarchive's `parse_escapes`.
fn unescape(word: &str) -> String {
    let src = word.as_bytes();
    let mut out = Vec::with_capacity(src.len());
    let mut i = 0;
    let is_octal = |b: Option<&u8>| b.is_some_and(|b| (b'0'..=b'7').contains(b));
    while i < src.len() {
        let mut c = src[i];
        i += 1;
        if c == b'\\' {
            match src.get(i) {
                Some(b'0') if !is_octal(src.get(i + 1)) => {
                    c = 0;
                    i += 1;
                }
                Some(b'0'..=b'3') if is_octal(src.get(i + 1)) && is_octal(src.get(i + 2)) => {
                    c = (src[i] - b'0') << 6 | (src[i + 1] - b'0') << 3 | (src[i + 2] - b'0');
                    i += 3;
                }
                Some(&e @ (b'a' | b'b' | b'f' | b'n' | b'r' | b's' | b't' | b'v' | b'\\')) => {
                    c = match e {
                        b'a' => 7,
                        b'b' => 8,
                        b'f' => 12,
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b's' => b' ',
                        b't' => b'\t',
                        b'v' => 11,
                        _ => b'\\',
                    };
                    i += 1;
                }
                _ => {}
            }
        }
        // C strings end at the first NUL.
        if c == 0 {
            break;
        }
        out.push(c);
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    /// Build a package with bsdtar from `files`, which are written into a
    /// staging directory first.
    fn package(dir: &Path, name: &str, files: &[(&str, &str)], options: &[&str]) -> String {
        let stage = dir.join(format!("{name}-stage"));
        for (path, content) in files {
            let target = stage.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            if path.ends_with('/') {
                std::fs::create_dir_all(&target).unwrap();
            } else {
                std::fs::write(&target, content).unwrap();
            }
        }
        let out = dir.join(format!("{name}.pkg.tar")).to_string_lossy().into_owned();
        let mut args = vec!["/usr/bin/bsdtar", "-cf", &out];
        args.extend_from_slice(options);
        let stage = stage.to_string_lossy().into_owned();
        args.extend_from_slice(&["-C", &stage]);
        args.extend(files.iter().map(|(p, _)| p.trim_end_matches('/')).filter(|p| !p.contains('/')));
        crate::util::run(&args, None).unwrap();
        out
    }

    const PKGINFO: &str = "# generated\npkgname = foo\npkgbase = foo-base\npkgver = 1.0-1\npkgdesc = a = b\nsize = 42\nbuilddate = 1750000000\ndepend = bar>=2\nbackup = etc/foo.conf\nxdata = pkgtype=pkg\nbogus = 1\n";

    #[test]
    fn archives_without_an_mtree_list_their_entries_and_compressed_ones_load_alike() {
        let tmp = TempDir::new();
        let files = [(".PKGINFO", PKGINFO), (".INSTALL", ""), ("usr/", ""), ("usr/bin/", ""), ("usr/bin/foo", "binary")];
        let formats = [&[][..], &["--zstd"], &["-z"], &["--xz"], &["-j"], &["--lz4"], &["--lzip"], &["-Z"]];
        for (i, options) in formats.into_iter().enumerate() {
            let path = package(tmp.path(), &format!("foo{i}"), &files, options);
            let pkg = load(&path, true).unwrap();
            assert_eq!(("foo", "1.0-1"), (pkg.name.as_str(), pkg.version.as_str()));
            let info = pkg.info();
            assert_eq!(("foo-base", "a = b", 42, 1750000000), (info.base.as_str(), info.desc.as_str(), info.isize, info.builddate));
            assert_eq!("bar", info.depends[0].name);
            assert_eq!(vec![("pkgtype".to_string(), "pkg".to_string())], info.xdata);
            assert!(info.has_scriptlet);
            let names: Vec<_> = pkg.files().files.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(vec!["usr/", "usr/bin/", "usr/bin/foo"], names);
            assert_eq!(libc::S_IFREG, pkg.files().files[2].mode & S_IFMT);
            assert_eq!(6, pkg.files().files[2].size);
            assert_eq!("etc/foo.conf", pkg.files().backup[0].name);
        }
    }

    #[test]
    fn an_mtree_replaces_the_archive_listing() {
        let tmp = TempDir::new();
        let mtree = "#mtree\n/set type=file uid=0 gid=0 mode=644\n./.BUILDINFO size=1\n./.INSTALL size=1\n./usr time=1.0 mode=755 type=dir\n./usr/my\\040file size=3 \\\n  sha256digest=00\n/unset all\n./usr/link type=link link=x\n";
        let stage = tmp.path().join("mtree");
        std::fs::write(&stage, mtree).unwrap();
        crate::util::run(&["/usr/bin/gzip", "-n", stage.to_str().unwrap()], None).unwrap();
        let gz = std::fs::read(tmp.path().join("mtree.gz")).unwrap();
        let mut scriptlet = false;
        let files = mtree_files(&gz, &mut scriptlet).unwrap();
        assert!(scriptlet);
        let listed: Vec<_> = files.iter().map(|f| (f.name.as_str(), f.size, f.mode)).collect();
        assert_eq!(vec![("usr/", 0, libc::S_IFDIR | 0o755), ("usr/my file", 3, libc::S_IFREG | 0o644), ("usr/link", 0, libc::S_IFLNK)], listed);
        assert!(mtree_files(b"not an mtree", &mut scriptlet).is_none());
        assert!(mtree_files(b"#mtree\n./x mode=u+x\n", &mut scriptlet).is_none());
    }

    #[test]
    fn broken_archives_and_metadata_are_refused_with_libalpm_errors() {
        let tmp = TempDir::new();
        let kind = |path: &str| load(path, true).unwrap_err().kind;
        let junk = tmp.path().join("junk.pkg.tar");
        std::fs::write(&junk, "not a package archive").unwrap();
        assert_eq!(ErrorKind::PkgOpen, kind(junk.to_str().unwrap()));
        assert_eq!(ErrorKind::PkgNotFound, kind(tmp.path().join("absent").to_str().unwrap()));
        let no_info = package(tmp.path(), "noinfo", &[("usr/", "")], &[]);
        assert_eq!(ErrorKind::PkgInvalid, kind(&no_info));
        for (name, pkginfo) in [("nover", "pkgname = foo\n"), ("norel", "pkgname = foo\npkgver = 1\n"), ("xdata", "pkgname = foo\npkgver = 1-1\nxdata = novalue\n")] {
            let path = package(tmp.path(), name, &[(".PKGINFO", pkginfo)], &[]);
            assert_eq!(ErrorKind::PkgInvalid, kind(&path), "{name}");
        }
    }
}

