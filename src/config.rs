//! The subset of pacman.conf that zap honours. Settings zap cannot honour
//! faithfully are rejected instead of silently ignored.
use crate::errors::{self, Kind, Result};
use crate::libalpm::{self, db};
use crate::util;

#[derive(Clone, Debug)]
pub struct Repo {
    pub name: String,
    pub servers: Vec<String>,
    pub signatures: Option<String>,
    pub usage: String,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub root: String,
    pub db: String,
    pub log: String,
    pub gpg: String,
    pub arch: String,
    pub signature: String,
    pub local_signature: Option<String>,
    pub sandbox_user: Option<String>,
    pub disable_sandbox_filesystem: bool,
    pub disable_sandbox_syscalls: bool,
    pub caches: Vec<String>,
    pub hooks: Vec<String>,
    pub ignore: Vec<String>,
    pub ignore_groups: Vec<String>,
    pub hold: Vec<String>,
    pub no_upgrade: Vec<String>,
    pub no_extract: Vec<String>,
    pub repos: Vec<Repo>,
    pub check_space: bool,
    pub parallel: u32,
    pub architectures: Vec<String>,

    // Parser state.
    db_explicit: bool,
    log_explicit: bool,
    section: Option<usize>,
    in_options: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            root: "/".into(),
            db: "/var/lib/pacman/".into(),
            log: "/var/log/pacman.log".into(),
            gpg: "/etc/pacman.d/gnupg/".into(),
            arch: "x86_64".into(),
            signature: "Required DatabaseOptional".into(),
            local_signature: None,
            sandbox_user: None,
            disable_sandbox_filesystem: false,
            disable_sandbox_syscalls: false,
            caches: Vec::new(),
            hooks: Vec::new(),
            ignore: Vec::new(),
            ignore_groups: Vec::new(),
            hold: Vec::new(),
            no_upgrade: Vec::new(),
            no_extract: Vec::new(),
            repos: Vec::new(),
            check_space: false,
            parallel: 5,
            architectures: Vec::new(),
            db_explicit: false,
            log_explicit: false,
            section: None,
            in_options: false,
        }
    }
}

/// `std.fs.path.join`: one separator between parts.
fn join(base: &str, rest: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), rest.trim_start_matches('/'))
}

fn words(value: &str) -> impl Iterator<Item = &str> {
    value.split([' ', '\t']).filter(|w| !w.is_empty())
}

