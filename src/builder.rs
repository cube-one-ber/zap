//! AUR and local builds: dependency planning from static metadata, source
//! review, sandboxed makepkg runs, and hand-off of verified archives to the
//! transaction worker.
//!
//! Nothing from a package is executed before its sources are approved, and
//! approved sources are re-verified around every step that could change them.
use crate::alpm::{self, Alpm};
use crate::artifacts;
use crate::aur::{self, Package};
use crate::cli::{Reason, Scope, SearchBy};
use crate::errors::{self, Kind, Result};
use crate::libalpm::{self, vercmp, Pkg};
use crate::srcinfo::{self, Metadata};
use crate::transaction::{self, Operation, Request, Target};
use crate::ui::{self, Cell, Column, NoteKind, Table, Tone};
use crate::util::{self, append_unique, contains, dep_name, join, valid_name};
use crate::sandbox;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

const SRCINFO_LIMIT: usize = 2 * 1024 * 1024;
const SOURCE_LIMIT: usize = 16 * 1024 * 1024;
/// The commit of a package base you last approved. Diffs are shown against
/// it, so fetching for a dry run, `get` or `pkgbuild` never hides a change.
const APPROVED_REF: &str = "refs/zap/approved";

/// One package base to build, from the AUR or from a local snapshot.
#[derive(Clone, Debug)]
struct Node {
    package: Package,
    dir: String,
    metadata: Metadata,
    /// The packages of this base that the plan installs.
    wanted: Vec<String>,
    /// On the current resolution path; meeting it again is a cycle.
    visiting: bool,
    planned: bool,
    reviewed: bool,
    /// Fingerprint of the tracked sources the user approved.
    source_digest: Option<String>,
    local_source: bool,
}

impl Node {
    fn new(package: Package, dir: String, metadata: Metadata) -> Node {
        Node { package, dir, metadata, wanted: Vec::new(), visiting: true, planned: false, reviewed: false, source_digest: None, local_source: false }
    }
}

pub struct Builder<'a> {
    db: &'a mut Alpm,
    root: String,
    /// Holds the cache lock while the builder lives.
    _lock: Option<std::fs::File>,
    requested: Vec<String>,
    pub preserve_reasons: bool,
    pub scope: Scope,
    /// Requested names that must come from the AUR even if a repository has them.
    pub aur_targets: Vec<String>,
    pub needed: bool,
    pub sandbox: bool,
    pub clean_after: bool,
    pub rebuild_tree: bool,
    pub reason: Option<Reason>,
    nodes: Vec<Node>,
    /// Indices into `nodes`, dependencies before dependants.
    order: Vec<usize>,
    repos: Vec<Target>,
    reason_targets: Vec<Target>,
    explicit_names: Vec<String>,
}

impl<'a> Builder<'a> {
    fn with_root(db: &'a mut Alpm, root: String, lock: Option<std::fs::File>, requested: &[String]) -> Builder<'a> {
        Builder {
            db,
            root,
            _lock: lock,
            requested: requested.to_vec(),
            preserve_reasons: false,
            scope: Scope::All,
            aur_targets: Vec::new(),
            needed: false,
            sandbox: true,
            clean_after: false,
            rebuild_tree: false,
            reason: None,
            nodes: Vec::new(),
            order: Vec::new(),
            repos: Vec::new(),
            reason_targets: Vec::new(),
            explicit_names: Vec::new(),
        }
    }

    /// Open the user's build cache and take its lock, so that two builds
    /// never share a checkout.
    pub fn new(db: &'a mut Alpm, requested: &[String]) -> Result<Builder<'a>> {
        if unsafe { libc::geteuid() } == 0 {
            return errors::fail(Kind::BuildsMustNotRunAsRoot);
        }
        let home = std::env::var("HOME").map_err(|_| Kind::HomeNotSet)?;
        let cache = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| join(&home, ".cache"));
        if !cache.starts_with('/') {
            return Err(errors::with(Kind::InvalidCacheDirectory, cache));
        }
        let root = join(&cache, "zap");
        util::secure_dir(&root)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(join(&root, ".lock"))
            .map_err(|_| Kind::UnsafeCacheLock)?;
        let meta = lock.metadata().map_err(|_| Kind::UnsafeCacheLock)?;
        if meta.uid() != unsafe { libc::getuid() } || !meta.is_file() {
            return errors::fail(Kind::UnsafeCacheLock);
        }
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return errors::fail(Kind::AnotherBuildIsRunning);
        }
        Ok(Builder::with_root(db, root, Some(lock), requested))
    }

    // ── Planning ────────────────────────────────────────────────────────────

    /// Resolve every requested package and its dependencies, then show the plan.
    pub fn plan(&mut self) -> Result<()> {
        for index in 0..self.nodes.len() {
            if self.nodes[index].local_source {
                self.plan_local(index, 0)?;
            }
        }
        for name in self.requested.clone() {
            self.resolve(&name, true, 0)?;
        }
        // makepkg itself needs the base-devel group's tools.
        if !self.nodes.is_empty() {
            self.resolve("base-devel", false, 0)?;
        }
        self.show_plan();
        Ok(())
    }

