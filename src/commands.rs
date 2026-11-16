//! Command dispatch. Each command is a short flow over the modules that do
//! the work.
use crate::alpm::{self, Alpm, PkgRef};
use crate::builder::Builder;
use crate::cli::{Command, Options, Reason, Scope, ELEVATING};
use crate::errors::{self, Kind, Result};
use crate::libalpm::{self, archive, vercmp, LIBALPM_VERSION};
use crate::transaction::{self as tx, Operation, Request, Target};
use crate::ui::{self, Cell, Column, NoteKind, Table, Tone};
use crate::{aur, help, news, query, util};

pub fn run(o: Options) -> Result<()> {
    ui::set_color_mode(o.color);
    ui::set_unattended(o.noconfirm);
    if o.command == Command::Help {
        help::show(o.topic);
        return Ok(());
    }
    if o.dry {
        ui::note(NoteKind::Warning, "Dry run: nothing will be built, elevated or changed.");
    }
    if o.command == Command::Version {
        let rust = env!("ZAP_RUSTC_VERSION");
        ui::print(&format!("zap {} · libalpm {LIBALPM_VERSION} · Rust {rust}\n", util::VERSION));
        return Ok(());
    }
    if unsafe { libc::geteuid() } == 0 {
        return errors::fail(Kind::BuildsMustNotRunAsRoot);
    }
    tx::set_elevator(o.elevate);
    // A misconfigured elevation tool is reported before any work is shown.
    // Dry runs refuse an unknown choice, but need no tool on this machine.
    if ELEVATING.contains(o.command) {
        if o.dry {
            tx::chosen_elevator()?;
        } else {
            tx::trusted_elevator()?;
        }
    }
    if o.command == Command::News {
        return news::show();
    }
    check_names(&o)?;

    let mut db = Alpm::init()?;
    let result = match o.command {
        Command::Search => query::search(&db, &o).map(drop),
        Command::Select => select(&mut db, &o),
        Command::Info | Command::Localinfo => query::info(&db, &o),
        Command::List | Command::Foreign => query::list(&db, &o),
        Command::LocalSearch => query::local_search(&db, &o),
        Command::Files => query::files(&db, &o.names, o.quiet),
        Command::Owns => query::owners(&db, &o.names, o.quiet),
        Command::Check => query::check(&db, &o.names),
        Command::Stats => query::stats(&db),
        Command::Updates => updates(&db, &o).map(drop),
        Command::Upgrade => upgrade(&mut db, &o),
        Command::Install | Command::Build => install(&mut db, &o),
        Command::Get | Command::Pkgbuild => fetch(&mut db, &o),
        Command::BuildLocal | Command::InstallLocal => install_local(&mut db, &o),
        Command::InstallFiles => install_files(&o),
        Command::Remove => remove(&db, &o),
        Command::Reason => reason(&db, &o),
        Command::Orphans | Command::Autoremove => orphans(&db, &o),
        Command::Clean => clean(&mut db, &o),
        Command::Help | Command::Version | Command::News => unreachable!(),
    };
    // The process exits next; freeing every cached package first only costs time.
    std::mem::forget(db);
    result
}

/// Commands whose targets are package names accept only valid, distinct ones.
fn check_names(o: &Options) -> Result<()> {
    use Command::*;
    if !matches!(
        o.command,
        Install | Build | Get | Pkgbuild | Info | Localinfo | Files | Reason | Remove | List | Foreign | Check | Clean
    ) {
        return Ok(());
    }
    for (i, name) in o.names.iter().enumerate() {
        if !util::valid_name(name) {
            return Err(errors::with(Kind::InvalidPackageName, name.as_str()));
        }
        if o.names[..i].contains(name) {
            return Err(errors::with(Kind::DuplicateTarget, name.as_str()));
        }
    }
    Ok(())
}

/// The root transaction worker, started by `escalate` as `zap __transaction <json>`.
pub fn worker(request: &str) -> Result<()> {
    tx::worker(request)
}

fn configure(builds: &mut Builder, o: &Options) {
    builds.scope = o.scope;
    builds.needed = o.needed;
    builds.reason = o.reason;
    builds.sandbox = o.sandbox.unwrap_or(true);
    builds.clean_after = o.clean_after;
    builds.rebuild_tree = o.rebuild_tree;
}

// ── Install and build ───────────────────────────────────────────────────────

