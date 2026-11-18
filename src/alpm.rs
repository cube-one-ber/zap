//! zap's view of the package databases: the handle configured from
//! pacman.conf, package helpers, and the front end that surfaces libalpm's
//! questions, events and progress.
use crate::config::{self, Config};
use crate::errors::{self, Kind, Result};
use crate::libalpm::{self, log, Db, Depend, Event, Handle, Pkg, Progress, Question};
use crate::{signals, ui};
use std::sync::Arc;

pub type PkgRef = Arc<Pkg>;

/// The repository a sync package comes from; `local` for installed packages.
pub fn repository(p: &Pkg) -> &str {
    p.db_name()
}

/// Set when input ended at a provider prompt, so the caller stops instead
/// of taking "no provider" as the answer.
static PROVIDER_ABANDONED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn provider_abandoned() -> bool {
    PROVIDER_ABANDONED.swap(false, std::sync::atomic::Ordering::Relaxed)
}

pub fn explicit(p: &Pkg) -> bool {
    p.explicit()
}

pub fn reason(p: &Pkg) -> &'static str {
    if p.explicit() {
        "explicit"
    } else {
        "dependency"
    }
}

/// Dependencies formatted as `name>=version: description`.
pub fn dependencies(list: &[Depend]) -> Vec<String> {
    list.iter().map(|d| d.to_string()).collect()
}

pub struct Alpm {
    pub h: Handle,
    pub cfg: Config,
}

impl Alpm {
    /// Open the databases configured in pacman.conf.
    pub fn init() -> Result<Alpm> {
        let cfg = Config::load()?;
        let h = Handle::new(&cfg.root, &cfg.db)
            .map_err(|e| errors::with(Kind::AlpmInitializationFailed, e.kind.message()))?;
        libalpm::log::set_frontend(Arc::new(Callbacks));
        let mut alpm = Alpm { h, cfg };
        alpm.configure()?;
        for repo in alpm.cfg.repos.clone() {
            alpm.register(&repo)?;
        }
        Ok(alpm)
    }

    fn configure(&mut self) -> Result<()> {
        let cfg = &self.cfg;
        let h = &mut self.h;
        h.architectures = cfg.architectures.clone();
        h.logfile = cfg.log.clone();
        h.set_gpgdir(&cfg.gpg);
        let sig = config::sig_level(&cfg.signature, 0)?;
        h.siglevel = sig;
        // Local archives inherit the global policy unless configured otherwise.
        h.local_file_siglevel = match &cfg.local_signature {
            Some(tokens) => config::sig_level(tokens, sig)?,
            None => sig,
        };
        h.checkspace = cfg.check_space;
        h.parallel_downloads = cfg.parallel;
        h.sandboxuser = cfg.sandbox_user.clone();
        h.disable_sandbox_filesystem = cfg.disable_sandbox_filesystem;
        h.disable_sandbox_syscalls = cfg.disable_sandbox_syscalls;
        for dir in &cfg.caches {
            h.add_cachedir(dir);
        }
        for dir in &cfg.hooks {
            h.add_hookdir(dir);
        }
        h.ignorepkg = cfg.ignore.clone();
        h.ignoregroup = cfg.ignore_groups.clone();
        h.noupgrade = cfg.no_upgrade.clone();
        h.noextract = cfg.no_extract.clone();
        Ok(())
    }

    fn register(&mut self, repo: &config::Repo) -> Result<()> {
        let sig = config::sig_level(&self.cfg.signature, 0)?;
        let repo_sig = match &repo.signatures {
            Some(tokens) => config::sig_level(tokens, sig)?,
            None => sig,
        };
        let usage = config::usage(&repo.usage)?;
        let arch = self.cfg.arch.clone();
        let db = self
            .h
            .register_syncdb(&repo.name, repo_sig)
            .map_err(|_| errors::with(Kind::RepositoryRegistrationFailed, repo.name.as_str()))?;
        db.usage = usage;
        db.snapshot_dir = snapshot_dir();
        for server in &repo.servers {
            db.servers.push(server.replace("$repo", &repo.name).replace("$arch", &arch));
        }
        Ok(())
    }

    /// Close and reopen the databases after another process changed them.
    pub fn reload(&mut self) -> Result<()> {
        *self = Alpm::init()?;
        Ok(())
    }

    /// A libalpm failure as zap reports it.
    pub fn failed(e: &libalpm::Error) -> errors::Error {
        errors::with(Kind::AlpmOperationFailed, e.kind.message())
    }

    pub fn local_db(&self) -> &Db {
        &self.h.local
    }

    /// The installed package with exactly this name.
    pub fn installed_pkg(&self, name: &str) -> Option<&PkgRef> {
        self.h.local.get(name)
    }

    /// An installed package that satisfies a dependency, including providers.
    pub fn local(&self, dep: &str) -> Option<&PkgRef> {
        libalpm::deps::find_satisfier(self.h.local.pkgs(), &Depend::parse(dep))
    }

