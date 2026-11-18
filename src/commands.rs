//! Command dispatch. Each command is a short flow over the modules that do
//! the work.
use std::sync::atomic::{AtomicBool, Ordering};

use crate::alpm::{self, Alpm, PkgRef};
use crate::builder::{self, Builder};
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
            // Fail before planning, not after a plan nobody can confirm.
            // select says so itself, with its own hint. --needed may find
            // nothing to do, which a script must be able to run.
            if !o.noconfirm && !o.needed && o.command != Command::Select && unsafe { libc::isatty(0) } == 0 {
                return errors::fail(Kind::InteractiveTerminalRequired);
            }
        }
    }
    if o.command == Command::News {
        return news::show();
    }
    let mut db = Alpm::init()?;
    let o = strip_sources(&db, o)?;
    check_names(&o)?;

    let result = match o.command {
        // As pacman -Ss, finding nothing exits 1.
        Command::Search => query::search(&db, &o).and_then(|m| if m.is_empty() { errors::fail(Kind::NothingFound) } else { Ok(()) }),
        Command::Select => select(&mut db, &o),
        Command::Info | Command::Localinfo => query::info(&db, &o),
        Command::List | Command::Foreign => query::list(&db, &o),
        Command::LocalSearch => query::local_search(&db, &o),
        Command::Files => query::files(&db, &o.names, o.quiet),
        Command::Owns => query::owners(&db, &o.names, o.quiet),
        Command::Check => query::check(&db, &o.names),
        Command::Stats => query::stats(&db),
        // As with pacman -Qu, a script can test for updates by the exit status.
        Command::Updates => updates(&db, &o).and_then(|t| if t.is_empty() { errors::fail(Kind::NothingFound) } else { Ok(()) }),
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
    // Said last, under the plan; a failure already says what went wrong.
    // Nothing to do needs no reminder that nothing was done.
    if o.dry && result.is_ok() && !NOTHING_TO_DO.load(Ordering::Relaxed) {
        let fetched = builder::sources_fetched();
        let undone = match o.command {
            Command::Clean => "nothing was deleted",
            Command::Reason => "no installation reasons were changed",
            // Fetching did write to the cache, so "nothing changed" would not hold.
            _ if fetched => "nothing was built or installed",
            _ => "nothing changed",
        };
        let cache = if fetched { "; AUR build files were fetched to the build cache" } else { "" };
        ui::separate();
        ui::note(NoteKind::Info, &format!("Dry run: {undone}{cache}."));
        ui::blank();
    }
    result
}

/// Set when a command found nothing to do, so a dry run has nothing to disclaim.
static NOTHING_TO_DO: AtomicBool = AtomicBool::new(false);

pub fn nothing_to_do() {
    NOTHING_TO_DO.store(true, Ordering::Relaxed);
}

/// Names as search results show them, such as extra/ripgrep, aur/yay-bin
/// or local/zap. The prefix must name a configured repository, aur or
/// local. Installing and info take an aur/ target from the AUR and a
/// repository target from that repository; search limits itself to the
/// kind of source named; commands on installed packages just drop it.
fn strip_sources(db: &Alpm, mut o: Options) -> Result<Options> {
    use Command::*;
    let sourced = matches!(o.command, Install | Build | Get | Pkgbuild | Info);
    let searching = matches!(o.command, Search | Select);
    let local = matches!(o.command, Localinfo | Files | Remove | List | Check | Reason);
    if !(sourced || searching || local) {
        return Ok(o);
    }
    let repositories: Vec<&str> = db.h.syncdbs.iter().map(|d| d.name.as_str()).collect();
    for name in &mut o.names {
        // A path or an archive, typed as pacman -U or paru would take it.
        if matches!(o.command, Install | Build) && (name.starts_with(['.', '/']) || name.contains(".pkg.tar")) {
            let hint = if name.contains(".pkg.tar") { format!("Use zap install-files {name} for a package archive.") } else { format!("Use zap install-local {name} for a directory with a PKGBUILD.") };
            return Err(errors::with_hint(Kind::InvalidPackageName, hint, name.as_str()));
        }
        let Some((prefix, rest)) = name.split_once('/') else { continue };
        let rest = rest.to_string();
        if !util::valid_name(prefix) || rest.is_empty() {
            return Err(errors::with(Kind::InvalidPackageName, name.as_str()));
        }
        let known = prefix == "aur" || prefix == "local" || repositories.contains(&prefix);
        if prefix == "local" && !local {
            return Err(errors::with_hint(Kind::UnknownRepository, "local/ names an installed package. Use a repository or aur/ prefix here.", prefix));
        }
        if !known {
            // repo/ is a natural guess after reading about --repo.
            let flag = if prefix == "repo" { "repo/ is not a prefix; pass --repo to use any repository. " } else { "" };
            let hint = format!("{flag}Configured repositories: {}. Use aur/ for the AUR.", repositories.join(", "));
            return Err(errors::with_hint(Kind::UnknownRepository, hint, prefix));
        }
        if searching {
            o.scope = if prefix == "aur" { Scope::Aur } else { Scope::Repo };
        } else if sourced && prefix == "aur" {
            o.aur_names.push(rest.clone());
        } else if sourced {
            check_repository(db, o.command, prefix, &rest)?;
        }
        *name = rest;
    }
    Ok(o)
}

