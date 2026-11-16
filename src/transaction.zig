//! System changes. The unprivileged process validates a request and hands it
//! to the root worker through run0 or sudo; the worker validates it again, stages
//! archives privately, and commits one libalpm transaction after a final
//! confirmation.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const signals = @import("signals.zig");
const errors = @import("errors.zig");
const cli = @import("cli.zig");
const Reason = cli.Reason;

// The request crosses the privilege boundary as JSON. Its shape is a
// compatibility contract: during an upgrade of zap itself, a running older
// process talks to the newly installed worker.
pub const Operation = enum { install, remove, upgrade, reason };
pub const Target = struct {
    name: []const u8,
    archive: ?[]const u8 = null,
    sha256: ?[]const u8 = null,
    dependency: bool = false,
    reason: ?Reason = null,
};
pub const Request = struct {
    operation: Operation,
    targets: []const Target = &.{},
    recursive: bool = false,
    nosave: bool = false,
    needed: bool = false,
};

const worker_path = "/usr/bin/zap";
const request_limit = 96 * 1024;
const target_limit = 1024;
const archive_limit = 4 * 1024 * 1024 * 1024;
const source_limit = 16 * 1024 * 1024;
const signature_limit = 64 * 1024;

// ── Unprivileged side ───────────────────────────────────────────────────────

/// The elevation tool chosen with --elevate; otherwise $ZAP_ELEVATE decides,
/// and then what the system provides.
var requested_elevator: ?cli.Elevator = null;

pub fn setElevator(choice: ?cli.Elevator) void {
    requested_elevator = choice;
}

fn present(path: []const u8) bool {
    std.fs.cwd().access(path, .{}) catch return false;
    return true;
}

/// The tool chosen with --elevate or ZAP_ELEVATE, if any. An unknown
/// ZAP_ELEVATE value is refused.
pub fn chosenElevator() !?cli.Elevator {
    if (requested_elevator) |choice| return choice;
    const value = std.posix.getenv("ZAP_ELEVATE") orelse return null;
    if (value.len == 0) return null;
    return std.meta.stringToEnum(cli.Elevator, value) orelse errors.with(error.InvalidElevator, "ZAP_ELEVATE={s}", .{value});
}

/// run0 needs systemd as the running service manager and polkit to
/// authenticate; sudo is the fallback.
pub fn elevator() !cli.Elevator {
    if (try chosenElevator()) |choice| return choice;
    if (present("/run/systemd/system") and present("/usr/lib/polkit-1/polkitd") and present(elevatorPath(.run0))) return .run0;
    if (present(elevatorPath(.sudo))) return .sudo;
    return error.ElevatorUnavailable;
}

fn elevatorPath(tool: cli.Elevator) []const u8 {
    return switch (tool) {
        .run0 => "/usr/bin/run0",
        .sudo => "/usr/bin/sudo",
    };
}

/// Everything a system change needs, checked before any work that would be
/// wasted without it: the trusted worker, a trusted elevation tool, and a
/// terminal for the confirmations. Returns the tool.
pub fn preflight() !cli.Elevator {
    try trustedExecutable();
    const tool = try trustedElevator();
    if (c.isatty(0) == 0) return error.InteractiveTerminalRequired;
    return tool;
}

/// The chosen elevation tool, refused unless root controls it.
pub fn trustedElevator() !cli.Elevator {
    const tool = try elevator();
    try trustedTool(elevatorPath(tool));
    return tool;
}

/// Run a request in the root worker. The worker shows the final transaction
/// and asks for confirmation itself.
pub fn escalate(request: Request) !void {
    try validate(request);
    const tool = try preflight();
    const json = try std.json.Stringify.valueAlloc(u.a, request, .{});
    if (json.len > request_limit) return error.TransactionTooLarge;
    ui.notef(.info, "Asking {s} for administrator access to {s}.", .{ @tagName(tool), switch (request.operation) {
        .install => "install packages",
        .remove => "remove packages",
        .upgrade => "upgrade the system",
        .reason => "change installation reasons",
    } });
    // Ctrl-C belongs to the worker while it runs: this process stays to
    // report how the worker ended instead of returning the terminal early.
    signals.deferInterrupts();
    defer signals.restore();
    try outcome(try u.runStatus(try elevatedArgs(tool, json, ui.styled()), "/"));
}

