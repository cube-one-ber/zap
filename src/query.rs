//! Read-only views of the repositories, the AUR and the installed system.
use crate::alpm::{self, Alpm, PkgRef};
use crate::aur;
use crate::cli::{Command, Options, Scope, SortBy};
use crate::errors::{self, Kind, Result};
use crate::libalpm::{db::find_first, db::parallel_map, vercmp, Pkg};
use crate::ui::{self, Badge, Cell, Column, Entry, NoteKind, Table, Tone};
use crate::util;
use std::cmp::Ordering;

// ── Search ──────────────────────────────────────────────────────────────────

pub struct Match {
    pub name: String,
    pub source: Scope,
}

/// Run `f` with output held, so a long listing is written at once.
fn held<T>(f: impl FnOnce() -> T) -> T {
    ui::hold();
    let result = f();
    ui::release();
    result
}

/// Search the repositories and the AUR as the scope allows. Results are
/// numbered repositories first; `select` shows them with number 1 last, so
/// the best matches sit next to the prompt.
pub fn search(db: &Alpm, o: &Options) -> Result<Vec<Match>> {
    let term = o.names.join(" ");
    let numbered = o.command == Command::Select;
    // The AUR request and the repository scan overlap.
    let (repo_packages, aur_packages) = std::thread::scope(|s| {
        let remote = (o.scope != Scope::Repo).then(|| s.spawn(|| aur::search(&term, o.search_by)));
        let local = if o.scope != Scope::Aur { repository_search(db, &term, false) } else { Ok(Vec::new()) };
        let remote = remote.map(|h| h.join().expect("AUR search panicked")).unwrap_or(Ok(Vec::new()));
        (local, remote)
    });
    let mut repo_packages = repo_packages?;
    let mut aur_packages = aur_packages?;
    // The name you typed comes first, then names that start with it; the
    // sort is stable, so repositories keep their configured order after that.
    repo_packages.sort_by_key(|p| closeness(&p.name, &term));
    aur_packages.sort_by(|a, b| aur_order(a, b, &term, o.sort_by));

    let matches: Vec<Match> = repo_packages
        .iter()
        .map(|p| Match { name: p.name.clone(), source: Scope::Repo })
        .chain(aur_packages.iter().map(|p| Match { name: p.Name.clone(), source: Scope::Aur }))
        .collect();
    if o.quiet {
        held(|| matches.iter().for_each(|m| ui::print(&format!("{}\n", ui::safe(&m.name)))));
        return Ok(matches);
    }

    let sections = if numbered { [Scope::Aur, Scope::Repo] } else { [Scope::Repo, Scope::Aur] };
    for section in sections {
        if section == Scope::Repo && o.scope != Scope::Aur {
            ui::heading("Repositories", &ui::count(repo_packages.len(), "match", "matches"));
            for i in 0..repo_packages.len() {
                let at = if numbered { repo_packages.len() - 1 - i } else { i };
                ui::entry(&repo_entry(db, &repo_packages[at], numbered.then_some(at + 1)));
            }
            if repo_packages.is_empty() {
                ui::note(NoteKind::Info, "Nothing in the configured repositories matches.");
            }
        }
        if section == Scope::Aur && o.scope != Scope::Repo {
            let detail = format!(
                "{} · closest names first, then by {}",
                ui::count(aur_packages.len(), "match", "matches"),
                o.sort_by.name()
            );
            ui::heading("AUR", &detail);
            for i in 0..aur_packages.len() {
                let at = if numbered { aur_packages.len() - 1 - i } else { i };
                ui::entry(&aur_entry(db, &aur_packages[at], numbered.then_some(repo_packages.len() + at + 1)));
            }
            if aur_packages.is_empty() {
                ui::note(NoteKind::Info, "Nothing in the AUR matches.");
            }
        }
    }
    ui::blank();
    if matches.is_empty() {
        ui::note(NoteKind::Info, "Try a shorter or broader term.");
    } else if !numbered {
        ui::note(NoteKind::Info, "zap info NAME shows details · zap select TERM installs from this list.");
    }
    Ok(matches)
}

/// 0 for the exact name, 1 for a name that starts with the term, 2 otherwise.
fn closeness(name: &str, term: &str) -> u8 {
    if name == term {
        0
    } else if name.starts_with(term) {
        1
    } else {
        2
    }
}