/// A repository/name target: zap takes a package from the first repository
/// that carries it, so that must be the one named.
fn check_repository(db: &Alpm, command: Command, repository: &str, name: &str) -> Result<()> {
    let command = command.tag().replace('_', "-");
    let named = db.h.syncdbs.iter().find(|d| d.name == repository).and_then(|d| d.get(name));
    if named.is_none() {
        let hint = match db.repo(name).filter(|p| p.name == name) {
            Some(p) => format!("{name} is in {0}: zap {command} {0}/{name}.", alpm::repository(&p)),
            None if aur::find(name).ok().flatten().is_some() => format!("{name} is in the AUR: zap {command} aur/{name}."),
            None => format!("Run zap search {name} to see where it is available."),
        };
        return Err(errors::with_hint(Kind::NotInRepository, hint, format!("{repository}/{name}")));
    }
    match db.repo(name) {
        Some(first) if alpm::repository(&first) != repository => {
            let hint = format!("zap installs {name} from {}, the first repository in pacman.conf that has it. Drop the prefix to use that one.", alpm::repository(&first));
            Err(errors::with_hint(Kind::ShadowedRepository, hint, format!("{repository}/{name}")))
        }
        _ => Ok(()),
    }
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
    builds.aur_targets = o.aur_names.clone();
    builds.needed = o.needed;
    builds.reason = o.reason;
    builds.sandbox = o.sandbox.unwrap_or(true);
    builds.clean_after = o.clean_after;
    builds.rebuild_tree = o.rebuild_tree;
    builds.build_only = matches!(o.command, Command::Build | Command::BuildLocal);
}

// ── Install and build ───────────────────────────────────────────────────────