    /// A repository package that satisfies a dependency, including providers.
    pub fn repo(&self, dep: &str) -> Option<PkgRef> {
        self.h.find_dbs_satisfier(dep)
    }

    pub fn installed(&self) -> &[PkgRef] {
        self.h.local.pkgs()
    }

    /// Whether no configured repository carries a package of this name.
    pub fn foreign(&self, p: &Pkg) -> bool {
        self.h.syncdbs.iter().all(|db| db.get(&p.name).is_none())
    }

    /// Load every sync database at once, in parallel.
    pub fn load_sync(&self) {
        std::thread::scope(|s| {
            for db in &self.h.syncdbs {
                s.spawn(|| db.pkgs().len());
            }
        });
    }

    /// How current the repository data is: the oldest modification time
    /// among the sync databases, or 0 when one cannot be read.
    pub fn synced_at(&self) -> i64 {
        use std::os::unix::fs::MetadataExt;
        let mut oldest = 0;
        for db in &self.h.syncdbs {
            let path = format!("{}/sync/{}.db", self.cfg.db.trim_end_matches('/'), db.name);
            let Ok(meta) = std::fs::metadata(path) else { return 0 };
            let seconds = meta.mtime();
            if oldest == 0 || seconds < oldest {
                oldest = seconds;
            }
        }
        oldest
    }

    pub fn required_by(&self, p: &Pkg) -> Vec<String> {
        self.h.compute_requiredby(p, false)
    }

    pub fn optional_for(&self, p: &Pkg) -> Vec<String> {
        self.h.compute_requiredby(p, true)
    }

    /// Installed dependencies that nothing requires or optionally uses.
    pub fn orphans(&self) -> Vec<PkgRef> {
        let local = &self.h.local;
        local.preload();
        // One pass over every installed package's dependencies replaces a
        // scan of the database per candidate.
        let candidates: Vec<&PkgRef> = local.pkgs().iter().filter(|p| !p.explicit()).collect();
        let needed = libalpm::db::parallel_map(&candidates, |p| {
            local.pkgs().iter().any(|other| {
                let info = other.info();
                info.depends.iter().chain(&info.optdepends).any(|d| d.satisfied_by(p))
            })
        });
        candidates.into_iter().zip(needed).filter(|(_, n)| !n).map(|(p, _)| p.clone()).collect()
    }

    /// Explain why a transaction could not be prepared or committed.
    pub fn explain(&self, e: &libalpm::Error) {
        match e.kind {
            libalpm::ErrorKind::UnsatisfiedDeps => {
                for m in &e.missing {
                    ui::warn(&format!("{} requires {}, which is not available", m.target, m.depend));
                }
            }
            libalpm::ErrorKind::ConflictingDeps => {
                for c in &e.conflicts {
                    ui::warn(&format!("{} conflicts with {}", c.package1.name, c.package2.name));
                }
            }
            libalpm::ErrorKind::FileConflicts => {
                for c in &e.file_conflicts {
                    ui::warn(&format!("{} would overwrite {}", c.target, c.file));
                }
            }
            _ => {
                for name in &e.names {
                    ui::warn(name);
                }
            }
        }
    }
}

/// Where an unprivileged zap keeps sync database snapshots: beside the build
/// cache, under a name no package base can take. The root worker reads the
/// databases themselves, never a file another user could write.
fn snapshot_dir() -> Option<std::path::PathBuf> {
    if unsafe { libc::geteuid() } == 0 {
        return None;
    }
    let cache = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".cache"),
    };
    cache.is_absolute().then(|| cache.join("zap/.sync"))
}

// ── Callbacks ───────────────────────────────────────────────────────────────

struct Callbacks;

/// A yes/no question: asked, or answered with `unattended` under --noconfirm.
fn ask(prompt: &str, unattended: bool) -> bool {
    if ui::unattended() {
        ui::assumed(prompt, if unattended { "yes" } else { "no" });
        return unattended;
    }
    ui::confirm(prompt)
}

fn interrupted() -> bool {
    signals::CANCELLED.load(std::sync::atomic::Ordering::Relaxed)
}

impl libalpm::Frontend for Callbacks {
    fn log(&self, level: log::Level, message: &str) {
        if level != log::Level::Debug {
            ui::diagnostic(level == log::Level::Error, message);
        }
    }