impl Config {
    pub fn load() -> Result<Config> {
        let mut cfg = Config::default();
        unsafe {
            let mut uts: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut uts) == 0 {
                cfg.arch = std::ffi::CStr::from_ptr(uts.machine.as_ptr()).to_string_lossy().into_owned();
            }
        }
        cfg.read("/etc/pacman.conf", 0)?;
        // DBPath and LogFile follow RootDir unless set explicitly.
        if !cfg.db_explicit {
            cfg.db = join(&cfg.root, "var/lib/pacman/");
        }
        if !cfg.log_explicit {
            cfg.log = join(&cfg.root, "var/log/pacman.log");
        }
        if cfg.architectures.is_empty() {
            cfg.architectures.push(cfg.arch.clone());
        }
        if cfg.parallel == 0 || cfg.parallel > 128 {
            return errors::fail(Kind::InvalidParallelDownloads);
        }
        for path in [&cfg.root, &cfg.db, &cfg.log, &cfg.gpg] {
            if !path.starts_with('/') {
                return Err(errors::with(Kind::InvalidConfigurationPath, path.as_str()));
            }
        }
        if cfg.caches.is_empty() {
            cfg.caches.push("/var/cache/pacman/pkg/".into());
        }
        Ok(cfg)
    }

    fn read(&mut self, path: &str, depth: usize) -> Result<()> {
        if depth > 16 {
            return Err(errors::with(Kind::ConfigurationIncludeCycle, path));
        }
        let bytes = util::read_file(path, 4 * 1024 * 1024).map_err(|e| errors::with(e, path))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        for raw in text.split('\n') {
            let uncommented = &raw[..raw.find('#').unwrap_or(raw.len())];
            let s = uncommented.trim_matches([' ', '\r', '\t']);
            if s.is_empty() {
                continue;
            }
            if s.len() >= 2 && s.starts_with('[') && s.ends_with(']') {
                self.enter(&s[1..s.len() - 1])?;
                continue;
            }
            let equals = s.find('=');
            let key = s[..equals.unwrap_or(s.len())].trim_matches([' ', '\t']);
            let value = equals.map(|at| s[at + 1..].trim_matches([' ', '\t'])).unwrap_or("");
            if key == "Include" {
                self.include(path, value, depth)?;
            } else {
                self.option(key, value)?;
            }
        }
        Ok(())
    }

    fn enter(&mut self, name: &str) -> Result<()> {
        self.in_options = name == "options";
        self.section = None;
        if self.in_options {
            return Ok(());
        }
        if !util::valid_name(name) {
            return Err(errors::with(Kind::InvalidRepositoryName, name));
        }
        // A repeated section continues the earlier one and keeps its priority.
        if let Some(i) = self.repos.iter().position(|r| r.name == name) {
            self.section = Some(i);
            return Ok(());
        }
        self.repos.push(Repo { name: name.into(), servers: Vec::new(), signatures: None, usage: "All".into() });
        self.section = Some(self.repos.len() - 1);
        Ok(())
    }

    fn include(&mut self, from: &str, value: &str, depth: usize) -> Result<()> {
        let pattern = if value.starts_with('/') {
            value.to_string()
        } else {
            let dir = match from.rfind('/') {
                Some(0) => "/",
                Some(at) => &from[..at],
                None => ".",
            };
            join(dir, value)
        };
        let matches = glob(&pattern).ok_or_else(|| errors::with(Kind::ConfigurationIncludeFailed, pattern.as_str()))?;
        for path in matches {
            self.read(&path, depth + 1)?;
        }
        Ok(())
    }

    fn option(&mut self, key: &str, value: &str) -> Result<()> {
        if !self.in_options {
            let Some(index) = self.section else { return Ok(()) };
            let repo = &mut self.repos[index];
            match key {
                "Server" => repo.servers.push(value.into()),
                "SigLevel" => repo.signatures = Some(value.into()),
                "Usage" => repo.usage = value.into(),
                _ => {}
            }
            return Ok(());
        }
        let list = |list: &mut Vec<String>| list.extend(words(value).map(String::from));
        match key {
            "RootDir" => self.root = value.into(),
            "DBPath" => {
                self.db = value.into();
                self.db_explicit = true;
            }
            "LogFile" => {
                self.log = value.into();
                self.log_explicit = true;
            }
            "GPGDir" => self.gpg = value.into(),
            "SigLevel" => self.signature = value.into(),
            "LocalFileSigLevel" => self.local_signature = Some(value.into()),
            "CacheDir" => list(&mut self.caches),
            "HookDir" => list(&mut self.hooks),
            "IgnorePkg" => list(&mut self.ignore),
            "IgnoreGroup" => list(&mut self.ignore_groups),
            "HoldPkg" => list(&mut self.hold),
            "NoUpgrade" => list(&mut self.no_upgrade),
            "NoExtract" => list(&mut self.no_extract),
            "Architecture" => {
                for word in words(value) {
                    let arch = if word == "auto" { self.arch.clone() } else { word.to_string() };
                    self.architectures.push(arch);
                }
                if let Some(first) = self.architectures.first() {
                    self.arch = first.clone();
                }
            }
            "DownloadUser" => self.sandbox_user = Some(value.into()),
            "DisableSandbox" => {
                self.disable_sandbox_filesystem = true;
                self.disable_sandbox_syscalls = true;
            }
            "DisableSandboxFilesystem" => self.disable_sandbox_filesystem = true,
            "DisableSandboxSyscalls" => self.disable_sandbox_syscalls = true,
            "CheckSpace" => self.check_space = true,
            "ParallelDownloads" => {
                self.parallel = value.parse().map_err(|_| Kind::InvalidParallelDownloads)?;
            }
            "XferCommand" | "AssumeInstalled" => return Err(errors::with(Kind::UnsupportedPacmanConfiguration, key)),
            _ => {}
        }
        Ok(())
    }
}

/// glob(3) without flags: None when nothing matches or on error.
fn glob(pattern: &str) -> Option<Vec<String>> {
    let c = util::cstr(pattern);
    unsafe {
        let mut matches: libc::glob_t = std::mem::zeroed();
        let result = libc::glob(c.as_ptr(), 0, None, &mut matches);
        let paths = (result == 0).then(|| {
            (0..matches.gl_pathc)
                .map(|i| std::ffi::CStr::from_ptr(*matches.gl_pathv.add(i)).to_string_lossy().into_owned())
                .collect()
        });
        libc::globfree(&mut matches);
        paths
    }
}