/// The worker command line for each tool. run0 starts the worker with a clean
/// environment, so the one presentation preference is carried as a constant;
/// sudo's environment policy may refuse that, so under sudo the worker styles
/// output by what its terminal supports.
fn elevatedArgs(tool: cli.Elevator, json: []const u8, styled: bool) ![]const []const u8 {
    var args: std.ArrayList([]const u8) = .empty;
    switch (tool) {
        .run0 => {
            try args.appendSlice(u.a, &.{ elevatorPath(.run0), "--pty", "--background=", "--chdir=/" });
            if (!styled) try args.append(u.a, "--setenv=NO_COLOR=1");
        },
        .sudo => try args.append(u.a, elevatorPath(.sudo)),
    }
    try args.appendSlice(u.a, &.{ "--", worker_path, "__transaction", json });
    return args.toOwnedSlice(u.a);
}

/// The worker reports its own failures, and run0 or sudo theirs; only the
/// outcome travels back, as the exit status.
fn outcome(status: u8) !void {
    return switch (status) {
        0 => {},
        errors.exit_cancelled => error.WorkerCancelled,
        // Killed or crashed: nothing else has said so. runStatus gives
        // 128 + N for signal N and 255 for any other abnormal end.
        129, 131...255 => if (status == 255 or status > 128 + 31)
            errors.with(error.WorkerInterrupted, "the worker ended with status {d}", .{status})
        else
            errors.with(error.WorkerInterrupted, "the worker ended by signal {d}", .{status - 128}),
        else => error.WorkerFailed,
    };
}

/// The elevation tool must be root's: its directory and the file it resolves
/// to are owned by root and writable by nobody else. run0 is a link to
/// systemd-run, so the link itself is followed.
fn trustedTool(path: []const u8) !void {
    for ([_][]const u8{ "/", "/usr", "/usr/bin" }) |dir| {
        var st: c.struct_stat = undefined;
        if (c.lstat((try u.z(dir)).ptr, &st) != 0 or st.st_uid != 0 or st.st_mode & 0o022 != 0 or st.st_mode & c.S_IFMT != c.S_IFDIR) {
            return errors.with(error.UntrustedWorkerPath, "{s}", .{dir});
        }
    }
    var st: c.struct_stat = undefined;
    if (c.lstat((try u.z(path)).ptr, &st) != 0) return errors.with(error.ElevatorUnavailable, "{s}", .{path});
    if (st.st_uid != 0) return errors.with(error.UntrustedWorkerPath, "{s}", .{path});
    if (c.stat((try u.z(path)).ptr, &st) != 0 or st.st_uid != 0 or st.st_mode & 0o022 != 0 or st.st_mode & c.S_IFMT != c.S_IFREG) {
        return errors.with(error.UntrustedWorkerPath, "{s}", .{path});
    }
}

/// Only a root-controlled executable may be elevated: every component of the
/// worker's path must be owned by root, not writable by others, and not a link.
pub fn trustedExecutable() !void {
    for ([_][]const u8{ "/", "/usr", "/usr/bin", worker_path }) |path| {
        var st: c.struct_stat = undefined;
        if (c.lstat((try u.z(path)).ptr, &st) != 0) return error.WorkerNotInstalled;
        if (st.st_uid != 0 or st.st_mode & 0o022 != 0 or st.st_mode & c.S_IFMT == c.S_IFLNK) {
            return errors.with(error.UntrustedWorkerPath, "{s}", .{path});
        }
    }
}

/// Structural validation, applied on both sides of the privilege boundary.
pub fn validate(request: Request) !void {
    if (request.operation != .remove and (request.recursive or request.nosave)) return error.InvalidTransactionFlags;
    if (request.needed and request.operation != .install) return error.InvalidTransactionFlags;
    if (request.targets.len > target_limit) return error.TransactionTooLarge;
    for (request.targets, 0..) |target, i| {
        if (target.reason != null and request.operation != .install and request.operation != .reason) return error.InvalidTransactionFlags;
        if (request.operation == .reason and target.reason == null) return error.InstallationReasonRequired;
        if (!u.validName(target.name)) return error.InvalidPackageName;
        for (request.targets[0..i]) |previous| if (std.mem.eql(u8, target.name, previous.name)) return error.DuplicateTarget;
        if (target.archive) |path| {
            if (request.operation != .install or !std.fs.path.isAbsolute(path)) return error.InvalidArchive;
            const hash = target.sha256 orelse return error.MissingDigest;
            if (hash.len != 64) return error.InvalidDigest;
            for (hash) |ch| if (!std.ascii.isHex(ch)) return error.InvalidDigest;
        } else if (target.sha256 != null) return error.InvalidDigest;
    }
    if (request.operation != .upgrade and request.targets.len == 0) return error.NoTargets;
}