fn install(db: &mut Alpm, o: &Options) -> Result<()> {
    let mut builds = Builder::new(db, &o.names)?;
    configure(&mut builds, o);
    builds.plan()?;
    // A dry run must refuse what the real build would refuse.
    if o.command == Command::Build {
        builds.require_installed_dependencies()?;
    }
    if !o.dry {
        builds.execute(o.command == Command::Install)?;
    }
    Ok(())
}

fn install_local(db: &mut Alpm, o: &Options) -> Result<()> {
    let mut builds = Builder::new(db, &[])?;
    configure(&mut builds, o);
    builds.load_local(&o.names)?;
    builds.plan()?;
    if o.command == Command::BuildLocal {
        builds.require_installed_dependencies()?;
    }
    if !o.dry {
        builds.execute(o.command == Command::InstallLocal)?;
    }
    Ok(())
}

fn fetch(db: &mut Alpm, o: &Options) -> Result<()> {
    let builds = Builder::new(db, &o.names)?;
    if o.command == Command::Get {
        builds.get_sources()
    } else {
        builds.print_pkgbuilds()
    }
}

fn select(db: &mut Alpm, o: &Options) -> Result<()> {
    let matches = query::search(db, o)?;
    if matches.is_empty() {
        return Err(errors::with(Kind::PackageNotFound, o.names.join(" ")));
    }
    let selected = loop {
        let reply = ui::answer("Install which? Numbers or ranges such as 1 3-5; empty cancels:")?;
        match query::selection(&reply, matches.len()) {
            Ok(selected) => break selected,
            Err(e) if e.kind == Kind::InvalidSelection => {
                ui::note(NoteKind::Warning, &format!("Use numbers from 1 to {}, or ranges such as 1-3.", matches.len()));
            }
            Err(e) => return Err(e),
        }
    };
    let mut names = Vec::new();
    let mut aur_names = Vec::new();
    for &index in &selected {
        let chosen = &matches[index];
        // One name cannot be installed from both a repository and the AUR.
        if selected.iter().any(|&other| other != index && matches[other].name == chosen.name) {
            return Err(errors::with(Kind::ConflictingSelection, &chosen.name));
        }
        names.push(chosen.name.clone());
        // Keep the chosen source when a repository and the AUR share a name.
        if chosen.source == Scope::Aur {
            aur_names.push(chosen.name.clone());
        }
    }
    let mut builds = Builder::new(db, &names)?;
    configure(&mut builds, o);
    builds.aur_targets = aur_names;
    builds.plan()?;
    if !o.dry {
        builds.execute(true)?;
    }
    Ok(())
}

/// Hash and identify local archives by their own metadata. Nothing in them
/// is executed, and a final symlink is refused rather than followed.
pub fn archive_targets<S: AsRef<str>>(paths: &[S], wanted_reason: Option<Reason>) -> Result<Vec<Target>> {
    let cwd = std::env::current_dir()?;
    let mut targets: Vec<Target> = Vec::new();
    for input in paths {
        let path = util::resolve(&cwd.to_string_lossy(), input.as_ref());
        let digest = tx::hash_file(&path)?;
        let p = archive::load(&path, true).map_err(|_| errors::with(Kind::InvalidBuildArchive, &path))?;
        // The metadata just read must describe the bytes that were hashed.
        if digest != tx::hash_file(&path)? {
            return Err(errors::with(Kind::ArchiveChanged, &path));
        }
        if targets.iter().any(|previous| previous.name == p.name) {
            return Err(errors::with(Kind::DuplicateTarget, &p.name));
        }
        targets.push(Target { archive: Some(path), sha256: Some(digest), reason: wanted_reason, ..Target::named(&p.name) });
    }
    tx::validate(&Request { targets: targets.clone(), ..Request::new(Operation::Install) })?;
    Ok(targets)
}

fn install_files(o: &Options) -> Result<()> {
    let targets = archive_targets(&o.names, o.reason)?;
    ui::heading("Package archives", &ui::count(targets.len(), "archive", "archives"));
    for target in &targets {
        ui::text(&target.name, 2, Tone::Bold);
        ui::field_raw("  Archive", target.archive.as_deref().unwrap_or(""));
        ui::field_raw("  SHA-256", target.sha256.as_deref().unwrap_or(""));
    }
    ui::blank();
    if !o.dry {
        tx::escalate(&Request { targets, needed: o.needed, ..Request::new(Operation::Install) })?;
    }
    Ok(())
}

// ── Updates ─────────────────────────────────────────────────────────────────

