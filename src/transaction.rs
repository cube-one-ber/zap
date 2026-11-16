//! System changes. The unprivileged process validates a request and hands it
//! to the root worker through run0 or sudo; the worker validates it again,
//! stages archives privately, and commits one transaction after a final
//! confirmation.
use crate::alpm::{self, Alpm};
use crate::cli::{Elevator, Reason};
use crate::errors::{self, Kind, Result};
use crate::libalpm::Reason as PkgReason;
use crate::ui::{self, Cell, Column, NoteKind, Table, Tone};
use crate::{signals, util};
use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Mutex;

// The request crosses the privilege boundary as JSON. Its shape is a
// compatibility contract: during an upgrade of zap itself, a running older
// process talks to the newly installed worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Install,
    Remove,
    Upgrade,
    Reason,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub name: String,
    // Defaults are left out of the JSON so a request naming every installed
    // package still fits in the worker's one argument (REQUEST_LIMIT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dependency: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

impl Target {
    pub fn named(name: &str) -> Target {
        Target { name: name.into(), ..Target::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub operation: Operation,
    #[serde(default)]
    pub targets: Vec<Target>,
    #[serde(default)]
    pub recursive: bool,
    #[serde(default)]
    pub nosave: bool,
    #[serde(default)]
    pub needed: bool,
    /// Commit without a confirmation or a terminal. `escalate` sets it from
    /// --noconfirm; it is left out of the JSON when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub noconfirm: bool,
}

impl Request {
    pub fn new(operation: Operation) -> Request {
        Request { operation, targets: Vec::new(), recursive: false, nosave: false, needed: false, noconfirm: false }
    }
}

const WORKER_PATH: &str = "/usr/bin/zap";
/// The request travels as one argument, and Linux caps a single argument at
/// 128 KiB (MAX_ARG_STRLEN). This bound, not a target count, limits a request.
pub const REQUEST_LIMIT: usize = 96 * 1024;
const ARCHIVE_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const SOURCE_LIMIT: u64 = 16 * 1024 * 1024;

// ── Unprivileged side ───────────────────────────────────────────────────────

/// The elevation tool chosen with --elevate; otherwise $ZAP_ELEVATE decides,
/// and then what the system provides.
static REQUESTED_ELEVATOR: Mutex<Option<Elevator>> = Mutex::new(None);

pub fn set_elevator(choice: Option<Elevator>) {
    *REQUESTED_ELEVATOR.lock().unwrap_or_else(|e| e.into_inner()) = choice;
}

fn present(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

/// The tool chosen with --elevate or ZAP_ELEVATE, if any. An unknown
/// ZAP_ELEVATE value is refused.
pub fn chosen_elevator() -> Result<Option<Elevator>> {
    if let Some(choice) = *REQUESTED_ELEVATOR.lock().unwrap_or_else(|e| e.into_inner()) {
        return Ok(Some(choice));
    }
    let value = std::env::var_os("ZAP_ELEVATE").unwrap_or_default();
    if value.is_empty() {
        return Ok(None);
    }
    let value = value.to_string_lossy();
    match Elevator::parse(&value) {
        Some(tool) => Ok(Some(tool)),
        None => Err(errors::with(Kind::InvalidElevator, format!("ZAP_ELEVATE={value}"))),
    }
}

/// run0 needs systemd as the running service manager and polkit to
/// authenticate; sudo is the fallback.
pub fn elevator() -> Result<Elevator> {
    if let Some(choice) = chosen_elevator()? {
        return Ok(choice);
    }
    if present("/run/systemd/system") && present("/usr/lib/polkit-1/polkitd") && present(elevator_path(Elevator::Run0)) {
        return Ok(Elevator::Run0);
    }
    if present(elevator_path(Elevator::Sudo)) {
        return Ok(Elevator::Sudo);
    }
    errors::fail(Kind::ElevatorUnavailable)
}

fn elevator_path(tool: Elevator) -> &'static str {
    match tool {
        Elevator::Run0 => "/usr/bin/run0",
        Elevator::Sudo => "/usr/bin/sudo",
    }
}

/// Everything a system change needs, checked before any work that would be
/// wasted without it: the trusted worker, a trusted elevation tool, and,
/// unless unattended, a terminal for the confirmations. Returns the tool.
pub fn preflight() -> Result<Elevator> {
    trusted_executable()?;
    let tool = trusted_elevator()?;
    if !ui::unattended() && unsafe { libc::isatty(0) } == 0 {
        return errors::fail(Kind::InteractiveTerminalRequired);
    }
    Ok(tool)
}

/// The chosen elevation tool, refused unless root controls it.
pub fn trusted_elevator() -> Result<Elevator> {
    let tool = elevator()?;
    trusted_tool(elevator_path(tool))?;
    Ok(tool)
}

/// Run a request in the root worker. The worker shows the final transaction
/// and asks for confirmation itself.
pub fn escalate(request: &Request) -> Result<()> {
    let request = &Request { noconfirm: ui::unattended(), ..request.clone() };
    validate(request)?;
    let tool = preflight()?;
    let json = serde_json::to_string(request).map_err(|_| Kind::InvalidTransactionRequest)?;
    if json.len() > REQUEST_LIMIT {
        return errors::fail(Kind::TransactionTooLarge);
    }
    let action = match request.operation {
        Operation::Install => "install packages",
        Operation::Remove => "remove packages",
        Operation::Upgrade => "upgrade the system",
        Operation::Reason => "change installation reasons",
    };
    ui::note(NoteKind::Info, &format!("Asking {} for administrator access to {action}.", tool.name()));
    // Ctrl-C belongs to the worker while it runs: this process stays to
    // report how the worker ended instead of returning the terminal early.
    signals::defer_interrupts();
    let terminal = unsafe { libc::isatty(0) } != 0;
    let status = util::run_status(&elevated_args(tool, &json, ui::styled(), terminal), Some("/"));
    signals::restore();
    outcome(status?)
}

/// The worker command line for each tool. run0 starts the worker with a clean
/// environment, so the one presentation preference is carried as a constant;
/// sudo's environment policy may refuse that, so under sudo the worker styles
/// output by what its terminal supports. Without a `terminal` (an unattended
/// run), run0 pipes the worker's output and polkit asks for authentication
/// through the desktop's agent, an on-screen dialog.
fn elevated_args(tool: Elevator, json: &str, styled: bool, terminal: bool) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    match tool {
        Elevator::Run0 => {
            let stdio = if terminal { "--pty" } else { "--pipe" };
            args.extend([elevator_path(Elevator::Run0), stdio, "--background=", "--chdir=/"].map(String::from));
            if !styled {
                args.push("--setenv=NO_COLOR=1".into());
            }
        }
        Elevator::Sudo => args.push(elevator_path(Elevator::Sudo).into()),
    }
    args.extend(["--", WORKER_PATH, "__transaction", json].map(String::from));
    args
}

/// The worker reports its own failures, and run0 or sudo theirs; only the
/// outcome travels back, as the exit status.
fn outcome(status: u8) -> Result<()> {
    match status {
        0 => Ok(()),
        errors::EXIT_CANCELLED => errors::fail(Kind::WorkerCancelled),
        // Killed or crashed: nothing else has said so. run_status gives
        // 128 + N for signal N and 255 for any other abnormal end.
        129 | 131..=255 => Err(if status == 255 || status > 128 + 31 {
            errors::with(Kind::WorkerInterrupted, format!("the worker ended with status {status}"))
        } else {
            errors::with(Kind::WorkerInterrupted, format!("the worker ended by signal {}", status - 128))
        }),
        _ => errors::fail(Kind::WorkerFailed),
    }
}

/// Whether `meta` is owned by root and writable by nobody else.
fn root_only(meta: &std::fs::Metadata) -> bool {
    meta.uid() == 0 && meta.mode() & 0o022 == 0
}

/// The elevation tool must be root's: its directory and the file it resolves
/// to are owned by root and writable by nobody else. run0 is a link to
/// systemd-run, so the link itself is followed.
fn trusted_tool(path: &str) -> Result<()> {
    for dir in ["/", "/usr", "/usr/bin"] {
        match std::fs::symlink_metadata(dir) {
            Ok(meta) if root_only(&meta) && meta.is_dir() => {}
            _ => return Err(errors::with(Kind::UntrustedWorkerPath, dir)),
        }
    }
    let link = std::fs::symlink_metadata(path).map_err(|_| errors::with(Kind::ElevatorUnavailable, path))?;
    if link.uid() != 0 {
        return Err(errors::with(Kind::UntrustedWorkerPath, path));
    }
    match std::fs::metadata(path) {
        Ok(meta) if root_only(&meta) && meta.is_file() => Ok(()),
        _ => Err(errors::with(Kind::UntrustedWorkerPath, path)),
    }
}

/// Only a root-controlled executable may be elevated: every component of the
/// worker's path must be owned by root, not writable by others, and not a link.
pub fn trusted_executable() -> Result<()> {
    for path in ["/", "/usr", "/usr/bin", WORKER_PATH] {
        let meta = std::fs::symlink_metadata(path).map_err(|_| Kind::WorkerNotInstalled)?;
        if !root_only(&meta) || meta.file_type().is_symlink() {
            return Err(errors::with(Kind::UntrustedWorkerPath, path));
        }
    }
    Ok(())
}

/// Structural validation, applied on both sides of the privilege boundary.
pub fn validate(request: &Request) -> Result<()> {
    let op = request.operation;
    if op != Operation::Remove && (request.recursive || request.nosave) {
        return errors::fail(Kind::InvalidTransactionFlags);
    }
    if request.needed && op != Operation::Install {
        return errors::fail(Kind::InvalidTransactionFlags);
    }
    for (i, target) in request.targets.iter().enumerate() {
        if target.reason.is_some() && op != Operation::Install && op != Operation::Reason {
            return errors::fail(Kind::InvalidTransactionFlags);
        }
        if op == Operation::Reason && target.reason.is_none() {
            return errors::fail(Kind::InstallationReasonRequired);
        }
        if !util::valid_name(&target.name) {
            return errors::fail(Kind::InvalidPackageName);
        }
        if request.targets[..i].iter().any(|previous| previous.name == target.name) {
            return errors::fail(Kind::DuplicateTarget);
        }
        match (&target.archive, &target.sha256) {
            (Some(path), hash) => {
                if op != Operation::Install || !path.starts_with('/') {
                    return errors::fail(Kind::InvalidArchive);
                }
                let hash = hash.as_deref().ok_or(Kind::MissingDigest)?;
                if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                    return errors::fail(Kind::InvalidDigest);
                }
            }
            (None, Some(_)) => return errors::fail(Kind::InvalidDigest),
            (None, None) => {}
        }
    }
    if op != Operation::Upgrade && request.targets.is_empty() {
        return errors::fail(Kind::NoTargets);
    }
    Ok(())
}