// ── Hashing ─────────────────────────────────────────────────────────────────

/// SHA-256 of a package archive.
pub fn hashFile(path: []const u8) ![]const u8 {
    return copyHash(path, null, archive_limit, false);
}

/// SHA-256 of a tracked build file, which may be empty.
pub fn hashSourceFile(path: []const u8) ![]const u8 {
    return copyHash(path, null, source_limit, true);
}

/// Hash a regular file read through one descriptor opened without following
/// links, optionally writing the same bytes to a new private file. The copy
/// therefore has exactly the content that was hashed.
fn copyHash(path: []const u8, dest: ?[]const u8, limit: u64, allow_empty: bool) ![]const u8 {
    const fd = c.open((try u.z(path)).ptr, @as(c_int, c.O_RDONLY | c.O_CLOEXEC | c.O_NOFOLLOW | c.O_NONBLOCK));
    if (fd < 0) return errors.with(error.UnsafeArchive, "{s}", .{path});
    defer _ = c.close(fd);
    var st: c.struct_stat = undefined;
    if (c.fstat(fd, &st) != 0 or st.st_mode & c.S_IFMT != c.S_IFREG or st.st_size < 0 or (!allow_empty and st.st_size == 0) or st.st_size > limit) {
        return errors.with(error.UnsafeArchive, "{s}", .{path});
    }
    var out: ?std.fs.File = null;
    if (dest) |target| out = try std.fs.cwd().createFile(target, .{ .exclusive = true, .mode = 0o600 });
    defer if (out) |file| file.close();
    var hash = std.crypto.hash.sha2.Sha256.init(.{});
    var buf: [64 * 1024]u8 = undefined;
    var total: u64 = 0;
    while (true) {
        const n = try std.posix.read(fd, &buf);
        if (n == 0) break;
        try signals.check();
        total += n;
        if (total > limit) return errors.with(error.ArchiveTooLarge, "{s}", .{path});
        hash.update(buf[0..n]);
        if (out) |file| try file.writeAll(buf[0..n]);
    }
    if (total != st.st_size) return errors.with(error.ArchiveChanged, "{s}", .{path});
    const digest = hash.finalResult();
    return std.fmt.allocPrint(u.a, "{s}", .{std.fmt.bytesToHex(digest, .lower)});
}

// ── Root worker ─────────────────────────────────────────────────────────────

pub fn worker(bytes: []const u8) !void {
    if (c.geteuid() != 0 or c.getuid() != 0) return error.RootWorkerRequired;
    if (c.isatty(0) == 0) return error.InteractiveTerminalRequired;
    if (bytes.len > request_limit) return error.TransactionTooLarge;
    // Unknown fields are refused: a request this worker does not fully
    // understand is never partially applied.
    const parsed = std.json.parseFromSlice(Request, u.a, bytes, .{}) catch |err| {
        return if (err == error.OutOfMemory) err else error.InvalidTransactionRequest;
    };
    const request = parsed.value;
    signals.install();
    try validate(request);
    var db = try alpm.Alpm.init();
    defer db.deinit();

    const template = try u.a.dupeZ(u8, "/var/tmp/zap-transaction-XXXXXX");
    if (c.mkdtemp(template.ptr) == null) return error.StagingFailed;
    const staging = std.mem.sliceTo(template, 0);
    defer std.fs.cwd().deleteTree(staging) catch {};
    const archives = try stage(request, staging);
    try signals.check();

    if (request.operation == .upgrade) try db.refresh();
    try signals.check();
    // Take the normal ALPM lock. Another process's lock is never removed.
    try db.check(c.alpm_trans_init(db.h, flags(request)));
    defer _ = c.alpm_trans_release(db.h);

    if (request.operation == .reason) {
        for (request.targets) |target| {
            if (try db.installedPkg(target.name) == null) return errors.with(error.PackageNotInstalled, "{s}", .{target.name});
        }
        return applyReasons(&db, request.targets, null);
    }

    // Reasons are decided against the state before this transaction.
    var was_explicit: std.StringHashMap(void) = .init(u.a);
    var local_it = db.installed();
    while (local_it != null) : (local_it = local_it.*.next) {
        const p = alpm.pkg(local_it.*.data);
        if (alpm.explicit(p)) try was_explicit.put(try u.a.dupe(u8, alpm.name(p)), {});
    }

    if (request.operation == .upgrade) try db.check(c.alpm_sync_sysupgrade(db.h, 0));
    try queue(&db, request, archives);
    var data: alpm.List = null;
    if (c.alpm_trans_prepare(db.h, &data) < 0) {
        db.explain(data);
        try db.check(-1);
    }
    try signals.check();

    const additions = c.alpm_trans_get_add(db.h);
    const removals = c.alpm_trans_get_remove(db.h);
    var reason_changes: std.ArrayList(Target) = .empty;
    if (request.operation == .install) for (request.targets) |target| {
        const p = try db.installedPkg(target.name) orelse continue;
        if (c.alpm_pkg_get_reason(p) != installReason(target, was_explicit.contains(target.name))) try reason_changes.append(u.a, target);
    };
    if (additions == null and removals == null) {
        if (reason_changes.items.len == 0) return ui.note(.success, "Nothing to do: everything is up to date.");
        return applyReasons(&db, reason_changes.items, &was_explicit);
    }

    try protectHeld(&db, removals);
    summarize(&db, additions, removals, reason_changes.items, &was_explicit);
    try ui.require("Commit this transaction, including package scripts and hooks? [y/N]");
    try signals.check();
    data = null;
    if (c.alpm_trans_commit(db.h, &data) < 0) {
        db.explain(data);
        try db.check(-1);
    }
    if (request.operation != .remove) for (request.targets) |target| {
        const p = try db.installedPkg(target.name) orelse continue;
        try db.check(c.alpm_pkg_set_reason(p, installReason(target, was_explicit.contains(target.name))));
    };
    ui.blank();
    ui.note(.success, "Transaction complete.");
}