const VCS_SUFFIXES: [&str; 7] = ["-git", "-svn", "-hg", "-bzr", "-cvs", "-darcs", "-fossil"];

fn is_vcs(name: &str) -> bool {
    VCS_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
}

/// Show pending updates and return the AUR packages to rebuild.
fn updates(db: &Alpm, o: &Options) -> Result<Vec<String>> {
    let mut table = Table::new(&[
        Column::new("Package").tone(Tone::Bold),
        Column::new("Current"),
        Column::new("New").tone(Tone::Success).joiner(" → "),
        Column::new("Source"),
        Column::new("Updated").tone(Tone::Muted).label("updated"),
    ]);
    let mut names: Vec<String> = Vec::new();
    let mut foreign: Vec<&str> = Vec::new();
    db.load_sync();
    for p in db.installed() {
        if db.h.should_ignore(p) {
            continue;
        }
        if db.foreign(p) {
            if o.scope != Scope::Repo {
                foreign.push(&p.name);
            }
        } else if o.scope != Scope::Aur {
            if let Some(next) = db.h.sync_new_version(p) {
                names.push(p.name.clone());
                table.add(vec![
                    Cell::new(&p.name),
                    Cell::new(&p.version),
                    Cell::new(&next.version),
                    Cell::toned(alpm::repository(next), Tone::Accent),
                    Cell::new(ui::date(next.info().builddate)),
                ]);
            }
        }
    }

    let mut targets = Vec::new();
    let mut absent = Vec::new();
    let remote = aur::info(&foreign)?;
    for name in foreign {
        let Some(p) = remote.iter().find(|candidate| candidate.Name == name) else {
            absent.push(name);
            continue;
        };
        let Some(old) = db.installed_pkg(name) else { continue };
        let rebuild = o.devel && is_vcs(name);
        let newer = vercmp(&p.Version, &old.version) > 0;
        if !newer && !rebuild {
            continue;
        }
        names.push(name.to_string());
        targets.push(name.to_string());
        table.add(vec![
            Cell::new(name),
            Cell::new(&old.version),
            Cell::new(&p.Version),
            if newer { Cell::toned("aur", Tone::Special) } else { Cell::toned("aur · rebuild", Tone::Warning) },
            Cell::new(ui::date(p.LastModified)),
        ]);
    }

    if o.quiet {
        ui::hold();
        for name in &names {
            ui::print(&format!("{}\n", ui::safe(name)));
        }
        ui::release();
        return Ok(targets);
    }
    ui::heading("Updates", &ui::count(names.len(), "package", "packages"));
    table.render();
    if names.is_empty() {
        ui::note(NoteKind::Success, if o.scope == Scope::Aur { "AUR packages are up to date." } else { "Everything is up to date." });
    } else if o.command == Command::Updates {
        ui::blank();
        ui::note(NoteKind::Info, "zap upgrade applies these updates.");
    }
    if o.command == Command::Updates && o.scope != Scope::Aur {
        let synced = db.synced_at();
        if synced == 0 {
            ui::note(NoteKind::Info, "Repository databases are as fresh as the last zap upgrade.");
        } else {
            ui::note(NoteKind::Info, &format!("The oldest repository database dates from {}; zap upgrade refreshes them.", ui::age(synced, ui::now())));
        }
    }
    if !absent.is_empty() {
        ui::note(NoteKind::Info, &format!("Not in the AUR, left as they are: {}", absent.join(", ")));
    }
    ui::blank();
    Ok(targets)
}

fn upgrade(db: &mut Alpm, o: &Options) -> Result<()> {
    if o.scope != Scope::Aur {
        if o.dry {
            ui::note(NoteKind::Info, "A dry run compares against the repository databases as last refreshed.");
        } else {
            tx::escalate(&Request::new(Operation::Upgrade))?;
            db.reload()?;
        }
    }
    let targets = updates(db, o)?;
    if targets.is_empty() {
        return Ok(());
    }
    let mut builds = Builder::new(db, &targets)?;
    configure(&mut builds, o);
    builds.scope = Scope::Aur;
    builds.preserve_reasons = true;
    builds.plan()?;
    if !o.dry {
        builds.execute(true)?;
    }
    Ok(())
}

// ── Remove and maintain ─────────────────────────────────────────────────────