/// The HoldPkg pattern that protects `name` from removal, if any.
pub fn held<'c>(hold: &'c [String], name: &str) -> Option<&'c str> {
    hold.iter().find(|pattern| crate::libalpm::fnmatch(pattern, name)).map(String::as_str)
}

// ── Root worker ─────────────────────────────────────────────────────────────

const SIGNATURE_LIMIT: u64 = 64 * 1024;

/// The root side of `escalate`: validate the request again, stage its
/// archives, resolve one transaction, show it and commit it once confirmed.
pub fn worker(bytes: &str) -> Result<()> {
    if unsafe { libc::geteuid() != 0 || libc::getuid() != 0 } {
        return errors::fail(Kind::RootWorkerRequired);
    }
    if bytes.len() > REQUEST_LIMIT {
        return errors::fail(Kind::TransactionTooLarge);
    }
    // Unknown fields are refused: a request this worker does not fully
    // understand is never partially applied.
    let request: Request = serde_json::from_str(bytes).map_err(|_| Kind::InvalidTransactionRequest)?;
    ui::set_unattended(request.noconfirm);
    if !request.noconfirm && unsafe { libc::isatty(0) } == 0 {
        return errors::fail(Kind::InteractiveTerminalRequired);
    }
    signals::install();
    validate(&request)?;
    let mut db = Alpm::init()?;

    let staging = Staging::new()?;
    let archives = stage(&request, &staging.0)?;
    signals::check()?;

    if request.operation == Operation::Upgrade {
        ui::heading("Refreshing repository databases", "");
        db.h.db_update(false).map_err(|e| Alpm::failed(&e))?;
    }
    signals::check()?;
    // Take the normal lock. Another process's lock is never removed.
    db.h.trans_init(flags(&request)).map_err(|e| Alpm::failed(&e))?;
    let result = run_transaction(&mut db, &request, &archives);
    db.h.trans_release();
    result
}