    fn show_plan(&self) {
        let builds: usize = self.order.iter().map(|&i| self.nodes[i].wanted.len()).sum();
        let mut parts = Vec::new();
        if !self.repos.is_empty() {
            parts.push(ui::count(self.repos.len(), "repository package", "repository packages"));
        }
        if builds > 0 {
            parts.push(ui::count(builds, "package to build", "packages to build"));
        }
        if !self.reason_targets.is_empty() {
            parts.push(ui::count(self.reason_targets.len(), "reason change", "reason changes"));
        }
        if parts.is_empty() {
            return ui::note(NoteKind::Success, "Nothing to do.");
        }

        ui::heading("Plan", &parts.join(" · "));
        let mut table = Table::new(&[
            Column::new("Package").tone(Tone::Bold),
            Column::new("Current"),
            Column::new("New").tone(Tone::Success).joiner(" → "),
            Column::new("Change"),
            Column::new("Source"),
            Column::new("Reason").tone(Tone::Muted),
        ]);
        let mut download: i64 = 0;
        let mut growth: i64 = 0;
        let mut reinstalls: Vec<&str> = Vec::new();
        for target in &self.repos {
            let Some(p) = self.db.repo(&target.name) else { continue };
            let old = self.db.installed_pkg(&target.name);
            download += self.db.h.download_size(&p);
            growth += p.info().isize - old.map_or(0, |previous| previous.info().isize);
            if plan_row(&mut table, &target.name, old.map(|o| &**o), &p.version, alpm::repository(&p), !target.dependency) {
                reinstalls.push(&target.name);
            }
        }
        for &index in &self.order {
            let node = &self.nodes[index];
            let origin = if node.local_source { "local" } else { "aur" };
            for wanted in &node.wanted {
                let split = *wanted != node.package.PackageBase;
                let source = if split { format!("{origin} · base {}", node.package.PackageBase) } else { origin.to_string() };
                let old = self.db.installed_pkg(wanted).map(|o| &**o);
                if plan_row(&mut table, wanted, old, &node.package.Version, &source, contains(&self.explicit_names, wanted)) {
                    reinstalls.push(wanted);
                }
            }
        }
        table.render();
        ui::blank();
        if !self.repos.is_empty() {
            ui::field("Download", &if download == 0 { "nothing: already in the package cache".to_string() } else { ui::size(download) });
            ui::field("Disk space", &if growth == 0 { "no change".to_string() } else { format!("{} for repository packages", ui::size_delta(growth)) });
            ui::note(NoteKind::Info, "Repository dependencies, conflicts and replacements are settled in the final transaction, which you confirm separately.");
        }
        if builds > 0 {
            if self.sandbox {
                ui::note(NoteKind::Info, "Packages to build are reviewed, then built in a sandbox and sized before anything is installed.");
            } else {
                ui::note(NoteKind::Warning, "Sandbox disabled: approved build code will run with your user's full permissions.");
            }
        }
        for target in &self.reason_targets {
            let reason = reason_name(target.reason.unwrap_or(Reason::Explicit));
            ui::note(NoteKind::Info, &format!("{} is already current; it will be marked {reason}.", target.name));
        }
        if !reinstalls.is_empty() {
            let subject = if reinstalls.len() == 1 { format!("{} is", reinstalls[0]) } else { ui::count(reinstalls.len(), "package is", "packages are") };
            ui::note(NoteKind::Warning, &format!("{subject} already installed at this version. Add --needed to skip current packages."));
        }
        ui::blank();
    }

    /// `--needed`: leave a current package alone, but still honour a
    /// requested change of installation reason.
    fn skip_installed(&mut self, name: &str, version: &str, installed_reason: libalpm::Reason) {
        ui::note(NoteKind::Success, &format!("{name} {version} is already installed and current."));
        let reason = self.reason.unwrap_or(Reason::Explicit);
        let desired = if reason == Reason::Explicit { libalpm::Reason::Explicit } else { libalpm::Reason::Depend };
        if installed_reason != desired {
            self.reason_targets.push(Target { reason: Some(reason), ..Target::named(name) });
        }
    }

    /// The installed package of this name, when it is at least `version`:
    /// its version and installation reason.
    fn current(&self, name: &str, version: &str) -> Option<(String, libalpm::Reason)> {
        let old = self.db.installed_pkg(name)?;
        (vercmp(&old.version, version) >= 0).then(|| (old.version.clone(), old.info().reason))
    }

    fn add_repo(&mut self, name: &str, explicit: bool) {
        if let Some(target) = self.repos.iter_mut().find(|t| t.name == name) {
            if explicit {
                target.dependency = false;
                target.reason = self.reason;
            }
            return;
        }
        let reason = if explicit { self.reason } else { None };
        self.repos.push(Target { dependency: !explicit, reason, ..Target::named(name) });
    }

    /// Decide where a dependency comes from, in order of preference: a local
    /// snapshot supplied on the command line, an installed package, a
    /// repository, a package base already in the plan, then the AUR.
    fn resolve(&mut self, dep: &str, explicit: bool, depth: usize) -> Result<()> {
        if depth > 128 || self.nodes.len() > 512 {
            return errors::fail(Kind::DependencyGraphTooLarge);
        }
        if !valid_name(dep_name(dep)) {
            return Err(errors::with(Kind::InvalidDependency, dep));
        }

        for index in 0..self.nodes.len() {
            if !self.nodes[index].local_source {
                continue;
            }
            if let Some(name) = self.nodes[index].metadata.provider(dep).map(String::from) {
                self.plan_local(index, depth + 1)?;
                if explicit {
                    append_unique(&mut self.explicit_names, &name);
                }
                return Ok(());
            }
        }

        if !explicit {
            if let Some(installed) = self.db.local(dep) {
                // --rebuildtree rebuilds installed foreign dependencies only;
                // repository packages stay satisfied.
                if !self.rebuild_tree || !self.db.foreign(installed) {
                    return Ok(());
                }
            }
        }

        let from_aur = explicit && (self.scope == Scope::Aur || contains(&self.aur_targets, dep_name(dep)));
        if !from_aur {
            if let Some(p) = self.db.repo(dep) {
                if explicit {
                    append_unique(&mut self.explicit_names, &p.name);
                    if self.needed {
                        if let Some((version, reason)) = self.current(&p.name, &p.version) {
                            self.skip_installed(&p.name, &version, reason);
                            return Ok(());
                        }
                    }
                }
                self.add_repo(&p.name, explicit);
                return Ok(());
            }
        }
        if explicit && self.scope == Scope::Repo {
            return Err(errors::with(Kind::RepositoryPackageNotFound, dep));
        }

        for index in 0..self.nodes.len() {
            let Some(name) = self.nodes[index].metadata.provider(dep).map(String::from) else { continue };
            if self.nodes[index].local_source && !self.nodes[index].planned {
                self.plan_local(index, depth + 1)?;
            }
            if self.nodes[index].visiting {
                return Err(errors::with(Kind::DependencyCycle, dep));
            }
            if explicit {
                append_unique(&mut self.explicit_names, &name);
            }
            append_unique(&mut self.nodes[index].wanted, &name);
            return Ok(());
        }

        let p = choose_aur(dep)?;
        if explicit && self.needed {
            if let Some((version, reason)) = self.current(&p.Name, &p.Version) {
                self.skip_installed(&p.Name, &version, reason);
                return Ok(());
            }
        }
        if explicit {
            append_unique(&mut self.explicit_names, &p.Name);
        }
        if let Some(node) = self.nodes.iter_mut().find(|node| node.package.PackageBase == p.PackageBase) {
            if node.visiting {
                return Err(errors::with(Kind::DependencyCycle, dep));
            }
            append_unique(&mut node.wanted, &p.Name);
            return Ok(());
        }

        let dir = self.checkout(&p.PackageBase, true)?;
        let meta = self.read_srcinfo(&dir)?;
        // The checkout must be the package the RPC described.
        if meta.base != p.PackageBase || !contains(&meta.names, &p.Name) || !meta.matches(dep, &p.Name) {
            return Err(errors::with(Kind::AurMetadataMismatch, &p.PackageBase));
        }
        let index = self.nodes.len();
        let mut node = Node::new(p, dir, meta.clone());
        node.wanted.push(node.package.Name.clone());
        self.nodes.push(node);
        // For safety the plan covers the dependencies of every split package
        // in the base, since building the base builds them all.
        for dependency in &meta.deps {
            match meta.provider(dependency) {
                Some(sibling) => append_unique(&mut self.nodes[index].wanted, sibling),
                None => self.resolve(dependency, false, depth + 1)?,
            }
        }
        self.nodes[index].visiting = false;
        self.nodes[index].planned = true;
        self.order.push(index);
        Ok(())
    }