/// Copy every requested archive, and its detached signature if one exists,
/// into root-only staging, and check each copy against the approved digest.
/// The transaction only ever reads the copies.
fn stage(request: Request, staging: []const u8) ![]const ?[]const u8 {
    const archives = try u.a.alloc(?[]const u8, request.targets.len);
    for (request.targets, archives, 0..) |target, *slot, i| {
        slot.* = null;
        const path = target.archive orelse continue;
        const snapshot = try std.fmt.allocPrint(u.a, "{s}/{d}.pkg.tar", .{ staging, i });
        const actual = try copyHash(path, snapshot, archive_limit, false);
        if (!std.ascii.eqlIgnoreCase(actual, target.sha256.?)) return errors.with(error.ArchiveDigestMismatch, "{s}", .{path});
        const signature = try std.fmt.allocPrint(u.a, "{s}.sig", .{path});
        var st: c.struct_stat = undefined;
        if (c.lstat((try u.z(signature)).ptr, &st) == 0) {
            _ = try copyHash(signature, try std.fmt.allocPrint(u.a, "{s}.sig", .{snapshot}), signature_limit, false);
        }
        slot.* = snapshot;
    }
    return archives;
}

/// Add every target to the open transaction.
fn queue(db: *alpm.Alpm, request: Request, archives: []const ?[]const u8) !void {
    for (request.targets, archives) |target, staged| {
        if (request.operation == .remove) {
            const p = try db.installedPkg(target.name) orelse return errors.with(error.PackageNotInstalled, "{s}", .{target.name});
            try db.check(c.alpm_remove_pkg(db.h, p));
            continue;
        }
        var p: ?*c.alpm_pkg_t = null;
        if (staged) |path| {
            // Staged archives are held to the configured local signature policy.
            try db.check(c.alpm_pkg_load(db.h, (try u.z(path)).ptr, 1, c.alpm_option_get_local_file_siglevel(db.h), &p));
            if (!std.mem.eql(u8, alpm.name(p.?), target.name)) {
                _ = c.alpm_pkg_free(p);
                return errors.with(error.ArchiveNameMismatch, "{s}", .{target.name});
            }
        } else p = try db.repo(target.name) orelse return errors.with(error.RepositoryPackageNotFound, "{s}", .{target.name});
        const result = c.alpm_add_pkg(db.h, p.?);
        if (result < 0) {
            // libalpm owns a loaded package only once it is in the transaction.
            if (staged != null) _ = c.alpm_pkg_free(p);
            try db.check(result);
        }
    }
}

/// The HoldPkg pattern that protects a package, if any.
pub fn held(db: *const alpm.Alpm, name: []const u8) !?[]const u8 {
    for (db.cfg.hold.items) |pattern| {
        if (c.fnmatch((try u.z(pattern)).ptr, (try u.z(name)).ptr, 0) == 0) return pattern;
    }
    return null;
}

/// HoldPkg entries are never removed, whatever pulled them into the transaction.
fn protectHeld(db: *alpm.Alpm, removals: alpm.List) !void {
    var it = removals;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (try held(db, alpm.name(p))) |pattern| return errors.with(error.ProtectedPackage, "{s} (HoldPkg {s})", .{ alpm.name(p), pattern });
    }
}

