//! Helpers from libalpm's util.c and log.c: file checks, digests, the log
//! file, and running scriptlets and hooks inside the root.
use super::log::{self, Event};
use super::Handle;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Arch builds libalpm with these paths.
pub const SCRIPTLET_SHELL: &str = "/usr/bin/bash";
pub const LDCONFIG: &str = "/usr/bin/ldconfig";

fn cstring(path: &str) -> std::ffi::CString {
    std::ffi::CString::new(path).unwrap_or_default()
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// `strerror` for an errno value.
pub fn errno_str(errno: i32) -> String {
    super::strerror(&std::io::Error::from_raw_os_error(errno))
}

/// `_alpm_access` without following a final symlink: the errno on failure.
pub fn access(path: &str, mode: i32) -> std::result::Result<(), i32> {
    let c = cstring(path);
    if unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(errno())
    }
}

/// `_alpm_files_in_directory` without a full count: Some(0) or Some(1),
/// `None` when the directory cannot be read.
pub fn files_in_directory(path: &str) -> Option<usize> {
    let mut entries = std::fs::read_dir(path).ok()?;
    Some(entries.next().is_some() as usize)
}

/// `alpm_compute_md5sum`.
pub fn md5_file(path: &str) -> Option<String> {
    use md5::Digest;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = md5::Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex(&hasher.finalize()))
}

/// `alpm_compute_sha256sum`.
pub fn sha256_file(path: &str) -> Option<String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `_alpm_fnmatch_patterns`: the last matching pattern decides. Some(true)
/// for a match, Some(false) for a match by a negated (`!`) pattern, `None`
/// when nothing matches.
pub fn fnmatch_patterns(patterns: &[String], string: &str) -> Option<bool> {
    for pattern in patterns.iter().rev() {
        let inverted = pattern.starts_with('!');
        let p = if inverted || pattern.starts_with('\\') { &pattern[1..] } else { pattern.as_str() };
        if super::fnmatch(p, string) {
            return Some(!inverted);
        }
    }
    None
}

/// `mkdir -p` with a mode, as `_alpm_makepath_mode`.
pub fn makepath_mode(path: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(mode).create(path)
}

/// Local time as `%FT%T%z`.
pub fn timestamp() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    let mut buf = [0u8; 32];
    let format = cstring("%FT%T%z");
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Run `argv` with the root as `/`, its output forwarded line by line as
/// scriptlet events. Returns whether it succeeded. `stdin` is written to
/// the command's standard input.
pub fn run_chroot(handle: &Handle, cmd: &str, argv: &[&str], stdin: Option<Vec<u8>>) -> bool {
    log::debug(&format!("executing \"{cmd}\" under chroot \"{}\"\n", handle.root));
    let (mut parent, child) = match std::os::unix::net::UnixStream::pair() {
        Ok(pair) => pair,
        Err(e) => {
            log::error(&format!("could not create pipe ({})\n", super::strerror(&e)));
            return false;
        }
    };
    let (Ok(out), Ok(err)) = (child.try_clone(), child.try_clone()) else { return false };
    let root = handle.root.clone();
    let mut command = Command::new(cmd);
    command
        .arg0(argv[0])
        .args(&argv[1..])
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(out)))
        .stderr(Stdio::from(std::os::fd::OwnedFd::from(err)))
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .env_remove("BASH_ENV");
    if std::env::var_os("SHLVL").is_none() {
        command.env("SHLVL", "1");
    }
    unsafe {
        command.pre_exec(move || {
            // Not chrooting to "/" lets a caller that set the root up run
            // with fewer capabilities.
            if root != "/" {
                let dir = std::ffi::CString::new(root.as_str()).unwrap_or_default();
                if libc::chroot(dir.as_ptr()) != 0 {
                    let e = std::io::Error::last_os_error();
                    let _ = writeln!(std::io::stderr(), "could not change the root directory ({})", super::strerror(&e));
                    return Err(e);
                }
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::umask(0o022);
            Ok(())
        });
    }
    let mut spawned = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            log::error(&format!("call to execv failed ({})\n", super::strerror(&e)));
            return false;
        }
    };
    drop(command);
    drop(child);
    let writer = spawned.stdin.take().zip(stdin).map(|(mut pipe, data)| {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        })
    });
    let mut pending = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match parent.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                pending.extend_from_slice(&buf[..n]);
                while let Some(at) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=at).collect();
                    scriptlet_output(handle, &String::from_utf8_lossy(&line));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        pending.push(b'\n');
        scriptlet_output(handle, &String::from_utf8_lossy(&pending));
    }
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    match spawned.wait() {
        Ok(status) => {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                let name = unsafe { std::ffi::CStr::from_ptr(libc::strsignal(signal)) }.to_string_lossy().into_owned();
                log::error(&format!("command terminated by signal {signal}: {name}\n"));
                false
            } else if status.code() != Some(0) {
                log::error("command failed to execute correctly\n");
                false
            } else {
                true
            }
        }
        Err(e) => {
            log::error(&format!("call to waitpid failed ({})\n", super::strerror(&e)));
            false
        }
    }
}