    fn read_srcinfo(&self, dir: &str) -> Result<Metadata> {
        let bytes = util::read_file(join(dir, ".SRCINFO"), SRCINFO_LIMIT)?;
        Metadata::parse(&String::from_utf8_lossy(&bytes), &self.db.cfg.arch)
    }

    // ── Local sources ───────────────────────────────────────────────────────

    /// Snapshot local PKGBUILD directories into the cache as reviewable
    /// commits. The originals are never sourced or modified.
    pub fn load_local<S: AsRef<str>>(&mut self, dirs: &[S]) -> Result<()> {
        for input in dirs {
            let input = input.as_ref();
            let source = std::fs::canonicalize(input).map_err(|e| errors::with(e, input))?.to_string_lossy().into_owned();
            let mut template = util::cstr(&join(&self.root, ".local-XXXXXX")).into_bytes_with_nul();
            if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
                return errors::fail(Kind::StagingFailed);
            }
            template.pop();
            let destination = String::from_utf8_lossy(&template).into_owned();
            self.add_local(&source, &destination).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&destination);
            })?;
            ui::note(NoteKind::Info, &format!("Snapshot of {source} taken for review."));
        }
        Ok(())
    }

    fn add_local(&mut self, source: &str, destination: &str) -> Result<()> {
        snapshot_local(source, destination)?;
        let meta = self.read_srcinfo(destination)?;
        if self.nodes.iter().any(|node| node.metadata.base == meta.base) {
            return Err(errors::with(Kind::DuplicateTarget, &meta.base));
        }
        for name in &meta.names {
            if contains(&self.explicit_names, name) {
                return Err(errors::with(Kind::DuplicateTarget, name));
            }
            append_unique(&mut self.explicit_names, name);
        }
        git(&["init", "--quiet"], Some(destination))?;
        git(&["add", "--force", "--all", "--", "."], Some(destination))?;
        git(&["-c", "user.name=zap local review", "-c", "user.email=local@example.invalid", "commit", "--quiet", "-m", "Local source snapshot"], Some(destination))?;
        let package = Package {
            Name: meta.names[0].clone(),
            PackageBase: meta.base.clone(),
            Version: meta.version.clone(),
            Maintainer: Some("local source".into()),
            ..Package::default()
        };
        let mut node = Node::new(package, destination.into(), meta);
        node.wanted = node.metadata.names.clone();
        node.visiting = false;
        node.local_source = true;
        self.nodes.push(node);
        Ok(())
    }

    fn plan_local(&mut self, index: usize, depth: usize) -> Result<()> {
        if depth > 128 {
            return errors::fail(Kind::DependencyGraphTooLarge);
        }
        if self.nodes[index].planned {
            return Ok(());
        }
        if self.nodes[index].visiting {
            return Err(errors::with(Kind::DependencyCycle, &self.nodes[index].metadata.base));
        }
        self.nodes[index].visiting = true;
        let meta = self.nodes[index].metadata.clone();
        for dep in &meta.deps {
            if meta.provider(dep).is_none() {
                self.resolve(dep, false, depth + 1)?;
            }
        }
        self.nodes[index].visiting = false;
        self.nodes[index].planned = true;
        self.order.push(index);
        Ok(())
    }

    // ── Sources ─────────────────────────────────────────────────────────────

    /// Clone a package base, or fast-forward the cached clone, showing what
    /// changed since the cached copy when `announce` is set. No package code
    /// runs here.
    fn checkout(&self, base: &str, announce: bool) -> Result<String> {
        let dir = join(&self.root, base);
        let url = format!("https://aur.archlinux.org/{base}.git");
        match std::fs::symlink_metadata(&dir) {
            Err(_) => {
                git(&["clone", "--", &url, &dir], None)?;
                util::secure_dir(&dir)?;
            }
            Ok(meta) => {
                if meta.uid() != unsafe { libc::getuid() } || !meta.is_dir() {
                    return Err(errors::with(Kind::UnsafeCacheDirectory, &dir));
                }
                if !git(&["status", "--porcelain", "--untracked-files=no"], Some(&dir))?.is_empty() {
                    return Err(errors::with(Kind::ModifiedBuildFiles, &dir));
                }
                let before = git_line(&["rev-parse", "HEAD"], &dir)?;
                git(&["fetch", "--", &url, "master"], Some(&dir))?;
                git(&["merge", "--ff-only", "FETCH_HEAD"], Some(&dir))?;
                let approved = git_line(&["for-each-ref", "--format=%(objectname)", APPROVED_REF], &dir)?;
                let head = git_line(&["rev-parse", "HEAD"], &dir)?;
                let baseline = if approved.is_empty() { &before } else { &approved };
                if announce && *baseline != head {
                    let diff = git(&["diff", "--no-ext-diff", "--no-textconv", baseline, &head, "--"], Some(&dir))?;
                    if !diff.is_empty() {
                        let since = if approved.is_empty() { "your cached copy" } else { "you last approved it" };
                        ui::rule(&format!("{base} · changes since {since}"));
                        print_diff(&diff);
                    }
                }
            }
        }
        // Refuse linked or foreign metadata before any PKGBUILD code can run.
        for file in ["PKGBUILD", ".SRCINFO"] {
            let path = join(&dir, file);
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && meta.uid() == unsafe { libc::getuid() } => {}
                _ => return Err(errors::with(Kind::UnsafeBuildFile, path)),
            }
        }
        Ok(dir)
    }

    /// `get`: fetch sources into the cache without executing anything.
    pub fn get_sources(&self) -> Result<()> {
        for name in &self.requested {
            let dir = self.checkout(&aur_base(name)?, true)?;
            ui::note(NoteKind::Success, &format!("{name} · sources are in {dir}"));
        }
        Ok(())
    }

    /// `pkgbuild`: print PKGBUILDs, and nothing else, to standard output.
    pub fn print_pkgbuilds(&self) -> Result<()> {
        for name in &self.requested {
            let dir = self.checkout(&aur_base(name)?, false)?;
            print_source(&util::read_file(join(&dir, "PKGBUILD"), SOURCE_LIMIT)?);
        }
        Ok(())
    }

    // ── Review and build ────────────────────────────────────────────────────

    fn makepkg_args(&self, dir: &str, args: &[&str]) -> Result<Vec<String>> {
        if self.sandbox {
            sandbox::argv(dir, &self.db.cfg.db, args)
        } else {
            Ok(args.iter().map(|s| s.to_string()).collect())
        }
    }

    /// Review, build and, when `install` is set, install the plan.
    pub fn execute(&mut self, install: bool) -> Result<()> {
        // Everything needed to install is checked before any review or build.
        // An empty plan has already said "Nothing to do." and needs none of it.
        if install && self.repos.is_empty() && self.order.is_empty() && self.reason_targets.is_empty() {
            return Ok(());
        }
        if install {
            transaction::preflight()?;
        } else {
            self.require_installed_dependencies()?;
        }
        if self.sandbox && !self.order.is_empty() {
            // Probe before any approval or installation. A sandbox that cannot
            // start is an error; it never falls back to an unsandboxed build.
            let probe = self.makepkg_args(&self.nodes[self.order[0]].dir, &["/usr/bin/true"]).map_err(|_| Kind::BuildSandboxUnavailable)?;
            util::run(&probe, None).map_err(|_| Kind::BuildSandboxUnavailable)?;
            ui::note(NoteKind::Info, "Builds run in a sandbox: private home, read-only system and package database, network for downloads only.");
        } else if !self.order.is_empty() {
            ui::note(NoteKind::Warning, "Sandbox disabled: approved build code runs with your user's full permissions.");
        }

        // Every base is reviewed before any PKGBUILD code runs or the system changes.
        let total = self.order.len();
        for (step, &index) in self.order.iter().enumerate() {
            review(&mut self.nodes[index], step + 1, total)?;
        }

        // Sourcing the PKGBUILD is the first execution of package code. The
        // plan came from .SRCINFO, so the two must agree.
        for &index in &self.order {
            let node = &self.nodes[index];
            verify_source(node)?;
            let generated = util::capture(&self.makepkg_args(&node.dir, &["/usr/bin/makepkg", "--printsrcinfo"])?, Some(&node.dir), SRCINFO_LIMIT)?;
            let actual = Metadata::parse(&String::from_utf8_lossy(&generated), &self.db.cfg.arch)?;
            if !node.metadata.equivalent(&actual) {
                return Err(errors::with(Kind::StaleSrcInfo, &node.package.PackageBase));
            }
            verify_source(node)?;
        }

        if install && !self.reason_targets.is_empty() {
            transaction::escalate(&Request { targets: self.reason_targets.clone(), ..Request::new(Operation::Reason) })?;
        }
        if install && !self.repos.is_empty() {
            transaction::escalate(&Request { targets: self.repos.clone(), needed: self.needed, ..Request::new(Operation::Install) })?;
            self.db.reload()?;
        }
        // Each base is installed before its dependants are built.
        for step in 0..self.order.len() {
            self.build(self.order[step], step + 1, install)?;
        }
        Ok(())
    }

    /// `build` changes nothing on the system, so everything must be in place.
    pub fn require_installed_dependencies(&self) -> Result<()> {
        if self.nodes.is_empty() {
            return errors::fail(Kind::BuildRequiresAurPackage);
        }
        for target in &self.repos {
            if self.db.local(&target.name).is_none() {
                return Err(errors::with(Kind::MissingBuildDependencies, &target.name));
            }
            if !target.dependency {
                return Err(errors::with(Kind::BuildRequiresAurPackage, format!("{} is a repository package", target.name)));
            }
        }
        for node in &self.nodes {
            for dep in &node.metadata.deps {
                if node.metadata.provider(dep).is_none() && self.db.local(dep).is_none() {
                    return Err(errors::with(Kind::MissingBuildDependencies, dep));
                }
            }
        }
        Ok(())
    }

    fn build(&mut self, index: usize, step: usize, install: bool) -> Result<()> {
        let node = self.nodes[index].clone();
        let base = &node.package.PackageBase;
        if !node.reviewed {
            return Err(errors::with(Kind::SourceNotReviewed, base));
        }
        verify_source(&node)?;
        ui::heading(&format!("Build {step}/{} · {base}", self.order.len()), &node.package.Version);
        util::run(&self.makepkg_args(&node.dir, &["/usr/bin/makepkg", "--cleanbuild", "--clean", "--force", "--noconfirm"])?, Some(&node.dir))?;
        verify_source(&node)?;
        let paths = util::capture(&self.makepkg_args(&node.dir, &["/usr/bin/makepkg", "--packagelist"])?, Some(&node.dir), SRCINFO_LIMIT)?;
        let options = artifacts::Options {
            base,
            dir: &node.dir,
            wanted: &node.wanted,
            explicit_names: &self.explicit_names,
            preserve_reasons: self.preserve_reasons,
        };
        let mut built = artifacts::collect(self.db, &options, &String::from_utf8_lossy(&paths))?;
        verify_source(&node)?;
        ui::blank();
        for (target, version) in built.targets.iter().zip(&built.versions) {
            ui::note(NoteKind::Success, &format!("Built {} {version}", target.name));
            ui::field_raw("  Archive", target.archive.as_deref().unwrap_or(""));
            ui::field_raw("  SHA-256", target.sha256.as_deref().unwrap_or(""));
        }
        for target in &mut built.targets {
            if contains(&self.explicit_names, &target.name) {
                target.reason = self.reason;
            }
        }
        if install {
            transaction::escalate(&Request { targets: built.targets, needed: self.needed, ..Request::new(Operation::Install) })?;
            self.db.reload()?;
        }
        // makepkg may rewrite a literal pkgver after a VCS build. Only that
        // validated change is restored; other tracked edits fail verification.
        git(&["restore", "--source=HEAD", "--", "PKGBUILD"], Some(&node.dir))?;
        if self.clean_after && install {
            git(&["clean", "-fdx", "--"], Some(&node.dir))?;
        }
        Ok(())
    }

    // ── Cache ───────────────────────────────────────────────────────────────

    /// `clean`: delete the named package bases, or the whole cache.
    pub fn clean(&self, dry: bool) -> Result<()> {
        if let Some(base) = self.requested.iter().find(|base| !valid_name(base)) {
            return Err(errors::with(Kind::InvalidPackageName, base));
        }
        if self.requested.is_empty() {
            ui::heading("Clean build cache", "everything");
            ui::line(&self.root, 2, Tone::Plain);
        } else {
            ui::heading("Clean build cache", &ui::count(self.requested.len(), "package base", "package bases"));
            for base in &self.requested {
                ui::line(&join(&self.root, base), 2, Tone::Plain);
            }
        }
        ui::blank();
        if dry {
            return Ok(());
        }
        ui::require("Delete these cached sources and build outputs? [y/N]")?;
        if self.requested.is_empty() {
            for entry in std::fs::read_dir(&self.root)? {
                let entry = entry?;
                if entry.file_name() == ".lock" {
                    continue;
                }
                remove(&entry.path(), entry.file_type()?.is_dir())?;
            }
        } else {
            for base in &self.requested {
                let path = std::path::Path::new(&self.root).join(base);
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) => remove(&path, meta.is_dir())?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        ui::note(NoteKind::Success, "Build cache cleaned.");
        Ok(())
    }
}