/// Show and, once confirmed, apply reason changes that need no transaction.
/// `was_explicit` is null when every target carries its own reason.
fn applyReasons(db: *alpm.Alpm, targets: []const Target, was_explicit: ?*const std.StringHashMap(void)) !void {
    ui.heading("Installation reasons", ui.count(targets.len, "change", "changes"));
    showReasons(db, targets, was_explicit);
    ui.blank();
    try ui.require("Apply these installation reasons? [y/N]");
    try signals.check();
    for (targets) |target| {
        const p = try db.installedPkg(target.name) orelse return errors.with(error.PackageNotInstalled, "{s}", .{target.name});
        try db.check(c.alpm_pkg_set_reason(p, installReason(target, if (was_explicit) |set| set.contains(target.name) else false)));
    }
    ui.note(.success, "Installation reasons updated.");
}

fn showReasons(db: *alpm.Alpm, targets: []const Target, was_explicit: ?*const std.StringHashMap(void)) void {
    var table = ui.Table.init(&.{ .{ .title = "Package", .tone = .bold }, .{ .title = "Current" }, .{ .title = "New", .tone = .accent, .joiner = " → " } });
    for (targets) |target| {
        const current = if (db.installedPkg(target.name) catch null) |p| alpm.reason(p) else "—";
        const wanted = installReason(target, if (was_explicit) |set| set.contains(target.name) else false);
        table.addText(&.{ target.name, current, if (wanted == c.ALPM_PKG_REASON_EXPLICIT) "explicit" else "dependency" });
    }
    table.render();
}

const Change = enum { install, upgrade, downgrade, reinstall, remove };

fn change(p: alpm.Pkg, old: ?alpm.Pkg) Change {
    const previous = old orelse return .install;
    const cmp = c.alpm_pkg_vercmp(c.alpm_pkg_get_version(p), c.alpm_pkg_get_version(previous));
    return if (cmp > 0) .upgrade else if (cmp < 0) .downgrade else .reinstall;
}

/// The final review: exactly what libalpm resolved, before anything changes.
fn summarize(db: *alpm.Alpm, additions: alpm.List, removals: alpm.List, reason_changes: []const Target, was_explicit: *const std.StringHashMap(void)) void {
    var counts = std.EnumArray(Change, usize).initFill(0);
    var net: i64 = 0;
    var download: i64 = 0;
    var table = ui.Table.init(&.{
        .{ .title = "Package", .tone = .bold },
        .{ .title = "Current" },
        .{ .title = "New", .joiner = " → " },
        .{ .title = "Change" },
        .{ .title = "Size", .tone = .muted, .alignment = .right },
    });
    var it = additions;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        const old: ?alpm.Pkg = c.alpm_db_get_pkg(db.localDb(), c.alpm_pkg_get_name(p));
        const kind = change(p, old);
        const delta = c.alpm_pkg_get_isize(p) - (if (old) |previous| c.alpm_pkg_get_isize(previous) else 0);
        counts.set(kind, counts.get(kind) + 1);
        net += delta;
        download += c.alpm_pkg_download_size(p);
        table.add(&.{
            .{ .text = alpm.name(p) },
            .{ .text = if (old) |previous| alpm.version(previous) else "—" },
            .{ .text = alpm.version(p), .tone = .success },
            .{ .text = @tagName(kind), .tone = if (kind == .downgrade) .warning else .accent },
            .{ .text = ui.sizeDelta(delta) },
        });
    }
    it = removals;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        counts.set(.remove, counts.get(.remove) + 1);
        net -= c.alpm_pkg_get_isize(p);
        table.add(&.{
            .{ .text = alpm.name(p) },
            .{ .text = alpm.version(p) },
            .{ .text = "—", .tone = .muted },
            .{ .text = "remove", .tone = .danger },
            .{ .text = ui.sizeDelta(-c.alpm_pkg_get_isize(p)) },
        });
    }

    var parts: std.ArrayList([]const u8) = .empty;
    inline for (.{ Change.install, Change.upgrade, Change.downgrade, Change.reinstall, Change.remove }) |kind| {
        if (counts.get(kind) > 0) parts.append(u.a, std.fmt.allocPrint(u.a, "{d} to {s}", .{ counts.get(kind), @tagName(kind) }) catch "") catch {};
    }
    ui.heading("Transaction", std.mem.join(u.a, " · ", parts.items) catch "");
    table.render();
    ui.blank();
    if (download > 0) ui.field("Download", ui.size(download));
    ui.field("Disk space", ui.sizeDelta(net));
    if (reason_changes.len > 0) {
        ui.heading("Installation reasons", ui.count(reason_changes.len, "change", "changes"));
        showReasons(db, reason_changes, was_explicit);
    }
    ui.blank();
}