fn aur_order(a: &aur::Package, b: &aur::Package, term: &str, by: SortBy) -> Ordering {
    let near = closeness(&a.Name, term).cmp(&closeness(&b.Name, term));
    let ranked = match by {
        SortBy::Votes => b.NumVotes.cmp(&a.NumVotes),
        SortBy::Popularity => b.Popularity.partial_cmp(&a.Popularity).unwrap_or(Ordering::Equal),
        SortBy::Modified => b.LastModified.cmp(&a.LastModified),
        SortBy::Name => Ordering::Equal,
    };
    near.then(ranked).then_with(|| a.Name.as_bytes().cmp(b.Name.as_bytes()))
}

/// Mark results that are installed under exactly this name. A package that
/// merely provides the name is a different package.
fn installed_badge(db: &Alpm, name: &str, available: &str) -> Option<Badge> {
    let current = &db.installed_pkg(name)?.version;
    if current == available {
        return Some(Badge { text: "✓ installed".into(), tone: Tone::Success });
    }
    // An older installed version means an update is waiting, not "done".
    if vercmp(current, available) < 0 {
        return Some(Badge { text: format!("installed {current} · update available"), tone: Tone::Warning });
    }
    Some(Badge { text: format!("✓ installed {current}"), tone: Tone::Success })
}

fn repo_entry(db: &Alpm, p: &Pkg, index: Option<usize>) -> Entry {
    let info = p.info();
    Entry {
        index,
        source: alpm::repository(p).into(),
        name: p.name.clone(),
        link: repo_page(p),
        version: p.version.clone(),
        badges: installed_badge(db, &p.name, &p.version).into_iter().collect(),
        description: info.desc.clone(),
        meta: format!("{} · built {}", ui::size(info.isize), ui::date(info.builddate)),
    }
}

/// The archlinux.org page of a package from an official Arch repository;
/// other repositories have no known page.
fn repo_page(p: &Pkg) -> String {
    let repo = alpm::repository(p);
    const OFFICIAL: [&str; 6] = ["core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing"];
    if OFFICIAL.contains(&repo) {
        format!("https://archlinux.org/packages/{repo}/{}/{}/", p.info().arch, p.name)
    } else {
        String::new()
    }
}

fn aur_entry(db: &Alpm, p: &aur::Package, index: Option<usize>) -> Entry {
    let mut badges: Vec<Badge> = installed_badge(db, &p.Name, &p.Version).into_iter().collect();
    if p.OutOfDate.is_some() {
        badges.push(Badge { text: "! out of date".into(), tone: Tone::Warning });
    }
    if p.Maintainer.is_none() {
        badges.push(Badge { text: "! orphaned".into(), tone: Tone::Warning });
    }
    Entry {
        index,
        source: "aur".into(),
        name: p.Name.clone(),
        link: format!("https://aur.archlinux.org/packages/{}", p.Name),
        version: p.Version.clone(),
        badges,
        description: p.Description.clone().unwrap_or_default(),
        meta: format!(
            "{} · popularity {} · {} · updated {}",
            ui::count(p.NumVotes.max(0) as usize, "vote", "votes"),
            ui::fixed(p.Popularity, 2),
            p.Maintainer.as_deref().unwrap_or("no maintainer"),
            ui::date(p.LastModified)
        ),
    }
}

/// Packages whose name or description matches, from the local database or
/// from every repository that allows searching. The first repository to
/// carry a name wins, as it does for installation.
pub fn repository_search(db: &Alpm, term: &str, local: bool) -> Result<Vec<PkgRef>> {
    if local {
        return db.local_db().search(&[term]).map_err(|e| Alpm::failed(&e));
    }
    db.load_sync();
    let mut seen = std::collections::HashSet::new();
    let mut packages = Vec::new();
    for database in &db.h.syncdbs {
        for p in database.search(&[term]).map_err(|e| Alpm::failed(&e))? {
            if seen.insert(p.name.clone()) {
                packages.push(p);
            }
        }
    }
    Ok(packages)
}

