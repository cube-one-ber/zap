//! Shared primitives: name validation and child processes with an
//! allowlisted environment.
use crate::errors::{self, Kind, Result};
use crate::ui;
use std::ffi::CString;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

pub const VERSION: &str = match option_env!("ZAP_VERSION") {
    Some(v) => v,
    None => "0.2.0",
};

/// Package, base and repository names. A valid name cannot be mistaken for an
/// option or escape the directory it is joined to.
pub fn valid_name(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 255 || b[0] == b'-' || b[0] == b'.' {
        return false;
    }
    b.iter().all(|&ch| ch.is_ascii_alphanumeric() || b"@._+-".contains(&ch))
}

/// The name part of a dependency such as `foo>=2`.
pub fn dep_name(s: &str) -> &str {
    &s[..s.find(['<', '>', '=']).unwrap_or(s.len())]
}

pub fn contains<S: AsRef<str>>(list: &[S], value: &str) -> bool {
    list.iter().any(|item| item.as_ref() == value)
}

pub fn append_unique(list: &mut Vec<String>, value: &str) {
    if !contains(list, value) {
        list.push(value.to_string());
    }
}

pub fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

pub fn read_file(path: impl AsRef<Path>, max: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.by_ref().take(max as u64 + 1).read_to_end(&mut buf)?;
    if buf.len() > max {
        return Err(Kind::FileTooBig.into());
    }
    Ok(buf)
}

/// Run a command attached to the terminal.
pub fn run<S: AsRef<str>>(args: &[S], cwd: Option<&str>) -> Result<()> {
    if run_status(args, cwd)? != 0 {
        return Err(failed(args));
    }
    Ok(())
}

/// Run a command attached to the terminal and return its exit status. A
/// command ended by a signal reports 128 plus the signal number, as shells do.
pub fn run_status<S: AsRef<str>>(args: &[S], cwd: Option<&str>) -> Result<u8> {
    ui::flush();
    let status = command(args, cwd).status()?;
    Ok(exit_status(status))
}

fn exit_status(status: ExitStatus) -> u8 {
    if let Some(code) = status.code() {
        return code as u8;
    }
    match status.signal() {
        Some(signal) => (128 + signal).min(255) as u8,
        None => 255,
    }
}

/// Run a command and return its standard output. Standard error is shown,
/// sanitized, only when the command fails.
pub fn capture<S: AsRef<str>>(args: &[S], cwd: Option<&str>, max: usize) -> Result<Vec<u8>> {
    ui::flush();
    let output = command(args, cwd).stdin(Stdio::inherit()).output()?;
    if output.status.success() {
        if output.stdout.len() > max {
            return Err(Kind::StdoutStreamTooLong.into());
        }
        return Ok(output.stdout);
    }
    // Tool output can quote remote or package-controlled text.
    for line in output.stderr.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        ui::eprint(&format!("    {}\n", ui::safe_bytes(line)));
    }
    Err(failed(args))
}

/// Children never inherit the caller's full environment.
fn command<S: AsRef<str>>(args: &[S], cwd: Option<&str>) -> Command {
    let mut cmd = Command::new(args[0].as_ref());
    cmd.args(args[1..].iter().map(|a| a.as_ref()));
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    cmd.env_clear().env("PATH", "/usr/bin:/bin");
    const INHERITED: [&str; 13] = [
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TERM",
        "COLORTERM",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSH_AUTH_SOCK",
        "MAKEFLAGS",
    ];
    for key in INHERITED {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null").env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

fn failed<S: AsRef<str>>(args: &[S]) -> errors::Error {
    errors::with(Kind::CommandFailed, command_label(args))
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The tool a command line runs, looking through the sandbox wrapper and
/// including the subcommand of multi-purpose tools.
pub fn command_label<S: AsRef<str>>(args: &[S]) -> String {
    let mut rest: Vec<&str> = args.iter().map(|a| a.as_ref()).collect();
    if !rest.is_empty() && basename(rest[0]) == "bwrap" {
        if let Some(i) = rest.iter().position(|a| *a == "--") {
            rest.drain(..=i);
        }
    }
    if rest.is_empty() {
        return "command".into();
    }
    let tool = basename(rest[0]);
    if tool != "git" {
        return tool.into();
    }
    // Skip `-c key=value` pairs to reach the git subcommand.
    let mut i = 1;
    while i + 1 < rest.len() && rest[i] == "-c" {
        i += 2;
    }
    if i >= rest.len() {
        return tool.into();
    }
    format!("git {}", rest[i])
}

/// Create a private directory, refusing one owned by another user or
/// replaced by a symlink.
pub fn secure_dir(path: &str) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path).map_err(|_| Kind::UnsafeCacheDirectory)?;
    if meta.uid() != unsafe { libc::getuid() } || !meta.file_type().is_dir() {
        return Err(Kind::UnsafeCacheDirectory.into());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(|_| Kind::UnsafeCacheDirectory)?;
    Ok(())
}

/// Join paths as Zig's `std.fs.path.join` does: one separator between
/// non-empty parts, keeping a trailing one.
pub fn join(base: &str, rest: &str) -> String {
    match (base.is_empty(), rest.is_empty()) {
        (_, true) => base.into(),
        (true, false) => rest.into(),
        _ if base.ends_with('/') && rest.starts_with('/') => format!("{base}{}", &rest[1..]),
        _ if base.ends_with('/') || rest.starts_with('/') => format!("{base}{rest}"),
        _ => format!("{base}/{rest}"),
    }
}

/// `path` made absolute and normalised without touching the file system,
/// as Zig's `std.fs.path.resolve`.
pub fn resolve(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let joined = if path.starts_with('/') { path.to_string() } else { format!("{cwd}/{path}") };
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_cannot_become_flags_or_paths() {
        for name in ["", "-oops", "../etc", ".hidden", "foo/bar", "x\n", "a;id", "a b"] {
            assert!(!valid_name(name), "{name:?}");
        }
        assert!(valid_name("libfoo-git"));
        assert_eq!("foo", dep_name("foo>=2:1.0"));
    }

    #[test]
    fn failed_commands_are_named_by_the_tool_the_user_would_recognise() {
        assert_eq!("git fetch", command_label(&["/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-c", "a=b", "fetch", "--", "url"]));
        assert_eq!("makepkg", command_label(&["/usr/bin/bwrap", "--unshare-all", "--", "/usr/bin/makepkg", "--printsrcinfo"]));
        assert_eq!("run0", command_label(&["/usr/bin/run0", "--pty"]));
        assert_eq!("command", command_label::<&str>(&[]));
    }
}