    /// Each yes/no prompt's default (on Enter) is pacman's --noconfirm answer,
    /// which is also the answer when unattended; a provider choice defaults to
    /// the first provider. Questions without a case here, such as skipping
    /// unresolvable targets or deleting corrupted files, stay "no".
    fn question(&self, q: &mut Question) {
        match q {
            Question::InstallIgnorePkg { pkg, install } => {
                ui::note(ui::NoteKind::Warning, &format!("{} is listed in IgnorePkg.", pkg.name));
                *install = ask("Install it anyway? [Y/n]", true);
            }
            Question::Replace { old, new, replace, .. } => {
                ui::note(ui::NoteKind::Warning, &format!("{} replaces {}.", new.name, old.name));
                *replace = ask("Approve the replacement? [Y/n]", true);
            }
            Question::Conflict { package1, package2, remove, .. } => {
                ui::note(ui::NoteKind::Warning, &format!("{package1} conflicts with {package2}."));
                *remove = ask(&format!("Remove {package2}? [y/N]"), false);
            }
            Question::SelectProvider { depend, providers, use_index } => {
                ui::heading("Choose a provider", &depend.to_string());
                for (i, p) in providers.iter().enumerate() {
                    ui::entry(&ui::Entry {
                        index: Some(i + 1),
                        source: repository(p).into(),
                        name: p.name.clone(),
                        version: p.version.clone(),
                        ..ui::Entry::default()
                    });
                }
                if ui::unattended() {
                    ui::assumed(&format!("Enter a number 1-{} (default 1):", providers.len()), "1");
                    *use_index = 0;
                    return;
                }
                // Ask again until the reply is in range; ended input cancels.
                *use_index = loop {
                    let Ok(reply) = ui::answer(&format!("Enter a number 1-{} (default 1):", providers.len())) else {
                        PROVIDER_ABANDONED.store(true, std::sync::atomic::Ordering::Relaxed);
                        break -1;
                    };
                    let index = if reply.is_empty() { 1 } else { reply.parse::<usize>().unwrap_or(0) };
                    if index > 0 && index <= providers.len() {
                        break index as i32 - 1;
                    }
                    ui::note(ui::NoteKind::Warning, &format!("Choose a number from 1 to {}.", providers.len()));
                };
            }
            Question::ImportKey { fingerprint, uid, import } => {
                ui::note(ui::NoteKind::Warning, &format!("Signing key {fingerprint} ({uid}) is not in the keyring."));
                *import = ask("Import and trust this key for package verification? [Y/n]", true);
            }
            // zap never deletes cache files itself; it names the file so the
            // user can remove it and run again for a fresh download.
            Question::CorruptedPkg { filepath, remove } => {
                ui::note(
                    ui::NoteKind::Failure,
                    &format!("{filepath} does not match the repository. Delete it as root, for example with sudo rm {filepath}, and run zap again to download it fresh."),
                );
                *remove = false;
            }
            Question::RemovePkgs { skip, .. } => *skip = false,
        }
    }

    fn event(&self, e: &Event) {
        if interrupted() {
            return;
        }
        match e {
            Event::ScriptletInfo { line } => ui::line(line.trim_end_matches([' ', '\r', '\n']), 4, ui::Tone::Plain),
            Event::HookRunStart { position, total, desc, .. } => {
                ui::note(ui::NoteKind::Info, &format!("Hook {position}/{total} · {desc}"))
            }
            Event::PacnewCreated { file } => ui::note(
                ui::NoteKind::Warning,
                &format!("New configuration saved as {file}.pacnew; merge it when convenient."),
            ),
            Event::PacsaveCreated { file } => {
                ui::note(ui::NoteKind::Warning, &format!("Previous configuration kept as {file}.pacsave."))
            }
            Event::PkgRetrieveStart { num, total_size } => ui::heading(
                "Downloading",
                &format!("{} · {}", ui::count(*num, "package", "packages"), ui::size(*total_size)),
            ),
            _ => {}
        }
    }

    fn progress(&self, kind: Progress, name: &str, percent: i32, total: usize, current: usize) {
        if interrupted() {
            return;
        }
        let label = match kind {
            Progress::Download if percent >= 100 => "Downloaded",
            Progress::Download => "Downloading",
            // A finished step reads in the past tense, as a finished download does.
            Progress::AddStart if percent >= 100 => "Installed",
            Progress::AddStart => "Installing",
            Progress::UpgradeStart if percent >= 100 => "Upgraded",
            Progress::UpgradeStart => "Upgrading",
            Progress::DowngradeStart if percent >= 100 => "Downgraded",
            Progress::DowngradeStart => "Downgrading",
            Progress::ReinstallStart if percent >= 100 => "Reinstalled",
            Progress::ReinstallStart => "Reinstalling",
            Progress::RemoveStart if percent >= 100 => "Removed",
            Progress::RemoveStart => "Removing",
            Progress::ConflictsStart if percent >= 100 => "Checked conflicts",
            Progress::ConflictsStart => "Checking conflicts",
            Progress::DiskspaceStart if percent >= 100 => "Checked disk space",
            Progress::DiskspaceStart => "Checking disk space",
            Progress::IntegrityStart if percent >= 100 => "Verified packages",
            Progress::IntegrityStart => "Verifying packages",
            Progress::LoadStart if percent >= 100 => "Loaded packages",
            Progress::LoadStart => "Loading packages",
            Progress::KeyringStart if percent >= 100 => "Checked keyring",
            Progress::KeyringStart => "Checking keyring",
        };
        ui::progress(label, name, percent.max(0) as usize, current, total);
    }
}