pub fn local_search(db: &Alpm, o: &Options) -> Result<()> {
    let packages = repository_search(db, &o.names.join(" "), true)?;
    if o.quiet {
        held(|| packages.iter().for_each(|p| ui::print(&format!("{}\n", ui::safe(&p.name)))));
        return Ok(());
    }
    ui::heading("Installed", &ui::count(packages.len(), "match", "matches"));
    for p in &packages {
        let info = p.info();
        ui::entry(&Entry {
            source: "local".into(),
            name: p.name.clone(),
            version: p.version.clone(),
            description: info.desc.clone(),
            meta: format!(
                "{} · {} · installed {}",
                ui::size(info.isize),
                alpm::reason(p),
                ui::date(info.installdate)
            ),
            ..Entry::default()
        });
    }
    if packages.is_empty() {
        ui::note(NoteKind::Info, "No installed package matches.");
    }
    ui::blank();
    Ok(())
}

/// Parse a reply such as `1 3-5, 7` into unique zero-based indices. Numbers
/// are bounded by `count` and are never used as anything but indices.
pub fn selection(reply: &str, count: usize) -> Result<Vec<usize>> {
    let number = |s: &str| s.parse::<usize>().map_err(|_| errors::Error::from(Kind::InvalidSelection));
    let mut selected = Vec::new();
    for word in reply.split([' ', ',', '\t']).filter(|w| !w.is_empty()) {
        let (first, last) = match word.split_once('-') {
            Some((a, b)) => (number(a)?, number(b)?),
            None => (number(word)?, number(word)?),
        };
        if first == 0 || first > last || last > count {
            return errors::fail(Kind::InvalidSelection);
        }
        for index in first - 1..last {
            if !selected.contains(&index) {
                selected.push(index);
            }
        }
    }
    if selected.is_empty() {
        return errors::fail(Kind::Cancelled);
    }
    Ok(selected)
}

// ── Details ─────────────────────────────────────────────────────────────────

/// The installed package with exactly this name, or a failure naming it.
pub fn installed<'a>(db: &'a Alpm, name: &str) -> Result<&'a PkgRef> {
    db.installed_pkg(name).ok_or_else(|| errors::with(Kind::PackageNotInstalled, name))
}

pub fn info(db: &Alpm, o: &Options) -> Result<()> {
    for name in &o.names {
        if o.command == Command::Localinfo {
            package_details(db, installed(db, name)?, true);
            continue;
        }
        let repo_package = if o.scope == Scope::Aur { None } else { db.repo(name) };
        if let Some(p) = repo_package {
            package_details(db, &p, false);
        } else if o.scope == Scope::Repo {
            return Err(errors::with(Kind::RepositoryPackageNotFound, name.as_str()));
        } else {
            let kind = if o.scope == Scope::Aur { Kind::AurPackageNotFound } else { Kind::PackageNotFound };
            let p = aur::find(name)?.ok_or_else(|| errors::with(kind, name.as_str()))?;
            aur_details(db, &p);
        }
    }
    Ok(())
}

fn package_details(db: &Alpm, p: &Pkg, local: bool) {
    let info = p.info();
    let badges = if local { Vec::new() } else { installed_badge(db, &p.name, &p.version).into_iter().collect() };
    ui::blank();
    ui::entry(&Entry {
        source: if local { "local".into() } else { alpm::repository(p).into() },
        name: p.name.clone(),
        version: p.version.clone(),
        badges,
        description: info.desc.clone(),
        ..Entry::default()
    });
    ui::blank();
    ui::field_optional("URL", &info.url);
    ui::field_list("Licenses", &info.licenses);
    ui::field_list("Groups", &info.groups);
    ui::field_optional("Architecture", &info.arch);
    ui::field_optional("Packager", &info.packager);
    ui::field("Built", &ui::dated(info.builddate));
    if local {
        ui::field("Installed", &ui::dated(info.installdate));
        ui::field("Reason", alpm::reason(p));
    } else {
        ui::field("Download size", &ui::size(info.size));
    }
    ui::field("Installed size", &ui::size(info.isize));
    ui::field("Depends on", &alpm::dependencies(&info.depends).join("  "));
    ui::field_lines("Optional", &optionals(db, &alpm::dependencies(&info.optdepends)));
    ui::field_list("Provides", &alpm::dependencies(&info.provides));
    ui::field_list("Conflicts with", &alpm::dependencies(&info.conflicts));
    ui::field_list("Replaces", &alpm::dependencies(&info.replaces));
    if local {
        ui::field("Required by", &db.required_by(p).join("  "));
        ui::field_list("Optional for", &db.optional_for(p));
    }
    ui::blank();
}