pub fn flags(request: Request) c_int {
    var result: c_int = 0;
    if (request.needed) result |= c.ALPM_TRANS_FLAG_NEEDED;
    if (request.operation == .remove and request.recursive) result |= c.ALPM_TRANS_FLAG_RECURSE;
    if (request.nosave) result |= c.ALPM_TRANS_FLAG_NOSAVE;
    return result;
}

/// The reason a target should have after installation: an explicit request
/// wins; otherwise a package that was explicit stays explicit.
pub fn installReason(target: Target, was_explicit: bool) c.alpm_pkgreason_t {
    if (target.reason) |reason| return if (reason == .explicit) c.ALPM_PKG_REASON_EXPLICIT else c.ALPM_PKG_REASON_DEPEND;
    return if (!target.dependency or was_explicit) c.ALPM_PKG_REASON_EXPLICIT else c.ALPM_PKG_REASON_DEPEND;
}

// ── Tests ───────────────────────────────────────────────────────────────────

test "reason overrides are explicit and worker flags cannot weaken unrelated operations" {
    try std.testing.expectEqual(@as(c.alpm_pkgreason_t, c.ALPM_PKG_REASON_DEPEND), installReason(.{ .name = "foo", .reason = .dependency }, true));
    try std.testing.expectEqual(@as(c.alpm_pkgreason_t, c.ALPM_PKG_REASON_EXPLICIT), installReason(.{ .name = "foo", .dependency = true }, true));
    try std.testing.expectError(error.InvalidTransactionFlags, validate(.{ .operation = .remove, .needed = true, .targets = &.{.{ .name = "foo" }} }));
    try std.testing.expectError(error.InstallationReasonRequired, validate(.{ .operation = .reason, .targets = &.{.{ .name = "foo" }} }));
    try std.testing.expectError(error.InvalidTransactionFlags, validate(.{ .operation = .upgrade, .targets = &.{.{ .name = "foo", .reason = .dependency }} }));
    try validate(.{ .operation = .reason, .targets = &.{.{ .name = "foo", .reason = .dependency }} });
    try std.testing.expectEqual(@as(c_int, c.ALPM_TRANS_FLAG_RECURSE | c.ALPM_TRANS_FLAG_NOSAVE), flags(.{ .operation = .remove, .recursive = true, .nosave = true }));
    try std.testing.expectEqual(@as(c_int, c.ALPM_TRANS_FLAG_NEEDED), flags(.{ .operation = .install, .needed = true }));
    try std.testing.expectEqual(@as(c_int, 0), flags(.{ .operation = .upgrade }));
}

test "each elevation tool runs the fixed worker, and only run0 carries the color preference" {
    const json = "{\"operation\":\"upgrade\"}";
    const run0 = try std.mem.join(u.a, " ", try elevatedArgs(.run0, json, false));
    try std.testing.expectEqualStrings("/usr/bin/run0 --pty --background= --chdir=/ --setenv=NO_COLOR=1 -- /usr/bin/zap __transaction " ++ json, run0);
    const styled = try std.mem.join(u.a, " ", try elevatedArgs(.run0, json, true));
    try std.testing.expect(std.mem.indexOf(u8, styled, "NO_COLOR") == null);
    const sudo = try std.mem.join(u.a, " ", try elevatedArgs(.sudo, json, false));
    try std.testing.expectEqualStrings("/usr/bin/sudo -- /usr/bin/zap __transaction " ++ json, sudo);
    // An explicit choice wins over the environment and the system.
    setElevator(.sudo);
    defer setElevator(null);
    try std.testing.expectEqual(cli.Elevator.sudo, try elevator());
}

test "elevation tools must belong to root" {
    try std.testing.expectError(error.ElevatorUnavailable, trustedTool("/usr/bin/no-such-elevator"));
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "fake-sudo", .data = "" });
    const fake = try tmp.dir.realpathAlloc(u.a, "fake-sudo");
    // A file the user owns is refused whatever its location.
    try std.testing.expectError(error.UntrustedWorkerPath, trustedTool(fake));
}