fn scriptlet_output(handle: &Handle, line: &str) {
    handle.logaction("ALPM-SCRIPTLET", line);
    log::event(&Event::ScriptletInfo { line });
}

/// Whether `needle` appears outside comments in the scriptlet at `path`.
fn grep(path: &str, needle: &str) -> bool {
    let Ok(text) = std::fs::read(path) else { return false };
    String::from_utf8_lossy(&text).lines().any(|line| line.split('#').next().unwrap_or("").contains(needle))
}

impl Handle {
    /// `alpm_logaction`: append a line to the log file.
    pub fn logaction(&self, prefix: &str, message: &str) {
        if self.logfile.is_empty() {
            return;
        }
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new().append(true).create(true).mode(0o644).open(&self.logfile);
        if let Ok(mut file) = file {
            let _ = write!(file, "[{}] [{}] {}", timestamp(), if prefix.is_empty() { "UNKNOWN" } else { prefix }, message);
        }
    }

    /// `_alpm_runscriptlet`: run `script` from an install file (or from the
    /// archive at `path` with `is_archive`) inside the root. Returns whether
    /// it succeeded or there was nothing to run.
    pub(crate) fn runscriptlet(&self, path: &str, script: &str, ver: &str, oldver: Option<&str>, is_archive: bool) -> bool {
        if access(path, libc::R_OK).is_err() {
            log::debug(&format!("scriptlet '{path}' not found\n"));
            return true;
        }
        if !is_archive && !grep(path, script) {
            return true;
        }
        let tmp = format!("{}tmp/", self.root);
        if !std::path::Path::new(&tmp).exists() {
            let _ = makepath_mode(&tmp, 0o1777);
            let _ = std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o1777));
        }
        let mut template = cstring(&format!("{tmp}alpm_XXXXXX")).into_bytes_with_nul();
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            log::error("could not create temp directory\n");
            return false;
        }
        template.pop();
        let tmpdir = String::from_utf8_lossy(&template).into_owned();
        let scriptfn = format!("{tmpdir}/.INSTALL");
        let ready = if is_archive {
            super::archive::unpack_single(path, ".INSTALL", &scriptfn)
        } else {
            match std::fs::copy(path, &scriptfn) {
                Ok(_) => true,
                Err(e) => {
                    log::error(&format!("could not copy tempfile to {scriptfn} ({})\n", super::strerror(&e)));
                    false
                }
            }
        };
        let mut ok = ready;
        if ready && (!is_archive || grep(&scriptfn, script)) {
            // The script's path inside the chroot.
            let scriptpath = &scriptfn[self.root.len() - 1..];
            let cmdline = match oldver {
                Some(old) => format!(". {scriptpath}; {script} {ver} {old}"),
                None => format!(". {scriptpath}; {script} {ver}"),
            };
            log::debug(&format!("executing \"{cmdline}\"\n"));
            ok = run_chroot(self, SCRIPTLET_SHELL, &[SCRIPTLET_SHELL, "-c", &cmdline], None);
        }
        if std::path::Path::new(&scriptfn).exists() && std::fs::remove_file(&scriptfn).is_err() {
            log::warning(&format!("could not remove {scriptfn}\n"));
        }
        if std::fs::remove_dir(&tmpdir).is_err() {
            log::warning(&format!("could not remove tmpdir {tmpdir}\n"));
        }
        ok
    }

    /// `_alpm_ldconfig`: run ldconfig in the root when it is configured.
    pub(crate) fn ldconfig(&self) {
        log::debug("running ldconfig\n");
        if std::path::Path::new(&format!("{}etc/ld.so.conf", self.root)).exists()
            && access(&format!("{}{}", self.root, &LDCONFIG[1..]), libc::X_OK).is_ok()
        {
            run_chroot(self, LDCONFIG, &["ldconfig"], None);
        }
    }
}