fn remove(path: &std::path::Path, dir: bool) -> Result<()> {
    Ok(if dir { std::fs::remove_dir_all(path)? } else { std::fs::remove_file(path)? })
}

fn reason_name(reason: Reason) -> &'static str {
    match reason {
        Reason::Explicit => "explicit",
        Reason::Dependency => "dependency",
    }
}

/// One plan row. Returns whether it reinstalls the installed version.
fn plan_row(table: &mut Table, name: &str, old: Option<&Pkg>, version: &str, source: &str, requested: bool) -> bool {
    let (kind, tone) = match old {
        Some(previous) => match vercmp(version, &previous.version) {
            1.. => ("upgrade", Tone::Accent),
            ..=-1 => ("downgrade", Tone::Warning),
            0 => ("reinstall", Tone::Warning),
        },
        None => ("install", Tone::Accent),
    };
    let same = old.is_some_and(|previous| previous.version == version);
    table.add(vec![
        Cell::new(name),
        Cell::new(old.map_or("—", |previous| previous.version.as_str())),
        // A version that does not change is not shown as new.
        Cell::maybe(version, same.then_some(Tone::Plain)),
        Cell::toned(kind, tone),
        Cell::toned(source, if source.starts_with("aur") { Tone::Special } else { Tone::Accent }),
        Cell::new(if requested { "requested" } else { "dependency" }),
    ]);
    kind == "reinstall"
}