/// Optional dependencies, `name: why`, marked when one is installed.
fn optionals(db: &Alpm, deps: &[String]) -> Vec<String> {
    deps.iter()
        .map(|dep| {
            let name = &dep[..dep.find(": ").unwrap_or(dep.len())];
            if db.local(name).is_some() {
                format!("{dep} · installed")
            } else {
                dep.clone()
            }
        })
        .collect()
}

fn aur_details(db: &Alpm, p: &aur::Package) {
    let mut entry = aur_entry(db, p, None);
    entry.meta.clear();
    ui::blank();
    ui::entry(&entry);
    ui::blank();
    ui::field_optional("URL", p.URL.as_deref().unwrap_or(""));
    ui::field("AUR page", &format!("https://aur.archlinux.org/packages/{}", p.Name));
    if p.PackageBase != p.Name {
        ui::field("Package base", &p.PackageBase);
    }
    ui::field_list("Licenses", &p.License);
    ui::field("Maintainer", p.Maintainer.as_deref().unwrap_or("none (orphaned)"));
    ui::field("Votes", &format!("{} · popularity {}", p.NumVotes, ui::fixed(p.Popularity, 2)));
    ui::field("First submitted", &ui::dated(p.FirstSubmitted));
    ui::field("Last modified", &ui::dated(p.LastModified));
    if let Some(flagged) = p.OutOfDate {
        ui::field("Out of date since", &ui::dated(flagged));
    }
    ui::field("Depends on", &p.Depends.join("  "));
    ui::field_list("Build needs", &p.MakeDepends);
    ui::field_list("Check needs", &p.CheckDepends);
    ui::field_lines("Optional", &optionals(db, &p.OptDepends));
    ui::field_list("Provides", &p.Provides);
    ui::field_list("Conflicts with", &p.Conflicts);
    ui::blank();
}

// ── Installed packages ──────────────────────────────────────────────────────

/// `list` and `foreign`: every installed package, or only the named ones.
pub fn list(db: &Alpm, o: &Options) -> Result<()> {
    for name in &o.names {
        installed(db, name)?;
    }
    let foreign = o.command == Command::Foreign;
    let shown: Vec<&PkgRef> = db
        .installed()
        .iter()
        .filter(|p| o.names.is_empty() || util::contains(&o.names, &p.name))
        .collect();
    let shown: Vec<&PkgRef> = if foreign {
        db.load_sync();
        shown.into_iter().filter(|p| db.foreign(p)).collect()
    } else {
        shown
    };
    if o.quiet {
        held(|| shown.iter().for_each(|p| ui::print(&format!("{}\n", ui::safe(&p.name)))));
        return Ok(());
    }
    db.local_db().preload();
    let mut table = Table::new(&[
        Column::new("Package").tone(Tone::Bold),
        Column::new("Version").tone(Tone::Success),
        Column::new("Reason"),
        Column::new("Installed").tone(Tone::Muted),
    ]);
    let mut explicit = 0;
    for p in &shown {
        explicit += p.explicit() as usize;
        table.add(vec![
            Cell::new(p.name.as_str()),
            Cell::new(p.version.as_str()),
            Cell::toned(alpm::reason(p), if p.explicit() { Tone::Plain } else { Tone::Muted }),
            Cell::new(ui::date(p.info().installdate)),
        ]);
    }
    let total = shown.len();
    held(|| {
        let title = if foreign { "Foreign packages" } else { "Installed packages" };
        let detail = format!(
            "{} · {explicit} explicit · {} as dependencies",
            ui::count(total, "package", "packages"),
            total - explicit
        );
        ui::heading(title, &detail);
        table.render();
        if total == 0 {
            ui::note(
                NoteKind::Info,
                if foreign { "Every installed package comes from a configured repository." } else { "No packages installed." },
            );
        }
        ui::blank();
    });
    Ok(())
}

/// Whether a path recorded in a package can be joined to the root directory
/// without leaving it.
pub fn valid_file(path: &str) -> bool {
    !path.is_empty() && !path.starts_with('/') && path.split('/').all(|part| part != ".." && part != ".")
}

pub fn file_path(db: &Alpm, path: &str) -> Result<String> {
    if !valid_file(path) {
        return Err(errors::with(Kind::InvalidPackageFile, path));
    }
    Ok(crate::util::join(&db.cfg.root, path))
}

/// A package's recorded files, failing when the list cannot be read.
pub fn file_entries(p: &Pkg) -> Result<&[crate::libalpm::package::File]> {
    if !p.files_readable() {
        return errors::fail(Kind::InvalidPackageFile);
    }
    Ok(&p.files().files)
}