test "the worker's exit status becomes the outcome without a second report" {
    try outcome(0);
    try std.testing.expectError(error.WorkerCancelled, outcome(130));
    try std.testing.expectError(error.WorkerFailed, outcome(1));
    try std.testing.expectError(error.WorkerFailed, outcome(2));
    try std.testing.expectError(error.WorkerInterrupted, outcome(137));
    try std.testing.expectError(error.WorkerInterrupted, outcome(255));
}

test "worker rejects unsafe manifests" {
    try std.testing.expectError(error.InvalidPackageName, validate(.{ .operation = .install, .targets = &.{.{ .name = "../bad" }} }));
    try std.testing.expectError(error.InvalidDigest, validate(.{ .operation = .install, .targets = &.{.{ .name = "foo", .archive = "/tmp/foo.pkg.tar.zst", .sha256 = "short" }} }));
    try std.testing.expectError(error.DuplicateTarget, validate(.{ .operation = .remove, .targets = &.{ .{ .name = "foo" }, .{ .name = "foo" } } }));
    try std.testing.expectError(error.InvalidArchive, validate(.{ .operation = .install, .targets = &.{.{ .name = "foo", .archive = "relative.pkg.tar", .sha256 = "0" ** 64 }} }));
    try std.testing.expectError(error.InvalidArchive, validate(.{ .operation = .remove, .targets = &.{.{ .name = "foo", .archive = "/tmp/foo.pkg.tar", .sha256 = "0" ** 64 }} }));
    try std.testing.expectError(error.MissingDigest, validate(.{ .operation = .install, .targets = &.{.{ .name = "foo", .archive = "/tmp/foo.pkg.tar" }} }));
    try std.testing.expectError(error.NoTargets, validate(.{ .operation = .install }));
    try validate(.{ .operation = .upgrade });
}

test "the request wire format stays compatible across versions" {
    const request: Request = .{ .operation = .install, .needed = true, .targets = &.{.{ .name = "foo", .archive = "/tmp/foo.pkg.tar", .sha256 = "a" ** 64, .dependency = true, .reason = .explicit }} };
    const json = try std.json.Stringify.valueAlloc(u.a, request, .{});
    try std.testing.expectEqualStrings("{\"operation\":\"install\",\"targets\":[{\"name\":\"foo\",\"archive\":\"/tmp/foo.pkg.tar\",\"sha256\":\"" ++ "a" ** 64 ++ "\",\"dependency\":true,\"reason\":\"explicit\"}],\"recursive\":false,\"nosave\":false,\"needed\":true}", json);
    const parsed = try std.json.parseFromSlice(Request, u.a, json, .{});
    try validate(parsed.value);
    try std.testing.expectEqualStrings("foo", parsed.value.targets[0].name);
    // Unknown fields are refused rather than ignored.
    try std.testing.expectError(error.UnknownField, std.json.parseFromSlice(Request, u.a, "{\"operation\":\"upgrade\",\"force\":true}", .{}));
}

test "snapshot hash rejects symlinks and detects changed content" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "pkg", .data = "package bytes" });
    const path = try tmp.dir.realpathAlloc(u.a, "pkg");
    const dir = std.fs.path.dirname(path).?;
    const hash = try hashFile(path);
    try std.testing.expectEqual(@as(usize, 64), hash.len);
    try tmp.dir.symLink("pkg", "link", .{});
    try std.testing.expectError(error.UnsafeArchive, hashFile(try std.fs.path.join(u.a, &.{ dir, "link" })));
    try tmp.dir.writeFile(.{ .sub_path = "pkg", .data = "different bytes" });
    const snapshot = try std.fs.path.join(u.a, &.{ dir, "snapshot" });
    const snapshot_hash = try copyHash(path, snapshot, 1024, false);
    try tmp.dir.writeFile(.{ .sub_path = "pkg", .data = "changed again" });
    try std.testing.expectEqualStrings(snapshot_hash, try hashFile(snapshot));
    const fifo = try std.fs.path.join(u.a, &.{ dir, "fifo" });
    try std.testing.expectEqual(@as(c_int, 0), c.mkfifo((try u.z(fifo)).ptr, 0o600));
    try std.testing.expectError(error.UnsafeArchive, hashFile(fifo));
    try std.testing.expect(!std.mem.eql(u8, try hashFile(path), hash));
    try tmp.dir.writeFile(.{ .sub_path = "empty", .data = "" });
    try std.testing.expectError(error.UnsafeArchive, hashFile(try std.fs.path.join(u.a, &.{ dir, "empty" })));
    _ = try hashSourceFile(try std.fs.path.join(u.a, &.{ dir, "empty" }));
}