/// List packages with their sizes and return the total installed size.
fn package_table(packages: &[PkgRef]) -> i64 {
    let mut table = Table::new(&[
        Column::new("Package").tone(Tone::Bold),
        Column::new("Version"),
        Column::new("Installed").tone(Tone::Muted).label("installed"),
        Column::new("Size").right(),
    ]);
    let mut total = 0;
    for p in packages {
        let info = p.info();
        total += info.isize;
        table.add_text(&[p.name.clone(), p.version.clone(), ui::date(info.installdate), ui::size(info.isize)]);
    }
    table.render();
    total
}

fn remove(db: &Alpm, o: &Options) -> Result<()> {
    let packages: Vec<PkgRef> = o.names.iter().map(|name| query::installed(db, name).cloned()).collect::<Result<_>>()?;
    let targets: Vec<Target> = o.names.iter().map(|name| Target::named(name)).collect();
    // Refuse before asking for root what libalpm or HoldPkg would refuse.
    for p in &packages {
        if let Some(pattern) = tx::held(&db.cfg.hold, &p.name) {
            return Err(errors::with(Kind::ProtectedPackage, format!("{} (HoldPkg {pattern})", p.name)));
        }
    }
    let mut table = Table::new(&[
        Column::new("Package").tone(Tone::Bold),
        Column::new("Version"),
        Column::new("Size").right(),
        Column::new("Used by").tone(Tone::Muted).label("used by"),
    ]);
    // "Used by" and the breakage check read every installed package's
    // dependencies; parse them all in parallel up front.
    db.h.local.preload();
    let mut total = 0;
    for p in &packages {
        // Informational: another installed provider may still satisfy these.
        let others: Vec<String> = db.required_by(p).into_iter().filter(|dependant| !o.names.contains(dependant)).collect();
        total += p.info().isize;
        table.add_text(&[p.name.clone(), p.version.clone(), ui::size(p.info().isize), used_by(&others)]);
    }
    ui::heading(if o.dry { "Would remove" } else { "Remove" }, &ui::count(packages.len(), "package", "packages"));
    table.render();
    ui::blank();
    // What would actually break, as libalpm decides it: a dependency that no
    // remaining installed package satisfies.
    let broken = broken_by(db, &packages);
    if !broken.is_empty() {
        let named = &broken[..broken.len().min(5)];
        let more = if broken.len() > named.len() { format!("; and {} more", broken.len() - named.len()) } else { String::new() };
        return Err(errors::with(Kind::RequiredByOthers, format!("{}{more}", named.join("; "))));
    }
    ui::note(
        NoteKind::Info,
        &format!(
            "Frees {}{}{}.",
            ui::size(total),
            if o.recursive { " plus any dependencies nothing else needs" } else { "" },
            if o.nosave { "; configuration files are deleted, not kept as .pacsave" } else { "" },
        ),
    );
    if o.dry {
        ui::blank();
        return Ok(());
    }
    tx::escalate(&Request { targets, recursive: o.recursive, nosave: o.nosave, ..Request::new(Operation::Remove) })
}

/// The "Used by" cell: the first few dependants, then how many more.
fn used_by(dependants: &[String]) -> String {
    if dependants.is_empty() {
        return "nothing".into();
    }
    let named = dependants[..dependants.len().min(8)].join(", ");
    if dependants.len() <= 8 {
        return named;
    }
    format!("{named} and {} more", dependants.len() - 8)
}

/// Installed packages whose dependencies would no longer be satisfied once
/// `removals` are gone, as `dependant needs dependency`.
fn broken_by(db: &Alpm, removals: &[PkgRef]) -> Vec<String> {
    libalpm::deps::checkdeps(db.installed(), removals, &[], true)
        .into_iter()
        .map(|missing| format!("{} needs {}", missing.target, missing.depend))
        .collect()
}

fn reason(db: &Alpm, o: &Options) -> Result<()> {
    let wanted = o.reason.ok_or(Kind::InstallationReasonRequired)?;
    let wanted_name = match wanted {
        Reason::Explicit => "explicit",
        Reason::Dependency => "dependency",
    };
    let mut targets = Vec::new();
    let mut table = Table::new(&[Column::new("Package").tone(Tone::Bold), Column::new("Current"), Column::new("New").tone(Tone::Accent).joiner(" → ")]);
    for name in &o.names {
        let p = query::installed(db, name)?;
        targets.push(Target { reason: Some(wanted), ..Target::named(name) });
        table.add_text(&[name.as_str(), alpm::reason(p), wanted_name]);
    }
    if o.dry {
        ui::heading("Would change installation reasons", &ui::count(targets.len(), "package", "packages"));
        table.render();
        ui::blank();
        return Ok(());
    }
    tx::escalate(&Request { targets, ..Request::new(Operation::Reason) })
}