fn install(db: &mut Alpm, o: &Options) -> Result<()> {
    let mut builds = Builder::new(db, &o.names)?;
    configure(&mut builds, o);
    builds.plan()?;
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
    // Check before searching, so a script does not wait on the AUR for nothing.
    if unsafe { libc::isatty(0) } == 0 {
        return errors::fail(Kind::SelectNeedsTerminal);
    }
    // The search already said that nothing matches and what to try.
    let matches = query::search(db, o)?;
    if matches.is_empty() {
        return errors::fail(Kind::NothingFound);
    }
    // A selection that cannot be installed is asked again, not fatal.
    let selected = loop {
        let example = if matches.len() > 1 { format!("1-{}", matches.len().min(3)) } else { "1".into() };
        let reply = ui::answer(&format!("Install which? Numbers or ranges such as {example}; empty cancels:"))?;
        match query::selection(&reply, matches.len()) {
            Ok(selected) => {
                // One name cannot be installed from both a repository and the AUR.
                let twice = selected.iter().find(|&&i| selected.iter().any(|&j| j != i && matches[j].name == matches[i].name));
                match twice {
                    Some(&i) => ui::note(NoteKind::Warning, &format!("{} was chosen from both a repository and the AUR. Choose one.", matches[i].name)),
                    None => break selected,
                }
            }
            Err(e) if e.kind == Kind::InvalidSelection => {
                ui::note(NoteKind::Warning, &format!("Use numbers from 1 to {}, or a range such as {example}.", matches.len()));
            }
            Err(e) => return Err(e),
        }
    };
    let mut names = Vec::new();
    let mut aur_names = Vec::new();
    for &index in &selected {
        let chosen = &matches[index];
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
    let mut vcs = 0;
    let remote = aur::info(&foreign)?;
    for name in foreign {
        let Some(p) = remote.iter().find(|candidate| candidate.Name == name) else {
            absent.push(name);
            continue;
        };
        let Some(old) = db.installed_pkg(name) else { continue };
        vcs += usize::from(is_vcs(name));
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
    // A dry upgrade compares against the same cached databases, so both say how old they are.
    let compared = (o.command == Command::Updates || o.dry) && o.scope != Scope::Aur;
    let synced = if compared { db.synced_at() } else { 0 };
    let stale = synced != 0 && ui::now() - synced > 24 * 3600;
    let refresh = if o.command == Command::Upgrade { "a real run" } else { "zap upgrade" };
    let age = ui::age(synced, ui::now());
    // How fresh the list is qualifies it, so it comes first, as under
    // upgrade, where the plan that follows is the list. A heading brings
    // its own blank line.
    if names.is_empty() || compared {
        ui::blank();
    }
    let freshness = format!("Repository databases were last refreshed {age}; {refresh} refreshes them first");
    let no_repo_updates = o.command == Command::Upgrade && o.dry && compared && !names.is_empty() && names.len() == targets.len();
    if names.is_empty() && stale {
        ui::note(NoteKind::Warning, &format!("No updates found. {freshness}."));
    } else if compared && synced == 0 {
        ui::note(NoteKind::Info, "Repository databases are as fresh as the last zap upgrade.");
    } else if stale && no_repo_updates {
        ui::note(NoteKind::Warning, &format!("No repository updates found. {freshness}."));
    } else if compared && stale {
        ui::note(NoteKind::Warning, &format!("{freshness}, so more updates may appear."));
    } else if compared {
        ui::note(NoteKind::Info, &format!("{freshness}."));
    }
    if o.devel && vcs == 0 && o.scope != Scope::Repo {
        ui::separate();
        ui::note(NoteKind::Info, "--devel found no installed VCS packages from the AUR to check.");
    }
    // The plan below holds AUR builds only, so say what the repositories add.
    let repo_updates = names.len() - targets.len();
    if o.command == Command::Upgrade && o.dry && repo_updates > 0 {
        let count = ui::count(repo_updates, "repository update", "repository updates");
        ui::note(NoteKind::Info, &format!("A real run applies {count} first, before any AUR build; zap updates --repo lists them."));
    } else if no_repo_updates && !stale {
        ui::note(NoteKind::Info, "No repository updates found; the plan below has only AUR builds.");
    }
    if names.is_empty() && !stale {
        // updates exits 1 here, as pacman -Qu does, so this is no ✓; upgrade has simply nothing to do.
        let kind = if o.command == Command::Updates { NoteKind::Info } else { NoteKind::Success };
        ui::note(kind, if o.scope == Scope::Aur { "AUR packages are up to date." } else { "Everything is up to date." });
    } else if o.command == Command::Updates && !names.is_empty() {
        ui::heading("Updates", &ui::count(names.len(), "package", "packages"));
        table.render();
        ui::blank();
        ui::note(NoteKind::Info, "zap upgrade applies these updates.");
    }
    // Said by updates only: an upgrade would repeat it every time.
    if !absent.is_empty() && o.command == Command::Updates {
        ui::note(NoteKind::Info, &format!("Not available in the AUR, so zap will not rebuild them: {}", absent.join(", ")));
    }
    if names.is_empty() || o.command == Command::Updates {
        ui::blank();
    }
    Ok(targets)
}

fn upgrade(db: &mut Alpm, o: &Options) -> Result<()> {
    if o.scope != Scope::Aur && !o.dry {
        tx::escalate(&Request::new(Operation::Upgrade))?;
        db.reload()?;
    }
    let targets = updates(db, o)?;
    if targets.is_empty() {
        nothing_to_do();
        return Ok(());
    }
    let mut builds = Builder::new(db, &targets)?;
    configure(&mut builds, o);
    builds.scope = Scope::Aur;
    builds.preserve_reasons = true;
    builds.updating = true;
    builds.repos_upgraded = o.scope != Scope::Aur;
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
    // What would actually break, as libalpm decides it: a dependency that no
    // remaining installed package satisfies. Refused before the plan is shown.
    let broken = broken_by(db, &packages);
    if !broken.is_empty() {
        return Err(required_by_others(&broken, &o.names));
    }
    // --recursive also removes the dependencies nothing else needs, as
    // libalpm will select them; list them rather than leave them vague.
    let mut all = packages.clone();
    if o.recursive {
        libalpm::deps::recursedeps(db.installed(), &mut all, false);
    }
    for p in &all[packages.len()..] {
        total += p.info().isize;
        table.add_text(&[p.name.clone(), p.version.clone(), ui::size(p.info().isize), "only the packages above".into()]);
    }
    ui::heading(if o.dry { "Would remove" } else { "Remove" }, &ui::count(all.len(), "package", "packages"));
    table.render();
    ui::blank();
    let nosave = if o.nosave { "; configuration files are deleted, not kept as .pacsave" } else { "" };
    ui::note(NoteKind::Info, &format!("Frees {}{nosave}.", ui::size(total)));
    if o.dry {
        ui::blank();
        return Ok(());
    }
    tx::escalate(&Request { targets, recursive: o.recursive, nosave: o.nosave, ..Request::new(Operation::Remove) })
}

/// The refusal for a removal that would break other packages: which targets
/// are still needed, the first few broken dependencies, and how to proceed.
fn required_by_others(broken: &[Broken], removing: &[String]) -> errors::Error {
    let mut blocked: Vec<&str> = Vec::new();
    let mut dependants: Vec<&str> = Vec::new();
    for b in broken {
        if !blocked.contains(&b.target.as_str()) {
            blocked.push(&b.target);
        }
        if !dependants.contains(&b.dependant.as_str()) {
            dependants.push(&b.dependant);
        }
    }
    let it = if blocked.len() == 1 { "it" } else { "them" };
    let hint = if dependants.len() <= 5 {
        let needs = broken.iter().map(|b| format!("{} needs {}", b.dependant, b.depend)).collect::<Vec<_>>().join(", ");
        format!("{needs}. Keep {it}, or remove them together: zap remove {} {}", removing.join(" "), dependants.join(" "))
    } else {
        // With one target there is nothing left to run without it.
        let drop = if removing.len() > 1 { format!(" Drop {it} from the command;") } else { String::new() };
        format!(
            "{} need {it}, such as {}.{drop} zap localinfo {} lists what needs {it}.",
            ui::count(dependants.len(), "installed package", "installed packages"),
            dependants[..3].join(", "),
            blocked.join(" "),
        )
    };
    errors::with_hint(Kind::RequiredByOthers, hint, blocked.join(", "))
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
/// `removals` are gone, as (dependant, dependency) in dependant order.
/// An installed package that would lose a dependency: `dependant` needs
/// `depend`, which only `target`, a package being removed, provides.
#[derive(Debug, PartialEq)]
struct Broken {
    dependant: String,
    depend: String,
    target: String,
}

fn broken_by(db: &Alpm, removals: &[PkgRef]) -> Vec<Broken> {
    libalpm::deps::checkdeps(db.installed(), removals, &[], true)
        .into_iter()
        .map(|missing| {
            let target = missing.causing.unwrap_or_else(|| missing.depend.name.clone());
            Broken { dependant: missing.target, depend: missing.depend.to_string(), target }
        })
        .collect()
}

fn reason(db: &Alpm, o: &Options) -> Result<()> {
    let wanted = o.reason.ok_or(Kind::InstallationReasonRequired)?;
    let wanted_name = match wanted {
        Reason::Explicit => "explicit",
        Reason::Dependency => "dependency",
    };
    let (mut targets, mut unchanged) = (Vec::new(), Vec::new());
    let mut table = Table::new(&[Column::new("Package").tone(Tone::Bold), Column::new("Current"), Column::new("New").tone(Tone::Accent).joiner(" → ")]);
    for name in &o.names {
        let p = query::installed(db, name)?;
        if alpm::reason(p) == wanted_name {
            unchanged.push(name.as_str());
            continue;
        }
        targets.push(Target { reason: Some(wanted), ..Target::named(name) });
        table.add_text(&[name.as_str(), alpm::reason(p), wanted_name]);
    }
    // Already right needs no change, and so no administrator access.
    if !unchanged.is_empty() {
        let installed = if wanted == Reason::Explicit { "explicitly installed" } else { "installed as a dependency" };
        let (subject, verb) = if unchanged.len() == 1 { (unchanged[0].to_string(), "is") } else { (unchanged.join(", "), "are") };
        ui::blank();
        ui::note(if targets.is_empty() { NoteKind::Success } else { NoteKind::Info }, &format!("{subject} {verb} already {installed}."));
        if targets.is_empty() {
            nothing_to_do();
            ui::blank();
            return Ok(());
        }
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
        return if packages.is_empty() && o.command == Command::Orphans { errors::fail(Kind::NothingFound) } else { Ok(()) };
    }
    if packages.is_empty() {
        nothing_to_do();
        // orphans exits 1 here, as pacman -Qdt does, so this is no ✓.
        let (kind, text) = if o.command == Command::Orphans { (NoteKind::Info, "No orphaned dependencies.") } else { (NoteKind::Success, "Nothing to remove: no orphaned dependencies.") };
        ui::blank();
        ui::note(kind, text);
        ui::blank();
        // As pacman -Qdt, listing nothing exits 1; autoremove simply has nothing to do.
        return if o.command == Command::Orphans { errors::fail(Kind::NothingFound) } else { Ok(()) };
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
        assert!(broken_by(&db, std::slice::from_ref(&a)).is_empty());
        let broken = Broken { dependant: "app".into(), depend: "java-runtime".into(), target: "jdk-a".into() };
        assert_eq!(vec![broken], broken_by(&db, &[a, b]));
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
