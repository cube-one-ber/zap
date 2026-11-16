//! Build isolation with bubblewrap.
//!
//! Every makepkg invocation, including metadata queries, gets a fresh home
//! and environment. Only the package checkout is writable on the host; the
//! system, the package database and the checkout's Git metadata are
//! read-only. Network access is kept for source downloads: this is filesystem
//! isolation, not a clean build chroot.
use crate::errors::{self, Kind, Result};

const HOME: &str = "/tmp/zap-home";

fn realpath(path: &str) -> Result<String> {
    Ok(std::fs::canonicalize(path)?.to_string_lossy().into_owned())
}

/// Wrap `command` so that it runs isolated inside `dir`.
pub fn argv<S: AsRef<str>>(dir: &str, database: &str, command: &[S]) -> Result<Vec<String>> {
    let unsafe_dir = || errors::fail(Kind::UnsafeSandboxDirectory);
    // The checkout must be a real path that cannot shadow system locations.
    if realpath(dir)? != dir || ["/", "/tmp", "/home"].contains(&dir) {
        return unsafe_dir();
    }
    if ["/usr", "/etc", "/var", "/proc", "/dev", "/run"].iter().any(|system| under(dir, system)) {
        return unsafe_dir();
    }
    if !database.starts_with('/') {
        return unsafe_dir();
    }
    let database_real = realpath(database)?;
    if under(&database_real, dir) || under(dir, &database_real) || ["/", "/tmp", "/home"].contains(&database_real.as_str()) {
        return unsafe_dir();
    }
    std::fs::metadata("/usr/bin/bwrap")?;

    let mut args: Vec<String> = Vec::new();
    let mut add = |items: &[&str]| args.extend(items.iter().map(|s| s.to_string()));
    add(&["/usr/bin/bwrap", "--unshare-all", "--share-net", "--die-with-parent", "--new-session", "--cap-drop", "ALL"]);
    // A read-only system with the usual merged-/usr links.
    add(&["--ro-bind", "/usr", "/usr"]);
    add(&["--symlink", "usr/bin", "/bin", "--symlink", "usr/bin", "/sbin"]);
    add(&["--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64"]);
    add(&["--ro-bind", "/etc", "/etc", "--proc", "/proc", "--dev", "/dev"]);
    // resolv.conf is usually a symlink into systemd-resolved's directory.
    // Mount only that, so downloads can resolve names without exposing /run/user.
    add(&["--dir", "/run", "--dir", "/run/systemd", "--ro-bind-try", "/run/systemd/resolve", "/run/systemd/resolve"]);
    // Private scratch space and home; nothing from the user's home is visible.
    add(&["--tmpfs", "/tmp", "--dir", HOME]);
    add(&["--ro-bind", &database_real, database]);
    add(&["--bind", dir, dir, "--chdir", dir]);
    // No credentials, agents or desktop sockets reach the build.
    add(&["--clearenv"]);
    let environment = [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", HOME),
        ("LANG", "C.UTF-8"),
        ("USER", "zap"),
        ("LOGNAME", "zap"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
    ];
    for (key, value) in environment {
        add(&["--setenv", key, value]);
    }

    // Reviewed history stays immutable: a build cannot rewrite what git reports.
    let git_dir = format!("{}/.git", dir.trim_end_matches('/'));
    match std::fs::metadata(&git_dir) {
        Ok(meta) => {
            if realpath(&git_dir)? != git_dir || !meta.is_dir() {
                return unsafe_dir();
            }
            add(&["--ro-bind", &git_dir, &git_dir]);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    add(&["--"]);
    args.extend(command.iter().map(|s| s.as_ref().to_string()));
    Ok(args)
}

fn under(path: &str, parent: &str) -> bool {
    path == parent || path.strip_prefix(parent).is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    #[test]
    fn sandbox_blocks_host_home_writes_hides_secrets_and_retains_build_outputs() {
        let tmp = TempDir::new();
        let root = tmp.str();
        let (build, database) = (format!("{root}/build"), format!("{root}/db"));
        std::fs::create_dir(&build).unwrap();
        std::fs::create_dir(&database).unwrap();
        std::fs::write(format!("{root}/secret"), "private").unwrap();
        std::fs::write(format!("{database}/record"), "read only").unwrap();
        // These fixed shell instructions are test fixtures, never user interpolation.
        let script = format!("test ! -e '{root}/secret' && test -z \"$SSH_AUTH_SOCK\" && test -z \"$DISPLAY\" && test ! -e /run/user && test -r /etc/resolv.conf && ! touch '{database}/record' 2>/dev/null && touch output && echo isolated");
        let args = argv(&build, &database, &["/usr/bin/bash", "-c", &script]).unwrap();
        let output = crate::util::capture(&args, Some(&build), 4096).unwrap();
        assert_eq!(b"isolated\n", &output[..]);
        assert!(std::path::Path::new(&format!("{build}/output")).exists());
        let refused = |dir: &str, db: &str| argv(dir, db, &["/usr/bin/true"]).unwrap_err().kind == Kind::UnsafeSandboxDirectory;
        assert!(refused("/", &database));
        assert!(refused(&build, root));
        assert!(refused(&build, &build));
    }

    #[test]
    fn sandbox_arguments_keep_the_system_read_only_and_only_the_checkout_writable() {
        let tmp = TempDir::new();
        let root = tmp.str();
        let (build, database) = (format!("{root}/build"), format!("{root}/db"));
        std::fs::create_dir_all(format!("{build}/.git")).unwrap();
        std::fs::create_dir(&database).unwrap();
        let joined = argv(&build, &database, &["/usr/bin/makepkg", "--printsrcinfo"]).unwrap().join(" ");
        let git_dir = format!("{build}/.git");
        let expectations = [
            "/usr/bin/bwrap --unshare-all --share-net --die-with-parent --new-session --cap-drop ALL".to_string(),
            "--ro-bind /usr /usr".into(),
            "--ro-bind /etc /etc".into(),
            "--dir /run --dir /run/systemd --ro-bind-try /run/systemd/resolve /run/systemd/resolve".into(),
            "--tmpfs /tmp --dir /tmp/zap-home".into(),
            format!("--ro-bind {database} {database}"),
            format!("--bind {build} {build} --chdir {build} --clearenv"),
            "--setenv HOME /tmp/zap-home".into(),
            format!("--ro-bind {git_dir} {git_dir} -- /usr/bin/makepkg --printsrcinfo"),
        ];
        for expected in expectations {
            assert!(joined.contains(&expected), "{expected}");
        }
        // Exactly one writable bind, and no host environment passes through.
        assert_eq!(1, joined.matches(" --bind ").count());
        assert!(!joined.contains("SSH_AUTH_SOCK"));
    }
}