pub fn files<S: AsRef<str>>(db: &Alpm, names: &[S], quiet: bool) -> Result<()> {
    held(|| {
        for name in names {
            let name = name.as_ref();
            for file in file_entries(installed(db, name)?)? {
                let path = ui::safe(&file_path(db, &file.name)?);
                if quiet {
                    ui::print(&format!("{path}\n"));
                } else {
                    ui::print(&format!("{}{}{} {path}\n", ui::style(Tone::Bold), ui::safe(name), ui::reset()));
                }
            }
        }
        Ok(())
    })
}

/// The path of an installed file relative to the root directory, as package
/// file lists record it. Directory aliases such as /bin are resolved; the
/// final component is kept, so a symlink is looked up as itself.
fn recorded_path(db: &Alpm, input: &str) -> Result<Option<String>> {
    let cwd = std::env::current_dir()?;
    let resolved = crate::util::resolve(cwd.to_str().unwrap_or("/"), input);
    let (dir, base) = match resolved.rfind('/') {
        Some(0) if resolved.len() == 1 => ("/", ""),
        Some(0) => ("/", &resolved[1..]),
        Some(at) => (&resolved[..at], &resolved[at + 1..]),
        None => ("/", resolved.as_str()),
    };
    let parent = match std::fs::canonicalize(dir) {
        Ok(parent) => parent,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR) => {
            return Ok(None)
        }
        Err(e) => return Err(e.into()),
    };
    let path = crate::util::join(parent.to_str().unwrap_or(""), base);
    let root = db.cfg.root.trim_end_matches('/');
    match path.strip_prefix(root) {
        Some(rest) if rest.len() > 0 && rest.starts_with('/') => Ok(Some(rest[1..].to_string())),
        _ => Ok(None),
    }
}

/// The first installed package, in database order, whose file list records
/// `path`. A package whose list cannot be read before that is a failure.
fn owner<'a>(db: &'a Alpm, path: &str) -> Result<Option<&'a PkgRef>> {
    let pkgs = db.installed();
    let Some(at) = find_first(pkgs, |p| p.records_file(path) != Some(false)) else { return Ok(None) };
    match pkgs[at].records_file(path) {
        Some(_) => Ok(Some(&pkgs[at])),
        None => errors::fail(Kind::InvalidPackageFile),
    }
}

pub fn owners<S: AsRef<str>>(db: &Alpm, paths: &[S], quiet: bool) -> Result<()> {
    let mut unowned = 0;
    let mut absent = 0;
    let mut table = Table::new(&[
        Column::new("File"),
        Column::new("Package").tone(Tone::Bold),
        Column::new("Version").tone(Tone::Success),
        Column::new("Status"),
    ]);
    for input in paths {
        let input = input.as_ref();
        let found = match recorded_path(db, input)? {
            Some(recorded) => owner(db, &recorded)?,
            None => None,
        };
        if let Some(p) = found {
            if quiet {
                ui::print(&format!("{}\n", ui::safe(&p.name)));
            }
            table.add_text(&[input, &p.name, &p.version, ""]);
        } else {
            unowned += 1;
            // A path that does not exist is a different problem from a stray file.
            let exists = std::fs::symlink_metadata(input).is_ok();
            absent += !exists as usize;
            let status = if exists { "not owned by any package" } else { "does not exist" };
            table.add(vec![
                Cell::new(input),
                Cell::toned("—", Tone::Muted),
                Cell::toned("—", Tone::Muted),
                Cell::toned(status, if exists { Tone::Danger } else { Tone::Warning }),
            ]);
        }
    }
    if !quiet {
        ui::heading("File owners", &ui::count(paths.len(), "file", "files"));
        table.render();
    }
    if unowned > 0 {
        if !quiet {
            ui::blank();
            if unowned > absent {
                let count = ui::count(unowned - absent, "file is", "files are");
                ui::note(NoteKind::Warning, &format!("{count} not owned by any installed package."));
            }
            if absent > 0 {
                ui::note(NoteKind::Warning, &format!("{} not exist.", ui::count(absent, "path does", "paths do")));
            }
            ui::blank();
        }
        return errors::fail(Kind::FileNotOwned);
    }
    if !quiet {
        ui::blank();
    }
    Ok(())
}

/// What `check` found for one package.
struct Checked {
    files: usize,
    absent: Vec<String>,
    hidden: usize,
}