/// The AUR package for a dependency: the package of that name if it
/// satisfies the version, otherwise a provider, chosen by the user when
/// several qualify.
fn choose_aur(dep: &str) -> Result<Package> {
    let name = dep_name(dep);
    if let Some(exact) = aur::find(name)? {
        if srcinfo::satisfies(dep, &exact.Name, &exact.Version, &exact.Provides) {
            return Ok(exact);
        }
    }
    // Search results lack provides and versions: fetch full metadata for
    // every candidate in one batched request.
    let candidates = aur::search(name, SearchBy::Provides)?;
    let names: Vec<&str> = candidates.iter().map(|c| c.Name.as_str()).collect();
    let mut providers: Vec<Package> = aur::info(&names)?.into_iter().filter(|d| srcinfo::satisfies(dep, &d.Name, &d.Version, &d.Provides)).collect();
    providers.sort_by(|a, b| b.NumVotes.cmp(&a.NumVotes).then_with(|| a.Name.cmp(&b.Name)));
    match providers.len() {
        0 => return Err(errors::with(Kind::UnsatisfiedDependency, dep)),
        1 => return Ok(providers.remove(0)),
        _ => {}
    }
    ui::heading("Choose a provider", dep);
    for (number, p) in providers.iter().enumerate() {
        ui::entry(&ui::Entry {
            index: Some(number + 1),
            source: "aur".into(),
            name: p.Name.clone(),
            version: p.Version.clone(),
            description: p.Description.clone().unwrap_or_default(),
            meta: format!("{} · popularity {}", ui::count(p.NumVotes.max(0) as usize, "vote", "votes"), ui::fixed(p.Popularity, 2)),
            ..ui::Entry::default()
        });
    }
    let reply = ui::answer("Provider number:")?;
    match reply.parse::<usize>() {
        Ok(choice) if (1..=providers.len()).contains(&choice) => Ok(providers.swap_remove(choice - 1)),
        _ => Err(errors::with(Kind::InvalidProvider, reply)),
    }
}