/// Combine SigLevel tokens with an inherited level. `Package` and `Database`
/// prefixes restrict a token to one kind of signature.
pub fn sig_level(tokens: &str, initial: i32) -> Result<i32> {
    use libalpm::*;
    let mut result = initial;
    for word in words(tokens) {
        let (token, package, database) = if let Some(rest) = word.strip_prefix("Package") {
            (rest, true, false)
        } else if let Some(rest) = word.strip_prefix("Database") {
            (rest, false, true)
        } else {
            (word, true, true)
        };
        for (enabled, kind) in [(package, 0), (database, 1)] {
            if !enabled {
                continue;
            }
            let (required, optional, marginal, unknown) = if kind == 0 {
                (SIG_PACKAGE, SIG_PACKAGE_OPTIONAL, SIG_PACKAGE_MARGINAL_OK, SIG_PACKAGE_UNKNOWN_OK)
            } else {
                (SIG_DATABASE, SIG_DATABASE_OPTIONAL, SIG_DATABASE_MARGINAL_OK, SIG_DATABASE_UNKNOWN_OK)
            };
            match token {
                "Never" => result &= !(required | optional),
                "Optional" => result |= required | optional,
                "Required" => {
                    result |= required;
                    result &= !optional;
                }
                "TrustedOnly" => result &= !(marginal | unknown),
                "TrustAll" => result |= marginal | unknown,
                _ => return Err(errors::with(Kind::InvalidSignaturePolicy, word)),
            }
        }
    }
    Ok(result)
}

/// A repository's Usage tokens as libalpm flags.
pub fn usage(tokens: &str) -> Result<u32> {
    let mut result = 0;
    for word in words(tokens) {
        result |= match word {
            "All" => db::USAGE_ALL,
            "Sync" => db::USAGE_SYNC,
            "Search" => db::USAGE_SEARCH,
            "Install" => db::USAGE_INSTALL,
            "Upgrade" => db::USAGE_UPGRADE,
            _ => return Err(errors::with(Kind::InvalidRepositoryUsage, word)),
        };
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libalpm::*;

    #[test]
    fn signature_policy_preserves_independent_database_and_package_requirements() {
        let required = sig_level("Required DatabaseOptional", 0).unwrap();
        assert!(required & SIG_PACKAGE != 0);
        assert!(required & SIG_PACKAGE_OPTIONAL == 0);
        assert!(required & SIG_DATABASE_OPTIONAL != 0);
        assert_eq!(Kind::InvalidSignaturePolicy, sig_level("Bogus", required).unwrap_err().kind);
        assert!(sig_level("Optional", 0).unwrap() & SIG_PACKAGE_OPTIONAL != 0);
    }

    #[test]
    fn repository_usage_combines_tokens_and_rejects_unknown_ones() {
        assert_eq!(db::USAGE_ALL, usage("All").unwrap());
        assert_eq!(db::USAGE_SYNC | db::USAGE_SEARCH, usage("Sync  Search").unwrap());
        assert_eq!(Kind::InvalidRepositoryUsage, usage("Sync Everything").unwrap_err().kind);
    }

    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zap-config-{}-{:?}", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn configuration_includes_preserve_repository_order_signatures_and_architecture_lists() {
        let dir = temp();
        std::fs::write(dir.join("mirrors"), "Server = https://mirror.example/$repo/os/$arch\n").unwrap();
        std::fs::write(dir.join("pacman.conf"), "[options]\nSigLevel = Required DatabaseOptional\nLocalFileSigLevel = Required\nArchitecture = x86_64 aarch64\nIgnorePkg = protected*\n[priority]\nInclude = mirrors\n[extra]\nServer = https://other.example/$repo/os/$arch\n").unwrap();
        let mut cfg = Config::default();
        cfg.read(dir.join("pacman.conf").to_str().unwrap(), 0).unwrap();
        assert_eq!(2, cfg.repos.len());
        assert_eq!("priority", cfg.repos[0].name);
        assert_eq!("https://mirror.example/$repo/os/$arch", cfg.repos[0].servers[0]);
        assert_eq!(Some("Required"), cfg.local_signature.as_deref());
        assert_eq!(2, cfg.architectures.len());
        assert_eq!("protected*", cfg.ignore[0]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configuration_include_cycles_and_unsafe_policy_overrides_fail_explicitly() {
        let dir = temp();
        std::fs::write(dir.join("loop"), "Include = loop\n").unwrap();
        let mut cfg = Config { in_options: true, ..Config::default() };
        assert_eq!(Kind::ConfigurationIncludeCycle, cfg.read(dir.join("loop").to_str().unwrap(), 0).unwrap_err().kind);
        assert_eq!(Kind::UnsupportedPacmanConfiguration, cfg.option("XferCommand", "sh -c something").unwrap_err().kind);
        assert_eq!(Kind::UnsupportedPacmanConfiguration, cfg.option("AssumeInstalled", "missing=1").unwrap_err().kind);
        assert_eq!(None, cfg.local_signature);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