/// Look for each recorded file of one package under `root`. File lists are
/// sorted, so each directory is opened once and its entries are examined
/// relative to it, as `lstat` of the full path would see them.
fn check_package(p: &Pkg, root: &str) -> Result<Checked> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let entries = file_entries(p)?;
    let mut checked = Checked { files: entries.len(), absent: Vec::new(), hidden: 0 };
    // The directory last opened, and its descriptor or the errno opening it gave.
    let mut current: Option<(&str, std::result::Result<OwnedFd, i32>)> = None;
    let errno = || std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO);
    for file in entries {
        if !valid_file(&file.name) {
            return Err(errors::with(Kind::InvalidPackageFile, format!("{}: {}", p.name, file.name)));
        }
        let recorded = file.name.trim_end_matches('/');
        let (dir, base) = recorded.rsplit_once('/').unwrap_or(("", recorded));
        if current.as_ref().is_none_or(|(open, _)| *open != dir) {
            let path = util::cstr(&format!("{root}/{dir}"));
            let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
            let fd = unsafe { libc::open(path.as_ptr(), flags) };
            current = Some((dir, if fd < 0 { Err(errno()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }));
        }
        let failure = match &current.as_ref().unwrap().1 {
            Ok(fd) => {
                let name = util::cstr(base);
                let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
                let found = unsafe {
                    libc::fstatat(fd.as_raw_fd(), name.as_ptr(), st.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
                };
                if found == 0 {
                    continue;
                }
                errno()
            }
            Err(e) => *e,
        };
        let shown = format!("{root}/{recorded}");
        match failure {
            libc::ENOENT | libc::ENOTDIR => checked.absent.push(shown),
            libc::EACCES => checked.hidden += 1,
            _ => return Err(errors::with(Kind::FileInspectionFailed, shown)),
        }
    }
    Ok(checked)
}

/// Check that installed files exist. Contents are not verified. Files in
/// directories this user cannot search are counted, not treated as missing.
pub fn check<S: AsRef<str>>(db: &Alpm, names: &[S]) -> Result<()> {
    for name in names {
        installed(db, name.as_ref())?;
    }
    let root = db.cfg.root.trim_end_matches('/');
    let heading = if names.is_empty() { "all packages".into() } else { ui::count(names.len(), "package", "packages") };
    ui::heading("Installed files", &heading);
    let selected: Vec<&PkgRef> = db
        .installed()
        .iter()
        .filter(|p| names.is_empty() || util::contains(names, &p.name))
        .collect();
    // Packages are checked in parallel and reported in database order.
    let results = parallel_map(&selected, |p| check_package(p, root));
    let (mut packages, mut total, mut missing, mut unreadable) = (0, 0, 0, 0);
    for (p, result) in selected.iter().zip(results) {
        let checked = result?;
        packages += 1;
        total += checked.files;
        missing += checked.absent.len();
        unreadable += checked.hidden;
        if !checked.absent.is_empty() {
            let files = ui::count(checked.files, "file", "files");
            ui::note(NoteKind::Failure, &format!("{} · {} of {files} missing", p.name, checked.absent.len()));
            for path in &checked.absent {
                ui::line(path, 6, Tone::Muted);
            }
        } else if !names.is_empty() {
            let present = ui::count(checked.files - checked.hidden, "file", "files");
            if checked.hidden > 0 {
                let message = format!("{} · {present} present, {} not visible to your user", p.name, checked.hidden);
                ui::note(NoteKind::Warning, &message);
            } else {
                ui::note(NoteKind::Success, &format!("{} · {present} present", p.name));
            }
        }
    }
    if names.len() != 1 {
        let summary = format!(
            "Checked {} · {} · {missing} missing",
            ui::count(packages, "package", "packages"),
            ui::count(total, "file", "files")
        );
        ui::note(if missing == 0 { NoteKind::Success } else { NoteKind::Failure }, &summary);
        if unreadable > 0 {
            let count = ui::count(unreadable, "file", "files");
            ui::note(NoteKind::Info, &format!("{count} in directories your user cannot read could not be checked."));
        }
    }
    ui::blank();
    if missing != 0 {
        return errors::fail(Kind::MissingInstalledFiles);
    }
    Ok(())
}

pub fn stats(db: &Alpm) -> Result<()> {
    // Sync databases load while the local one is read.
    std::thread::scope(|s| {
        s.spawn(|| db.load_sync());
        db.local_db().preload();
    });
    let installed = db.installed();
    let total = installed.len();
    let explicit = installed.iter().filter(|p| p.explicit()).count();
    let foreign = installed.iter().filter(|p| db.foreign(p)).count();
    let size: i64 = installed.iter().map(|p| p.info().isize).sum();
    let orphans = db.orphans();
    ui::heading("Installed packages", "");
    let counts = [
        ("Packages", total),
        ("Explicit", explicit),
        ("Dependencies", total - explicit),
        ("Foreign", foreign),
        ("Orphans", orphans.len()),
    ];
    for (label, value) in counts {
        ui::field(label, &value.to_string());
    }
    ui::field("Installed size", &ui::size(size));

    let mut packages: Vec<&PkgRef> = installed.iter().collect();
    packages.sort_by(|a, b| b.info().isize.cmp(&a.info().isize));
    let shown = &packages[..packages.len().min(10)];
    if shown.is_empty() {
        ui::blank();
        return Ok(());
    }
    ui::heading("Largest packages", "");
    let largest = shown[0].info().isize;
    let mut table = Table::new(&[
        Column::new("").tone(Tone::Muted).right(),
        Column::new("").tone(Tone::Bold),
        Column::new("").right(),
        Column::new("").tone(Tone::Accent),
    ]);
    for (rank, p) in shown.iter().enumerate() {
        let package_size = p.info().isize;
        table.add_text(&[
            (rank + 1).to_string(),
            p.name.clone(),
            ui::size(package_size),
            ui::bar(package_size, largest, 24),
        ]);
    }
    table.render();
    ui::blank();
    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, installed_fixture, TempDir};

    #[test]
    fn interactive_selections_reject_invalid_ranges_and_deduplicate() {
        assert_eq!(vec![0, 1, 2, 4], selection("1-3, 2 5", 5).unwrap());
        for input in ["0", "6", "3-1", "1-9999999999", "-1", "all", "1;id"] {
            assert_eq!(Kind::InvalidSelection, selection(input, 5).unwrap_err().kind, "{input}");
        }
        assert_eq!(Kind::Cancelled, selection("", 5).unwrap_err().kind);
    }

    #[test]
    fn installed_file_paths_cannot_escape_the_configured_root() {
        for path in ["", "/etc/passwd", "../etc/passwd", "usr/../../etc", "usr/./bin"] {
            assert!(!valid_file(path), "{path}");
        }
        assert!(valid_file("usr/bin/tool"));
        assert!(valid_file("usr/share/"));
    }

    #[test]
    fn aur_sort_is_deterministic_with_name_tie_breaks() {
        let pkg = |name: &str, votes| aur::Package { Name: name.into(), NumVotes: votes, ..Default::default() };
        let mut packages = vec![pkg("z", 2), pkg("a", 2), pkg("b", 9)];
        packages.sort_by(|a, b| aur_order(a, b, "", SortBy::Votes));
        assert_eq!(["b", "a", "z"], packages.iter().map(|p| p.Name.as_str()).collect::<Vec<_>>()[..]);
        packages.sort_by(|a, b| aur_order(a, b, "", SortBy::Name));
        assert_eq!(["a", "b", "z"], packages.iter().map(|p| p.Name.as_str()).collect::<Vec<_>>()[..]);
        // The exact name outranks votes, then names that start with the term.
        packages.sort_by(|a, b| aur_order(a, b, "z", SortBy::Votes));
        assert_eq!("z", packages[0].Name);
        assert_eq!(1, closeness("ripgrep-all", "ripgrep"));
        assert_eq!(2, closeness("repgrep", "ripgrep"));
    }

    #[test]
    fn installed_search_badges_require_an_exact_identity_not_a_virtual_provider() {
        let tmp = TempDir::new();
        installed_fixture(tmp.path(), "replacement", "");
        let desc = tmp.path().join("db/local/replacement-1.0-1/desc");
        let mut text = std::fs::read_to_string(&desc).unwrap();
        text.push_str("%PROVIDES%\noriginal\n\n");
        std::fs::write(&desc, text).unwrap();
        let db = testing::alpm(tmp.path());
        assert!(db.local("original").is_some());
        assert!(installed_badge(&db, "original", "1.0-1").is_none());
        assert_eq!("✓ installed", installed_badge(&db, "replacement", "1.0-1").unwrap().text);
        let update = installed_badge(&db, "replacement", "2.0-1").unwrap();
        assert_eq!("installed 1.0-1 · update available", update.text);
        assert_eq!(Tone::Warning, update.tone);
        assert_eq!("✓ installed 1.0-1", installed_badge(&db, "replacement", "0.9-1").unwrap().text);
    }

    #[test]
    fn file_queries_tolerate_empty_package_lists_and_resolve_directory_aliases() {
        let _lock = ui::test_lock();
        let tmp = TempDir::new();
        let root = tmp.str();
        installed_fixture(tmp.path(), "empty", "");
        installed_fixture(tmp.path(), "owner", "usr/bin/tool");
        std::fs::create_dir_all(tmp.path().join("usr/bin")).unwrap();
        std::fs::write(tmp.path().join("usr/bin/tool"), "tool").unwrap();
        std::os::unix::fs::symlink("usr/bin", tmp.path().join("bin")).unwrap();
        let db = testing::alpm(tmp.path());
        assert!(file_entries(installed(&db, "empty").unwrap()).unwrap().is_empty());
        let alias = format!("{root}/bin/tool");
        assert_eq!(Some("usr/bin/tool".into()), recorded_path(&db, &alias).unwrap());
        assert_eq!(None, recorded_path(&db, "/outside-the-root").unwrap());
        assert_eq!(None, recorded_path(&db, &format!("{root}/no-such-directory/file")).unwrap());
        ui::begin_capture();
        owners(&db, &[alias], true).unwrap();
        check(&db, &["empty", "owner"]).unwrap();
        std::fs::remove_file(tmp.path().join("usr/bin/tool")).unwrap();
        assert_eq!(Kind::MissingInstalledFiles, check(&db, &["owner"]).unwrap_err().kind);
        let missing = format!("{root}/usr/bin/missing");
        assert_eq!(Kind::FileNotOwned, owners(&db, &[missing], true).unwrap_err().kind);
        let nowhere = format!("{root}/no-such-directory/file");
        assert_eq!(Kind::FileNotOwned, owners(&db, &[nowhere], true).unwrap_err().kind);
        assert_eq!(Kind::PackageNotInstalled, check(&db, &["absent"]).unwrap_err().kind);
        ui::end_capture();
    }

    #[test]
    fn installed_listings_are_a_table_for_people_and_bare_names_for_scripts() {
        let _lock = ui::test_lock();
        let tmp = TempDir::new();
        installed_fixture(tmp.path(), "alpha", "");
        installed_fixture(tmp.path(), "beta", "");
        let db = testing::alpm(tmp.path());
        ui::set_caps(Some(ui::Caps { width: Some(80), ..ui::Caps::default() }));
        let options = |command, names: &[&str], quiet| Options {
            names: names.iter().map(|n| n.to_string()).collect(),
            quiet,
            ..Options::new(command)
        };

        ui::begin_capture();
        list(&db, &options(Command::List, &[], false)).unwrap();
        assert_eq!(
            "\n▌ Installed packages  2 packages · 0 explicit · 2 as dependencies\n  Package  Version  Reason      Installed\n  alpha    1.0-1    dependency  unknown\n  beta     1.0-1    dependency  unknown\n\n",
            ui::end_capture()
        );

        ui::begin_capture();
        list(&db, &options(Command::List, &["beta"], true)).unwrap();
        assert_eq!("beta\n", ui::end_capture());

        // No repository is configured for the fixture, so everything is foreign.
        ui::begin_capture();
        list(&db, &options(Command::Foreign, &[], true)).unwrap();
        assert_eq!("alpha\nbeta\n", ui::end_capture());
        assert_eq!(Kind::PackageNotInstalled, list(&db, &options(Command::List, &["gamma"], false)).unwrap_err().kind);

        ui::begin_capture();
        info(&db, &options(Command::Localinfo, &["alpha"], false)).unwrap();
        let details = ui::end_capture();
        ui::set_caps(None);
        for expected in [
            "local/alpha 1.0-1",
            "Installed fixture",
            "Reason              dependency",
            "Depends on          none",
            "Required by         none",
        ] {
            assert!(details.contains(expected), "{expected} in {details}");
        }
        // Fields with nothing to say are left out.
        for absent in ["Provides", "Conflicts with", "Groups", "Optional"] {
            assert!(!details.contains(absent), "{absent} in {details}");
        }
    }
}