fn aur_base(name: &str) -> Result<String> {
    match aur::find(name)? {
        Some(p) => Ok(p.PackageBase),
        None => Err(errors::with(Kind::AurPackageNotFound, name)),
    }
}

/// Run git with hooks, file transports and filesystem monitors disabled.
fn git(args: &[&str], dir: Option<&str>) -> Result<Vec<u8>> {
    let mut argv = vec!["/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-c", "protocol.file.allow=never", "-c", "core.fsmonitor=false"];
    argv.extend_from_slice(args);
    util::capture(&argv, dir, SOURCE_LIMIT)
}

/// Git output with surrounding newlines trimmed, such as a commit id.
fn git_line(args: &[&str], dir: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&git(args, Some(dir))?).trim_matches('\n').to_string())
}

/// A fingerprint of every tracked file. `strict` also requires a clean work
/// tree. A literal pkgver is normalized, because makepkg rewrites it in place
/// for VCS packages.
fn source_digest(dir: &str, strict: bool) -> Result<String> {
    use sha2::Digest;
    let status = git(&["status", "--porcelain", "--untracked-files=no"], Some(dir))?;
    if strict && !status.is_empty() {
        return Err(errors::with(Kind::ModifiedBuildFiles, dir));
    }
    let mut hash = sha2::Sha256::new();
    let files = git(&["ls-files", "-z"], Some(dir))?;
    hash.update(&files);
    for path in files.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path);
        if path.starts_with('/') || path.contains("../") {
            return Err(errors::with(Kind::UnsafeBuildFile, path.as_ref()));
        }
        let file = join(dir, &path);
        let digest = transaction::hash_source_file(&file)?;
        if path == "PKGBUILD" {
            hash.update(normalize_pkgver(&util::read_file(&file, SOURCE_LIMIT)?));
        } else {
            hash.update(digest.as_bytes());
        }
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn verify_source(node: &Node) -> Result<()> {
    let base = &node.package.PackageBase;
    let approved = node.source_digest.as_deref().ok_or_else(|| errors::with(Kind::SourceNotReviewed, base))?;
    if approved != source_digest(&node.dir, false)? {
        return Err(errors::with(Kind::BuildSourceChanged, base));
    }
    Ok(())
}

fn review(node: &mut Node, step: usize, total: usize) -> Result<()> {
    let p = &node.package;
    ui::heading(&format!("Review {step}/{total} · {}", p.PackageBase), &p.Version);
    if node.local_source {
        ui::field("Source", "local snapshot");
    } else {
        ui::field("Source", &format!("https://aur.archlinux.org/packages/{}", p.Name));
        ui::field("Maintainer", p.Maintainer.as_deref().unwrap_or("none (orphaned)"));
        ui::field("Last modified", &ui::dated(p.LastModified));
        ui::field("Votes", &format!("{} · popularity {}", p.NumVotes, ui::fixed(p.Popularity, 2)));
        if let Some(flagged) = p.OutOfDate {
            ui::note(NoteKind::Warning, &format!("Flagged out of date on {}.", ui::date(flagged)));
        }
        if p.Maintainer.is_none() {
            ui::note(NoteKind::Warning, "Orphaned: nobody maintains this package.");
        }
    }
    node.source_digest = Some(source_digest(&node.dir, true)?);
    let files = git(&["ls-files", "-z"], Some(&node.dir))?;
    for path in files.split(|&b| b == 0).filter(|p| !p.is_empty()) {
        let path = String::from_utf8_lossy(path);
        let content = git(&["show", &format!("HEAD:{path}")], Some(&node.dir))?;
        ui::rule(&path);
        print_source(&content);
    }
    ui::blank();
    ui::require(&format!("These build files can run arbitrary code as your user. Approve {}? [y/N]", node.package.PackageBase))?;
    verify_source(node)?;
    node.reviewed = true;
    // Later diffs start from what was approved here.
    if !node.local_source {
        git(&["update-ref", APPROVED_REF, "HEAD"], Some(&node.dir))?;
    }
    Ok(())
}

fn trim_end_newlines(content: &[u8]) -> &[u8] {
    let end = content.iter().rposition(|&b| b != b'\n').map_or(0, |i| i + 1);
    &content[..end]
}

/// Print file contents for review: newlines kept, everything else that could
/// control the terminal neutralized, tabs expanded.
fn print_source(content: &[u8]) {
    ui::hold();
    for line in trim_end_newlines(content).split(|&b| b == b'\n') {
        ui::line_bytes(&expand_tabs(line), 0, Tone::Plain);
    }
    ui::release();
}

/// Like `print_source`, with additions and removals colored.
fn print_diff(content: &[u8]) {
    ui::hold();
    for line in trim_end_newlines(content).split(|&b| b == b'\n') {
        let tone = if line.starts_with(b"+++") || line.starts_with(b"---") || line.starts_with(b"diff ") {
            Tone::Bold
        } else if line.starts_with(b"@@") {
            Tone::Accent
        } else if line.starts_with(b"+") {
            Tone::Success
        } else if line.starts_with(b"-") {
            Tone::Danger
        } else {
            Tone::Plain
        };
        ui::line_bytes(&expand_tabs(line), 0, tone);
    }
    ui::release();
}

fn expand_tabs(line: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    if !line.contains(&b'\t') {
        return line.into();
    }
    let mut out = Vec::with_capacity(line.len() + 16);
    for &b in line {
        if b == b'\t' {
            out.extend_from_slice(b"    ");
        } else {
            out.push(b);
        }
    }
    out.into()
}