/// A root-only directory under /var/tmp, removed with its contents on drop.
struct Staging(String);

impl Staging {
    fn new() -> Result<Staging> {
        let mut template = *b"/var/tmp/zap-transaction-XXXXXX\0";
        if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
            return errors::fail(Kind::StagingFailed);
        }
        Ok(Staging(String::from_utf8_lossy(&template[..template.len() - 1]).into_owned()))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Everything between opening and releasing the transaction.
fn run_transaction(db: &mut Alpm, request: &Request, archives: &[Option<String>]) -> Result<()> {
    if request.operation == Operation::Reason {
        for target in &request.targets {
            if db.installed_pkg(&target.name).is_none() {
                return Err(errors::with(Kind::PackageNotInstalled, &target.name));
            }
        }
        return apply_reasons(db, &request.targets, None);
    }

    // Reasons are decided against the state before this transaction.
    let was_explicit: HashSet<String> =
        db.installed().iter().filter(|p| alpm::explicit(p)).map(|p| p.name.clone()).collect();

    if request.operation == Operation::Upgrade {
        db.h.sync_sysupgrade(false).map_err(|e| Alpm::failed(&e))?;
    }
    queue(db, request, archives)?;
    if let Err(e) = db.h.trans_prepare() {
        db.explain(&e);
        return Err(Alpm::failed(&e));
    }
    signals::check()?;

    let reason_changes: Vec<Target> = if request.operation == Operation::Install {
        request
            .targets
            .iter()
            .filter(|t| {
                db.installed_pkg(&t.name)
                    .is_some_and(|p| p.info().reason != install_reason(t, was_explicit.contains(&t.name)))
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let trans = db.h.trans();
    if trans.add.is_empty() && trans.remove.is_empty() {
        if reason_changes.is_empty() {
            ui::note(NoteKind::Success, "Nothing to do: everything is up to date.");
            return Ok(());
        }
        return apply_reasons(db, &reason_changes, Some(&was_explicit));
    }

    protect_held(db)?;
    summarize(db, &reason_changes, &was_explicit);
    ui::require("Commit this transaction, including package scripts and hooks? [y/N]")?;
    signals::check()?;
    if let Err(e) = db.h.trans_commit() {
        db.explain(&e);
        return Err(Alpm::failed(&e));
    }
    if request.operation != Operation::Remove {
        for target in &request.targets {
            let Some(p) = db.installed_pkg(&target.name).cloned() else { continue };
            let reason = install_reason(target, was_explicit.contains(&target.name));
            db.h.pkg_set_reason(&p, reason).map_err(|e| Alpm::failed(&e))?;
        }
    }
    ui::blank();
    ui::note(NoteKind::Success, "Transaction complete.");
    Ok(())
}

/// Copy every requested archive, and its detached signature if one exists,
/// into root-only staging, and check each copy against the approved digest.
/// The transaction only ever reads the copies.
fn stage(request: &Request, staging: &str) -> Result<Vec<Option<String>>> {
    let mut archives = Vec::with_capacity(request.targets.len());
    for (i, target) in request.targets.iter().enumerate() {
        let Some(path) = &target.archive else {
            archives.push(None);
            continue;
        };
        let snapshot = format!("{staging}/{i}.pkg.tar");
        let actual = copy_hash(path, Some(&snapshot), ARCHIVE_LIMIT, false)?;
        if !actual.eq_ignore_ascii_case(target.sha256.as_deref().unwrap_or_default()) {
            return Err(errors::with(Kind::ArchiveDigestMismatch, path));
        }
        let signature = format!("{path}.sig");
        if std::fs::symlink_metadata(&signature).is_ok() {
            copy_hash(&signature, Some(&format!("{snapshot}.sig")), SIGNATURE_LIMIT, false)?;
        }
        archives.push(Some(snapshot));
    }
    Ok(archives)
}

/// Add every target to the open transaction.
fn queue(db: &mut Alpm, request: &Request, archives: &[Option<String>]) -> Result<()> {
    for (target, staged) in request.targets.iter().zip(archives) {
        if request.operation == Operation::Remove {
            let p = db.installed_pkg(&target.name).cloned();
            let p = p.ok_or_else(|| errors::with(Kind::PackageNotInstalled, &target.name))?;
            db.h.remove_pkg(&p).map_err(|e| Alpm::failed(&e))?;
            continue;
        }
        let p = match staged {
            Some(path) => {
                // Staged archives are held to the configured local signature policy.
                let level = db.h.local_file_siglevel;
                let p = db.h.pkg_load(path, true, level).map_err(|e| Alpm::failed(&e))?;
                if p.name != target.name {
                    return Err(errors::with(Kind::ArchiveNameMismatch, &target.name));
                }
                p
            }
            None => db.repo(&target.name).ok_or_else(|| errors::with(Kind::RepositoryPackageNotFound, &target.name))?,
        };
        db.h.add_pkg(p).map_err(|e| Alpm::failed(&e))?;
    }
    Ok(())
}

/// HoldPkg entries are never removed, whatever pulled them into the transaction.
fn protect_held(db: &Alpm) -> Result<()> {
    for p in &db.h.trans().remove {
        if let Some(pattern) = held(&db.cfg.hold, &p.name) {
            return Err(errors::with(Kind::ProtectedPackage, format!("{} (HoldPkg {pattern})", p.name)));
        }
    }
    Ok(())
}

/// Show and, once confirmed, apply reason changes that need no transaction.
/// `was_explicit` is `None` when every target carries its own reason.
fn apply_reasons(db: &mut Alpm, targets: &[Target], was_explicit: Option<&HashSet<String>>) -> Result<()> {
    ui::heading("Installation reasons", &ui::count(targets.len(), "change", "changes"));
    show_reasons(db, targets, was_explicit);
    ui::blank();
    ui::require("Apply these installation reasons? [y/N]")?;
    signals::check()?;
    for target in targets {
        let p = db.installed_pkg(&target.name).cloned();
        let p = p.ok_or_else(|| errors::with(Kind::PackageNotInstalled, &target.name))?;
        let reason = install_reason(target, was_explicit.is_some_and(|set| set.contains(&target.name)));
        db.h.pkg_set_reason(&p, reason).map_err(|e| Alpm::failed(&e))?;
    }
    ui::note(NoteKind::Success, "Installation reasons updated.");
    Ok(())
}

fn show_reasons(db: &Alpm, targets: &[Target], was_explicit: Option<&HashSet<String>>) {
    let mut table = Table::new(&[
        Column::new("Package").tone(Tone::Bold),
        Column::new("Current"),
        Column::new("New").tone(Tone::Accent).joiner(" → "),
    ]);
    for target in targets {
        let current = db.installed_pkg(&target.name).map_or("—", |p| alpm::reason(p));
        let wanted = install_reason(target, was_explicit.is_some_and(|set| set.contains(&target.name)));
        table.add_text(&[target.name.as_str(), current, if wanted == PkgReason::Explicit { "explicit" } else { "dependency" }]);
    }
    table.render();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Change {
    Install,
    Upgrade,
    Downgrade,
    Reinstall,
    Remove,
}

impl Change {
    const ALL: [Change; 5] = [Change::Install, Change::Upgrade, Change::Downgrade, Change::Reinstall, Change::Remove];

    fn of(version: &str, old: Option<&str>) -> Change {
        match old.map(|o| crate::libalpm::vercmp(version, o)) {
            None => Change::Install,
            Some(c) if c > 0 => Change::Upgrade,
            Some(c) if c < 0 => Change::Downgrade,
            Some(_) => Change::Reinstall,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Change::Install => "install",
            Change::Upgrade => "upgrade",
            Change::Downgrade => "downgrade",
            Change::Reinstall => "reinstall",
            Change::Remove => "remove",
        }
    }
}

/// The final review: exactly what the transaction resolved, before anything changes.
fn summarize(db: &Alpm, reason_changes: &[Target], was_explicit: &HashSet<String>) {
    let mut counts = [0usize; 5];
    let (mut net, mut download) = (0i64, 0i64);
    let mut table = Table::new(&[
        Column::new("Package").tone(Tone::Bold),
        Column::new("Current"),
        Column::new("New").joiner(" → "),
        Column::new("Change"),
        Column::new("Size").tone(Tone::Muted).right(),
    ]);
    let trans = db.h.trans();
    for add in &trans.add {
        let p = &add.pkg;
        let old = db.installed_pkg(&p.name);
        let kind = Change::of(&p.version, old.map(|o| o.version.as_str()));
        let delta = p.info().isize - old.map_or(0, |o| o.info().isize);
        counts[kind as usize] += 1;
        net += delta;
        download += db.h.download_size(p);
        table.add(vec![
            Cell::new(p.name.as_str()),
            Cell::new(old.map_or("—", |o| o.version.as_str())),
            Cell::toned(p.version.as_str(), Tone::Success),
            Cell::toned(kind.name(), if kind == Change::Downgrade { Tone::Warning } else { Tone::Accent }),
            Cell::new(ui::size_delta(delta)),
        ]);
    }
    for p in &trans.remove {
        counts[Change::Remove as usize] += 1;
        net -= p.info().isize;
        table.add(vec![
            Cell::new(p.name.as_str()),
            Cell::new(p.version.as_str()),
            Cell::toned("—", Tone::Muted),
            Cell::toned("remove", Tone::Danger),
            Cell::new(ui::size_delta(-p.info().isize)),
        ]);
    }
    let parts: Vec<String> = Change::ALL
        .iter()
        .filter(|k| counts[**k as usize] > 0)
        .map(|k| format!("{} to {}", counts[*k as usize], k.name()))
        .collect();
    ui::heading("Transaction", &parts.join(" · "));
    table.render();
    ui::blank();
    if download > 0 {
        ui::field("Download", &ui::size(download));
    }
    ui::field("Disk space", &ui::size_delta(net));
    if !reason_changes.is_empty() {
        ui::heading("Installation reasons", &ui::count(reason_changes.len(), "change", "changes"));
        show_reasons(db, reason_changes, Some(was_explicit));
    }
    ui::blank();
}

/// The transaction flags a request may set; nothing else can be asked for.
pub fn flags(request: &Request) -> u32 {
    use crate::libalpm::trans::{FLAG_NEEDED, FLAG_NOSAVE, FLAG_RECURSE};
    let mut result = 0;
    if request.needed {
        result |= FLAG_NEEDED;
    }
    if request.operation == Operation::Remove && request.recursive {
        result |= FLAG_RECURSE;
    }
    if request.nosave {
        result |= FLAG_NOSAVE;
    }
    result
}

/// The reason a target should have after installation: an explicit request
/// wins; otherwise a package that was explicit stays explicit.
pub fn install_reason(target: &Target, was_explicit: bool) -> PkgReason {
    match target.reason {
        Some(Reason::Explicit) => PkgReason::Explicit,
        Some(Reason::Dependency) => PkgReason::Depend,
        None if !target.dependency || was_explicit => PkgReason::Explicit,
        None => PkgReason::Depend,
    }
}

// ── Hashing ─────────────────────────────────────────────────────────────────

/// SHA-256 of a package archive.
pub fn hash_file(path: &str) -> Result<String> {
    copy_hash(path, None, ARCHIVE_LIMIT, false)
}

/// SHA-256 of a tracked build file, which may be empty.
pub fn hash_source_file(path: &str) -> Result<String> {
    copy_hash(path, None, SOURCE_LIMIT, true)
}

/// Hash a regular file read through one descriptor opened without following
/// links, optionally writing the same bytes to a new private file. The copy
/// therefore has exactly the content that was hashed.
fn copy_hash(path: &str, dest: Option<&str>, limit: u64, allow_empty: bool) -> Result<String> {
    use sha2::Digest;
    let unsafe_archive = || errors::with(Kind::UnsafeArchive, path);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| unsafe_archive())?;
    let meta = file.metadata().map_err(|_| unsafe_archive())?;
    if !meta.is_file() || (!allow_empty && meta.len() == 0) || meta.len() > limit {
        return Err(unsafe_archive());
    }
    let mut out = match dest {
        Some(target) => Some(std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(target)?),
        None => None,
    };
    let mut hash = sha2::Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        signals::check()?;
        total += n as u64;
        if total > limit {
            return Err(errors::with(Kind::ArchiveTooLarge, path));
        }
        hash.update(&buf[..n]);
        if let Some(file) = out.as_mut() {
            file.write_all(&buf[..n])?;
        }
    }
    if total != meta.len() {
        return Err(errors::with(Kind::ArchiveChanged, path));
    }
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;

    fn request(operation: Operation, targets: Vec<Target>) -> Request {
        Request { targets, ..Request::new(operation) }
    }

    fn fails(r: Request, kind: Kind) {
        assert_eq!(kind, validate(&r).unwrap_err().kind(), "{r:?}");
    }

    #[test]
    fn worker_flags_cannot_weaken_unrelated_operations_and_unsafe_manifests_are_refused() {
        let reason = |r| Target { reason: Some(r), ..Target::named("foo") };
        fails(Request { needed: true, ..request(Operation::Remove, vec![Target::named("foo")]) }, Kind::InvalidTransactionFlags);
        fails(request(Operation::Reason, vec![Target::named("foo")]), Kind::InstallationReasonRequired);
        fails(request(Operation::Upgrade, vec![reason(Reason::Dependency)]), Kind::InvalidTransactionFlags);
        validate(&request(Operation::Reason, vec![reason(Reason::Dependency)])).unwrap();

        let archive = |path: &str, hash: Option<&str>| Target { archive: Some(path.into()), sha256: hash.map(Into::into), ..Target::named("foo") };
        let zeros = "0".repeat(64);
        fails(request(Operation::Install, vec![Target::named("../bad")]), Kind::InvalidPackageName);
        fails(request(Operation::Install, vec![archive("/tmp/foo.pkg.tar.zst", Some("short"))]), Kind::InvalidDigest);
        fails(request(Operation::Remove, vec![Target::named("foo"), Target::named("foo")]), Kind::DuplicateTarget);
        fails(request(Operation::Install, vec![archive("relative.pkg.tar", Some(&zeros))]), Kind::InvalidArchive);
        fails(request(Operation::Remove, vec![archive("/tmp/foo.pkg.tar", Some(&zeros))]), Kind::InvalidArchive);
        fails(request(Operation::Install, vec![archive("/tmp/foo.pkg.tar", None)]), Kind::MissingDigest);
        fails(request(Operation::Install, vec![]), Kind::NoTargets);
        validate(&request(Operation::Upgrade, vec![])).unwrap();
    }

    #[test]
    fn each_elevation_tool_runs_the_fixed_worker_and_only_run0_carries_the_color_preference() {
        let json = "{\"operation\":\"upgrade\"}";
        assert_eq!(
            format!("/usr/bin/run0 --pty --background= --chdir=/ --setenv=NO_COLOR=1 -- /usr/bin/zap __transaction {json}"),
            elevated_args(Elevator::Run0, json, false, true).join(" ")
        );
        assert!(!elevated_args(Elevator::Run0, json, true, true).join(" ").contains("NO_COLOR"));
        // Unattended, with no terminal: run0 pipes, and polkit's desktop agent authenticates.
        assert_eq!(
            format!("/usr/bin/run0 --pipe --background= --chdir=/ -- /usr/bin/zap __transaction {json}"),
            elevated_args(Elevator::Run0, json, true, false).join(" ")
        );
        assert_eq!(format!("/usr/bin/sudo -- /usr/bin/zap __transaction {json}"), elevated_args(Elevator::Sudo, json, false, true).join(" "));
        // An explicit choice wins over the environment and the system.
        set_elevator(Some(Elevator::Sudo));
        let chosen = elevator();
        set_elevator(None);
        assert_eq!(Elevator::Sudo, chosen.unwrap());
    }

    #[test]
    fn elevation_tools_must_belong_to_root() {
        assert_eq!(Kind::ElevatorUnavailable, trusted_tool("/usr/bin/no-such-elevator").unwrap_err().kind());
        let tmp = TempDir::new();
        let fake = tmp.path().join("fake-sudo");
        std::fs::write(&fake, "").unwrap();
        // A file the user owns is refused whatever its location.
        assert_eq!(Kind::UntrustedWorkerPath, trusted_tool(fake.to_str().unwrap()).unwrap_err().kind());
    }

    #[test]
    fn the_workers_exit_status_becomes_the_outcome_without_a_second_report() {
        outcome(0).unwrap();
        for (status, kind) in [(130, Kind::WorkerCancelled), (1, Kind::WorkerFailed), (2, Kind::WorkerFailed), (137, Kind::WorkerInterrupted), (255, Kind::WorkerInterrupted)] {
            assert_eq!(kind, outcome(status).unwrap_err().kind(), "{status}");
        }
    }

    #[test]
    fn the_request_wire_format_stays_compatible_across_versions() {
        let hash = "a".repeat(64);
        let target = Target { archive: Some("/tmp/foo.pkg.tar".into()), sha256: Some(hash.clone()), dependency: true, reason: Some(Reason::Explicit), ..Target::named("foo") };
        let request = Request { needed: true, ..request(Operation::Install, vec![target]) };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(format!("{{\"operation\":\"install\",\"targets\":[{{\"name\":\"foo\",\"archive\":\"/tmp/foo.pkg.tar\",\"sha256\":\"{hash}\",\"dependency\":true,\"reason\":\"explicit\"}}],\"recursive\":false,\"nosave\":false,\"needed\":true}}"), json);
        let parsed: Request = serde_json::from_str(&json).unwrap();
        validate(&parsed).unwrap();
        assert_eq!(request, parsed);
        // noconfirm is sent only when set.
        let unattended = Request { noconfirm: true, ..Request::new(Operation::Upgrade) };
        let json = serde_json::to_string(&unattended).unwrap();
        assert_eq!("{\"operation\":\"upgrade\",\"targets\":[],\"recursive\":false,\"nosave\":false,\"needed\":false,\"noconfirm\":true}", json);
        assert_eq!(unattended, serde_json::from_str::<Request>(&json).unwrap());
        // Targets send only what differs from the defaults, so reinstalling a
        // few thousand packages fits in one request.
        assert_eq!("{\"name\":\"zlib\"}", serde_json::to_string(&Target::named("zlib")).unwrap());
        let everything = Request {
            targets: (0..4000).map(|i| Target::named(&format!("package{i:04}"))).collect(),
            ..Request::new(Operation::Install)
        };
        validate(&everything).unwrap();
        assert!(serde_json::to_string(&everything).unwrap().len() <= REQUEST_LIMIT);
        // Absent fields take their defaults; unknown or repeated ones are refused.
        assert_eq!(Request::new(Operation::Upgrade), serde_json::from_str("{\"operation\":\"upgrade\"}").unwrap());
        assert!(serde_json::from_str::<Request>("{\"operation\":\"upgrade\",\"force\":true}").is_err());
        assert!(serde_json::from_str::<Request>("{\"operation\":\"upgrade\",\"needed\":true,\"needed\":false}").is_err());
    }

    #[test]
    fn hashing_rejects_links_and_special_files_and_copies_exactly_what_it_hashed() {
        let tmp = TempDir::new();
        let at = |name: &str| tmp.path().join(name).to_string_lossy().into_owned();
        std::fs::write(at("pkg"), "package bytes").unwrap();
        let hash = hash_file(&at("pkg")).unwrap();
        assert_eq!(64, hash.len());
        std::os::unix::fs::symlink("pkg", at("link")).unwrap();
        let refused = |path: &str| hash_file(path).unwrap_err().kind() == Kind::UnsafeArchive;
        assert!(refused(&at("link")));
        std::fs::write(at("pkg"), "different bytes").unwrap();
        let copied = copy_hash(&at("pkg"), Some(&at("snapshot")), 1024, false).unwrap();
        std::fs::write(at("pkg"), "changed again").unwrap();
        assert_eq!(copied, hash_file(&at("snapshot")).unwrap());
        assert_ne!(hash, hash_file(&at("pkg")).unwrap());
        assert_eq!(0, unsafe { libc::mkfifo(util::cstr(&at("fifo")).as_ptr(), 0o600) });
        assert!(refused(&at("fifo")));
        std::fs::write(at("empty"), "").unwrap();
        assert!(refused(&at("empty")));
        hash_source_file(&at("empty")).unwrap();
    }
}