fn orphans(db: &Alpm, o: &Options) -> Result<()> {
    let packages = db.orphans();
    if o.quiet {
        ui::hold();
        for p in &packages {
            ui::print(&format!("{}\n", ui::safe(&p.name)));
        }
        ui::release();
        return Ok(());
    }
    if packages.is_empty() {
        ui::note(NoteKind::Success, "No orphaned dependencies.");
        return Ok(());
    }
    let title = if o.command == Command::Orphans {
        "Orphaned dependencies"
    } else if o.dry {
        "Would remove orphans"
    } else {
        "Orphans to remove"
    };
    ui::heading(title, &ui::count(packages.len(), "package", "packages"));
    let total = package_table(&packages);
    ui::blank();
    if o.command == Command::Orphans {
        ui::note(NoteKind::Info, &format!("{} in total · zap autoremove removes them.", ui::size(total)));
        ui::blank();
        return Ok(());
    }
    ui::note(NoteKind::Info, &format!("Frees at least {}; their own unneeded dependencies are removed too.", ui::size(total)));
    ui::blank();
    if o.dry {
        return Ok(());
    }
    let targets = packages.iter().map(|p| Target::named(&p.name)).collect();
    tx::escalate(&Request { targets, recursive: true, ..Request::new(Operation::Remove) })
}

fn clean(db: &mut Alpm, o: &Options) -> Result<()> {
    Builder::new(db, &o.names)?.clean(o.dry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, TempDir};

    #[test]
    fn local_archives_use_metadata_identities_and_reject_duplicates_and_symlinks() {
        let tmp = TempDir::new();
        let archive = testing::package_fixture(tmp.path(), "zap-archive-fixture", "");
        let arbitrary = format!("{}/arbitrary name.pkg.tar", tmp.str());
        std::fs::copy(&archive, &arbitrary).unwrap();
        let targets = archive_targets(&[&arbitrary], Some(Reason::Dependency)).unwrap();
        assert_eq!("zap-archive-fixture", targets[0].name);
        assert_eq!(Some(Reason::Dependency), targets[0].reason);
        let kind = |paths: &[&str]| archive_targets(paths, None).unwrap_err().kind;
        assert_eq!(Kind::DuplicateTarget, kind(&[&archive, &arbitrary]));
        let alias = format!("{}/alias.pkg.tar", tmp.str());
        std::os::unix::fs::symlink(&archive, &alias).unwrap();
        assert_eq!(Kind::UnsafeArchive, kind(&[&alias]));
        let text = format!("{}/not-a-package.pkg.tar", tmp.str());
        std::fs::write(&text, "plain text").unwrap();
        assert_eq!(Kind::InvalidBuildArchive, kind(&[&text]));
    }

    #[test]
    fn removal_is_refused_only_when_no_remaining_package_satisfies_a_dependency() {
        let tmp = TempDir::new();
        // Two providers of a virtual dependency, and a package that needs it.
        for (name, extra) in [("jdk-a", "%PROVIDES%\njava-runtime\n\n"), ("jdk-b", "%PROVIDES%\njava-runtime\n\n"), ("app", "%DEPENDS%\njava-runtime\n\n")] {
            testing::installed_fixture(tmp.path(), name, "");
            let desc = tmp.path().join(format!("db/local/{name}-1.0-1/desc"));
            let text = std::fs::read_to_string(&desc).unwrap();
            std::fs::write(&desc, format!("{text}{extra}")).unwrap();
        }
        let db = testing::alpm(tmp.path());
        let a = query::installed(&db, "jdk-a").unwrap().clone();
        let b = query::installed(&db, "jdk-b").unwrap().clone();
        // jdk-b still provides java-runtime: nothing breaks.
        assert!(broken_by(&db, &[a.clone()]).is_empty());
        assert_eq!(vec!["app needs java-runtime".to_string()], broken_by(&db, &[a, b]));
    }

    #[test]
    fn only_version_control_suffixes_mark_a_package_for_devel_rebuilds() {
        assert!(is_vcs("neovim-git"));
        assert!(is_vcs("tool-fossil"));
        for name in ["git", "gitui", "some-git-tool"] {
            assert!(!is_vcs(name), "{name}");
        }
    }
}