fn normalize_pkgver(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len() + 16);
    for line in bytes.split(|&b| b == b'\n') {
        let mut normalized = line;
        if let Some(mut value) = line.strip_prefix(b"pkgver=") {
            if value.len() >= 2 && ((value[0] == b'\'' && value[value.len() - 1] == b'\'') || (value[0] == b'"' && value[value.len() - 1] == b'"')) {
                value = &value[1..value.len() - 1];
            }
            if !value.is_empty() && value.iter().all(|&ch| ch.is_ascii_alphanumeric() || b"._+".contains(&ch)) {
                normalized = b"pkgver=<literal>";
            }
        }
        output.extend_from_slice(normalized);
        output.push(b'\n');
    }
    output
}

// Local build files are copied to a fresh review snapshot. Git metadata and
// makepkg work trees are excluded; every other file is tracked for review.
fn snapshot_local(source: &str, destination: &str) -> Result<()> {
    let (mut count, mut total) = (0usize, 0u64);
    copy_local_tree(source, destination, "", 0, &mut count, &mut total)?;
    for file in ["PKGBUILD", ".SRCINFO"] {
        transaction::hash_source_file(&join(destination, file))?;
    }
    Ok(())
}

fn copy_local_tree(source: &str, destination: &str, prefix: &str, depth: usize, count: &mut usize, total: &mut u64) -> Result<()> {
    use std::io::{Read, Write};
    if depth > 32 {
        return errors::fail(Kind::LocalSourceTooLarge);
    }
    let dir = join(source, prefix);
    // Refuse a directory that was swapped for a link since it was listed.
    if std::fs::symlink_metadata(&dir)?.file_type().is_symlink() {
        return errors::fail(Kind::UnsafeBuildFile);
    }
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let relative = join(prefix, &entry.file_name().to_string_lossy());
        if excluded_local(&relative) {
            continue;
        }
        *count += 1;
        if *count > 1024 {
            return errors::fail(Kind::LocalSourceTooLarge);
        }
        if !crate::query::valid_file(&relative) {
            return errors::fail(Kind::UnsafeBuildFile);
        }
        let (input, output) = (join(source, &relative), join(destination, &relative));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            std::fs::create_dir_all(&output)?;
            copy_local_tree(source, destination, &relative, depth + 1, count, total)?;
        } else if kind.is_file() {
            // Open without following symlinks and keep the descriptor while copying.
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&input)
                .map_err(|_| Kind::UnsafeBuildFile)?;
            let meta = file.metadata()?;
            if !meta.is_file() {
                return errors::fail(Kind::UnsafeBuildFile);
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut file).take(SOURCE_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > SOURCE_LIMIT {
                return errors::fail(Kind::FileTooBig);
            }
            *total += bytes.len() as u64;
            if *total > 64 * 1024 * 1024 {
                return errors::fail(Kind::LocalSourceTooLarge);
            }
            let mut out = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600 | (meta.mode() & 0o111)).open(&output)?;
            out.write_all(&bytes)?;
        } else {
            return errors::fail(Kind::UnsafeBuildFile);
        }
    }
    Ok(())
}