test "the final review shows what libalpm resolved, and held packages cannot be removed" {
    _ = c.setlocale(c.LC_CTYPE, "C.UTF-8");
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    try native.installedFixture(tmp.dir, "held-package", "");
    try native.installedFixture(tmp.dir, "upgraded", "");
    const added = try native.fixture(tmp.dir, root, "added", "");
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    try db.cfg.hold.append(u.a, "held-*");

    // An installation: one new package, shown with its size.
    try db.check(c.alpm_trans_init(db.h, 0));
    try queue(&db, .{ .operation = .install, .targets = &.{.{ .name = "added", .archive = added, .sha256 = "0" ** 64 }} }, &.{added});
    var data: alpm.List = null;
    try db.check(c.alpm_trans_prepare(db.h, &data));
    var was_explicit: std.StringHashMap(void) = .init(u.a);
    defer ui.setCaps(null);
    ui.setCaps(.{ .width = 80 });
    ui.beginCapture();
    summarize(&db, c.alpm_trans_get_add(db.h), c.alpm_trans_get_remove(db.h), &.{}, &was_explicit);
    try std.testing.expectEqualStrings(
        \\
        \\▌ Transaction  1 to install
        \\  Package  Current  New    Change   Size
        \\  added    —        1.0-1  install  +1 B
        \\
        \\  Disk space          +1 B
        \\
        \\
    , ui.endCapture());
    try protectHeld(&db, c.alpm_trans_get_remove(db.h));
    try db.check(c.alpm_trans_release(db.h));

    // A removal that includes a HoldPkg match is refused before any prompt.
    try db.check(c.alpm_trans_init(db.h, 0));
    defer _ = c.alpm_trans_release(db.h);
    try queue(&db, .{ .operation = .remove, .targets = &.{ .{ .name = "upgraded" }, .{ .name = "held-package" } } }, &.{ null, null });
    try std.testing.expectError(error.PackageNotInstalled, queue(&db, .{ .operation = .remove, .targets = &.{.{ .name = "absent" }} }, &.{null}));
    data = null;
    try db.check(c.alpm_trans_prepare(db.h, &data));
    try std.testing.expectError(error.ProtectedPackage, protectHeld(&db, c.alpm_trans_get_remove(db.h)));
    ui.beginCapture();
    summarize(&db, c.alpm_trans_get_add(db.h), c.alpm_trans_get_remove(db.h), &.{}, &was_explicit);
    const removal = ui.endCapture();
    try std.testing.expect(std.mem.indexOf(u8, removal, "Transaction  2 to remove") != null);
    try std.testing.expect(std.mem.indexOf(u8, removal, "held-package  1.0-1    —    remove  -1 B") != null);
}

test "an archive whose metadata names another package is refused" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    const archive = try native.fixture(tmp.dir, root, "actual", "");
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    try db.check(c.alpm_trans_init(db.h, 0));
    defer _ = c.alpm_trans_release(db.h);
    try std.testing.expectError(error.ArchiveNameMismatch, queue(&db, .{ .operation = .install, .targets = &.{.{ .name = "claimed", .archive = archive, .sha256 = "0" ** 64 }} }, &.{archive}));
    try std.testing.expectEqual(@as(usize, 0), c.alpm_list_count(c.alpm_trans_get_add(db.h)));
}

test "staging copies archives and signatures, and refuses a digest that no longer matches" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    try tmp.dir.makeDir("staging");
    try tmp.dir.writeFile(.{ .sub_path = "a.pkg.tar", .data = "archive" });
    try tmp.dir.writeFile(.{ .sub_path = "a.pkg.tar.sig", .data = "signature" });
    const archive = try std.fs.path.join(u.a, &.{ root, "a.pkg.tar" });
    const staging = try std.fs.path.join(u.a, &.{ root, "staging" });
    const digest = try hashFile(archive);
    const request: Request = .{ .operation = .install, .targets = &.{ .{ .name = "repo-package" }, .{ .name = "a", .archive = archive, .sha256 = digest } } };
    const staged = try stage(request, staging);
    try std.testing.expect(staged[0] == null);
    try std.testing.expectEqualStrings(digest, try hashFile(staged[1].?));
    try std.testing.expectEqualStrings("signature", try tmp.dir.readFileAlloc(u.a, "staging/1.pkg.tar.sig", 64));
    // The archive changes between approval and staging.
    try tmp.dir.writeFile(.{ .sub_path = "a.pkg.tar", .data = "tampered" });
    try tmp.dir.makeDir("again");
    try std.testing.expectError(error.ArchiveDigestMismatch, stage(request, try std.fs.path.join(u.a, &.{ root, "again" })));
}