fn excluded_local(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    first == "src" || first == "pkg" || path.split('/').any(|part| part == ".git") || first.contains(".pkg.tar")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, TempDir};

    /// Answer the next prompts with `reply` by standing a pipe in for stdin.
    struct Stdin {
        saved: libc::c_int,
    }

    impl Stdin {
        fn replying(reply: &str) -> Stdin {
            let mut fds = [0; 2];
            unsafe {
                assert_eq!(0, libc::pipe(fds.as_mut_ptr()));
                assert_eq!(reply.len() as isize, libc::write(fds[1], reply.as_ptr().cast(), reply.len()));
                libc::close(fds[1]);
                let saved = libc::dup(0);
                libc::dup2(fds[0], 0);
                libc::close(fds[0]);
                Stdin { saved }
            }
        }
    }

    impl Drop for Stdin {
        fn drop(&mut self) {
            unsafe {
                libc::dup2(self.saved, 0);
                libc::close(self.saved);
            }
        }
    }

    /// Point HOME elsewhere until dropped, isolating makepkg's user configuration.
    struct Home(Option<std::ffi::OsString>);

    impl Home {
        fn set(path: &str) -> Home {
            let old = std::env::var_os("HOME");
            std::env::set_var("HOME", path);
            Home(old)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            match self.0.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn git_commit(dir: &str) {
        git(&["init", "--quiet"], Some(dir)).unwrap();
        git(&["add", "--", "PKGBUILD", ".SRCINFO"], Some(dir)).unwrap();
        git(&["-c", "user.name=zap tests", "-c", "user.email=tests@example.invalid", "commit", "--quiet", "-m", "fixture"], Some(dir)).unwrap();
    }

    #[test]
    fn source_fingerprint_permits_literal_vcs_pkgver_updates_and_preserves_executable_changes() {
        let old = normalize_pkgver(b"pkgver=1.0\nbuild() { echo ok; }");
        assert_eq!(old, normalize_pkgver(b"pkgver='2.0.r4'\nbuild() { echo ok; }"));
        assert_ne!(old, normalize_pkgver(b"pkgver=2.0;evil\nbuild() { echo ok; }"));
        assert_ne!(old, normalize_pkgver(b"pkgver=2.0\nbuild() { echo evil; }"));
    }

    #[test]
    fn review_and_build_a_local_fixture_without_escalation_or_installation() {
        let _lock = ui::test_lock();
        let tmp = TempDir::new();
        let root = tmp.str();
        let _home = Home::set(root);
        // Stock Arch enables these options; a text-only fixture has no debug archive.
        std::fs::write(tmp.path().join(".makepkg.conf"), "OPTIONS+=(strip debug)\n").unwrap();
        let pkgbuild = "pkgname=zap-build-fixture\npkgver=1.0\npkgrel=1\npkgdesc='Isolated zap build test'\narch=('any')\nlicense=('MIT')\npackage() {\n  install -d \"$pkgdir/usr/share/zap-build-fixture\"\n  printf 'fixture\\n' > \"$pkgdir/usr/share/zap-build-fixture/example\"\n}\n";
        std::fs::write(tmp.path().join("PKGBUILD"), pkgbuild).unwrap();
        let generated = util::capture(&["/usr/bin/makepkg", "--printsrcinfo"], Some(root), 1024 * 1024).unwrap();
        std::fs::write(tmp.path().join(".SRCINFO"), &generated).unwrap();
        git_commit(root);
        let mut db = testing::alpm(tmp.path());
        let mut builds = Builder::with_root(&mut db, root.into(), None, &["zap-build-fixture".into()]);
        builds.sandbox = false;
        let package = Package { Name: "zap-build-fixture".into(), PackageBase: "zap-build-fixture".into(), Version: "1.0-1".into(), Maintainer: Some("fixture".into()), ..Package::default() };
        let mut node = Node::new(package, root.into(), Metadata::parse(&String::from_utf8_lossy(&generated), "x86_64").unwrap());
        node.wanted.push("zap-build-fixture".into());
        node.visiting = false;
        builds.nodes.push(node);
        builds.order.push(0);
        builds.explicit_names.push("zap-build-fixture".into());
        // Supply the single source approval to this isolated fixture only.
        let stdin = Stdin::replying("y\n");
        ui::begin_capture();
        let outcome = builds.execute(false);
        ui::end_capture();
        drop(stdin);
        outcome.unwrap();
        assert!(db.local("zap-build-fixture").is_none());
        let paths = String::from_utf8(util::capture(&["/usr/bin/makepkg", "--packagelist"], Some(root), 1024 * 1024).unwrap()).unwrap();
        let listed: Vec<&str> = paths.split('\n').filter(|l| !l.is_empty()).collect();
        assert_eq!(2, listed.len(), "{paths}");
        assert!(listed[1].contains("-debug-"));
        assert!(!std::path::Path::new(listed[1]).exists());
        assert_eq!(64, transaction::hash_file(listed[0]).unwrap().len());
    }

    #[test]
    fn local_source_snapshots_keep_patches_and_reject_symlinks() {
        let tmp = TempDir::new();
        let at = |p: &str| tmp.path().join(p);
        for dir in ["original/src", "original/patches", "copy", "copy2"] {
            std::fs::create_dir_all(at(dir)).unwrap();
        }
        for (path, data) in [("original/PKGBUILD", "pkgname=fixture"), ("original/.SRCINFO", "pkgbase = fixture"), ("original/patches/fix.patch", "patch"), ("original/src/ignored", "output")] {
            std::fs::write(at(path), data).unwrap();
        }
        let (source, dest) = (format!("{}/original", tmp.str()), format!("{}/copy", tmp.str()));
        snapshot_local(&source, &dest).unwrap();
        assert!(at("copy/patches/fix.patch").exists());
        assert!(!at("copy/src/ignored").exists());
        std::os::unix::fs::symlink("PKGBUILD", at("original/alias")).unwrap();
        let refused = snapshot_local(&source, &format!("{}/copy2", tmp.str())).unwrap_err();
        assert_eq!(Kind::UnsafeBuildFile, refused.kind);
    }

    #[test]
    fn local_package_build_runs_every_makepkg_phase_in_the_sandbox() {
        let _lock = ui::test_lock();
        let tmp = TempDir::new();
        let root = tmp.str();
        let (cache, source) = (format!("{root}/cache"), format!("{root}/local"));
        std::fs::create_dir(&cache).unwrap();
        std::fs::create_dir(&source).unwrap();
        std::fs::write(tmp.path().join("private"), "secret").unwrap();
        let pkgbuild = format!("pkgname=zap-isolated-fixture\npkgver=1\npkgrel=1\narch=('any')\nlicense=('MIT')\ntest ! -e '{root}/private' || exit 1\nif touch .git/injected 2>/dev/null; then exit 1; fi\npackage() {{ mkdir -p \"$pkgdir/usr/share/zap-isolated-fixture\"; echo isolated > \"$pkgdir/usr/share/zap-isolated-fixture/result\"; }}\n");
        std::fs::write(format!("{source}/PKGBUILD"), &pkgbuild).unwrap();
        std::fs::write(format!("{source}/.SRCINFO"), "pkgbase = zap-isolated-fixture\npkgver = 1\npkgrel = 1\narch = any\npkgname = zap-isolated-fixture\n").unwrap();
        let mut db = testing::alpm(tmp.path());
        db.cfg.db = format!("{root}/db");
        let mut builds = Builder::with_root(&mut db, cache, None, &[]);
        ui::begin_capture();
        builds.load_local(&[&source]).unwrap();
        // No host queries or installations are needed for this dependency-free fixture.
        builds.plan_local(0, 0).unwrap();
        let stdin = Stdin::replying("y\n");
        let outcome = builds.execute(false);
        drop(stdin);
        ui::end_capture();
        outcome.unwrap();
        assert!(db.local("zap-isolated-fixture").is_none());
        // The original checkout was never sourced or rewritten by the build.
        assert_eq!(pkgbuild, std::fs::read_to_string(format!("{source}/PKGBUILD")).unwrap());
    }

    #[test]
    fn local_dependency_planning_orders_supplied_bases_and_detects_cycles() {
        let tmp = TempDir::new();
        let mut db = testing::alpm(tmp.path());
        let mut builds = Builder::with_root(&mut db, tmp.str().into(), None, &[]);
        let a = Metadata::parse("pkgbase = first\npkgver = 1\npkgrel = 1\npkgname = first\ndepends = second>=1\n", "any").unwrap();
        let b = Metadata::parse("pkgbase = second\npkgver = 1\npkgrel = 1\npkgname = second\n", "any").unwrap();
        for meta in [a, b] {
            let package = Package { Name: meta.base.clone(), PackageBase: meta.base.clone(), Version: meta.version.clone(), ..Package::default() };
            let mut node = Node::new(package, tmp.str().into(), meta);
            node.visiting = false;
            node.local_source = true;
            builds.nodes.push(node);
        }
        builds.plan_local(0, 0).unwrap();
        assert_eq!(vec![1, 0], builds.order);
        builds.order.clear();
        for node in &mut builds.nodes {
            node.planned = false;
            node.visiting = false;
        }
        builds.nodes[1].metadata.deps.push("first".into());
        assert_eq!(Kind::DependencyCycle, builds.plan_local(0, 0).unwrap_err().kind);
    }
}
