const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const tx = @import("transaction.zig");
const builder = @import("builder.zig");

pub fn main() void {
    var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena.deinit();
    u.a = arena.allocator();
    dispatch() catch |err| {
        ui.print("\n{s}zap: {s}{s}\n", .{ ui.color("\x1b[31m"), @errorName(err), ui.color("\x1b[0m") });
        switch (err) {
            error.InteractiveTerminalRequired => ui.print("Run system changes from an interactive terminal; every transaction requires confirmation.\n", .{}),
            error.BuildsMustNotRunAsRoot => ui.print("Run zap as your regular user. Only the internal transaction worker runs as root through run0.\n", .{}),
            error.AlpmOperationFailed => ui.print("The transaction was not completed. Inspect the libalpm diagnostic above; never remove an active database lock.\n", .{}),
            error.UnsupportedPacmanConfiguration => ui.print("XferCommand and AssumeInstalled are unsupported; zap refuses to silently ignore them.\n", .{}),
            error.Cancelled => ui.print("Operation cancelled.\n", .{}),
            else => {},
        }
        std.process.exit(if (err == error.Cancelled) 130 else 1);
    };
}
fn dispatch() !void {
    const args = try std.process.argsAlloc(u.a);
    if (args.len >= 2 and std.mem.eql(u8, args[1], "__transaction")) {
        if (args.len != 3) return error.InvalidWorkerArguments;
        return tx.worker(args[2]);
    }
    if (args.len == 1 or eq(args[1], "help", "--help") or std.mem.eql(u8, args[1], "-h")) {
        help();
        return;
    }
    if (eq(args[1], "version", "--version")) {
        ui.print("zap 0.1.0 · Zig 0.15.2 · libalpm {s}\n", .{u.str(c.alpm_version())});
        return;
    }
    if (c.geteuid() == 0) return error.BuildsMustNotRunAsRoot;
    const command = args[1];
    var dry = false;
    var devel = false;
    var recursive = false;
    var nosave = false;
    var names: std.ArrayList([]const u8) = .empty;
    for (args[2..]) |arg| {
        if (std.mem.eql(u8, arg, "--dry-run")) dry = true else if (std.mem.eql(u8, arg, "--devel")) devel = true else if (std.mem.eql(u8, arg, "--recursive")) recursive = true else if (std.mem.eql(u8, arg, "--nosave")) nosave = true else if (std.mem.startsWith(u8, arg, "-")) return error.UnknownOption else try names.append(u.a, arg);
    }
    if (devel and !eq(command, "updates", "-Qu") and !eq(command, "upgrade", "-Syu")) return error.OptionNotValidForCommand;
    if ((recursive or nosave) and !eq(command, "remove", "-R") and !std.mem.eql(u8, command, "-Rns")) return error.OptionNotValidForCommand;
    var db = try alpm.Alpm.init();
    defer db.deinit();
    if (eq(command, "search", "-Ss")) {
        if (names.items.len == 0) return error.SearchTermRequired;
        const term = try std.mem.join(u.a, " ", names.items);
        ui.title("Repository packages · updated date is build date");
        try db.search(term);
        ui.title("AUR packages · updated date is last modification");
        const results = try aur.search(term, "name-desc");
        for (results) |p| p.show();
        if (results.len == 0) ui.print("No AUR matches.\n", .{});
    } else if (eq(command, "info", "-Si")) {
        try requireNames(names.items);
        for (names.items) |name| {
            if (try db.repo(name)) |p| {
                alpm.show(p);
                ui.print("  Repository build date: {s}\n  Installed size: {d} MiB\n  URL: {s}\n", .{ ui.date(c.alpm_pkg_get_builddate(p)), @divTrunc(c.alpm_pkg_get_isize(p), 1024 * 1024), ui.safe(u.str(c.alpm_pkg_get_url(p))) });
                showDependencies(c.alpm_pkg_get_depends(p), "Dependencies");
                showDependencies(c.alpm_pkg_get_optdepends(p), "Optional dependencies");
            } else {
                const result = try aur.info(&.{name});
                if (result.len == 0) return error.PackageNotFound;
                const p = result[0];
                p.show();
                ui.print("  Base: {s}\n  URL: {s}\n  First submitted: {s}\n", .{ ui.safe(p.PackageBase), ui.safe(p.URL orelse ""), ui.date(p.FirstSubmitted) });
                for ([_][]const []const u8{ p.Depends, p.MakeDepends, p.CheckDepends, p.OptDepends, p.Provides, p.Conflicts }, [_][]const u8{ "Dependencies", "Build dependencies", "Check dependencies", "Optional dependencies", "Provides", "Conflicts" }) |list, label| {
                    ui.print("  {s}: {s}\n", .{ label, ui.safe(try std.mem.join(u.a, " ", list)) });
                }
            }
        }
    } else if (eq(command, "list", "-Q") or eq(command, "foreign", "-Qm")) {
        if (names.items.len > 0) try requireNames(names.items);
        var it = db.installed();
        while (it != null) : (it = it.*.next) {
            const p = alpm.pkg(it.*.data);
            if (names.items.len > 0 and !@import("srcinfo.zig").contains(names.items, alpm.name(p))) continue;
            if (eq(command, "foreign", "-Qm") and !try db.foreign(p)) continue;
            ui.print("{s} {s} · installed {s} · {s}\n", .{ ui.safe(alpm.name(p)), ui.safe(alpm.version(p)), ui.date(c.alpm_pkg_get_installdate(p)), if (c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_EXPLICIT) "explicit" else "dependency" });
        }
    } else if (eq(command, "updates", "-Qu")) {
        if (names.items.len > 0) return error.UnexpectedTargets;
        _ = try updates(&db, devel);
    } else if (eq(command, "upgrade", "-Syu")) {
        if (names.items.len > 0) return error.UnexpectedTargets;
        if (!dry) {
            try tx.escalate(.{ .operation = .upgrade });
            const refreshed = try alpm.Alpm.init();
            db.deinit();
            db = refreshed;
        } else ui.print("Dry run uses the currently cached repository databases.\n", .{});
        const targets = try updates(&db, devel);
        if (targets.len > 0) {
            var builds = try builder.Builder.init(&db, targets);
            builds.preserve_reasons = true;
            defer builds.deinit();
            try builds.plan();
            if (!dry) try builds.execute(true);
        }
    } else if (eq(command, "install", "-S")) {
        try requireNames(names.items);
        var builds = try builder.Builder.init(&db, names.items);
        defer builds.deinit();
        try builds.plan();
        if (!dry) try builds.execute(true);
    } else if (std.mem.eql(u8, command, "build") or std.mem.eql(u8, command, "get")) {
        try requireNames(names.items);
        var builds = try builder.Builder.init(&db, names.items);
        defer builds.deinit();
        if (std.mem.eql(u8, command, "get")) try builds.getSources() else {
            try builds.plan();
            if (!dry) try builds.execute(false);
        }
    } else if (eq(command, "remove", "-R") or std.mem.eql(u8, command, "-Rns")) {
        try requireNames(names.items);
        var targets: std.ArrayList(tx.Target) = .empty;
        for (names.items) |name| {
            if (c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(name)).ptr) == null) return error.PackageNotInstalled;
            try targets.append(u.a, .{ .name = name });
            ui.print("Remove {s}\n", .{ui.safe(name)});
        }
        recursive = recursive or std.mem.eql(u8, command, "-Rns");
        nosave = nosave or std.mem.eql(u8, command, "-Rns");
        if (!dry) try tx.escalate(.{ .operation = .remove, .targets = targets.items, .recursive = recursive, .nosave = nosave });
    } else if (std.mem.eql(u8, command, "orphans")) {
        if (names.items.len > 0) return error.UnexpectedTargets;
        const orphans = try db.orphans();
        for (orphans.items) |name| ui.print("{s}\n", .{ui.safe(name)});
        if (orphans.items.len == 0) ui.print("No orphan dependencies.\n", .{});
    } else if (std.mem.eql(u8, command, "autoremove")) {
        if (names.items.len > 0) return error.UnexpectedTargets;
        const orphans = try db.orphans();
        if (orphans.items.len == 0) {
            ui.print("No orphan dependencies.\n", .{});
            return;
        }
        var targets: std.ArrayList(tx.Target) = .empty;
        for (orphans.items) |name| {
            ui.print("Remove orphan {s}\n", .{ui.safe(name)});
            try targets.append(u.a, .{ .name = name });
        }
        if (!dry) try tx.escalate(.{ .operation = .remove, .targets = targets.items, .recursive = true });
    } else if (std.mem.eql(u8, command, "clean")) {
        if (names.items.len > 0) return error.UnexpectedTargets;
        var builds = try builder.Builder.init(&db, &.{});
        defer builds.deinit();
        if (!dry) try builds.clean() else ui.print("Would clear {s}\n", .{ui.safe(builds.root)});
    } else return error.UnknownCommand;
}
fn eq(value: []const u8, a: []const u8, b: []const u8) bool {
    return std.mem.eql(u8, value, a) or std.mem.eql(u8, value, b);
}
fn requireNames(names: []const []const u8) !void {
    if (names.len == 0) return error.PackageNameRequired;
    for (names, 0..) |name, i| {
        if (!u.validName(name)) return error.InvalidPackageName;
        for (names[0..i]) |previous| if (std.mem.eql(u8, previous, name)) return error.DuplicateTarget;
    }
}
fn showDependencies(list: [*c]c.alpm_list_t, label: []const u8) void {
    ui.print("  {s}:", .{label});
    var it = list;
    while (it != null) : (it = it.*.next) {
        const dep: *c.alpm_depend_t = @ptrCast(@alignCast(it.*.data.?));
        const text = c.alpm_dep_compute_string(dep);
        defer c.free(text);
        ui.print(" {s}", .{ui.safe(u.str(text))});
    }
    ui.print("\n", .{});
}
fn updates(db: *alpm.Alpm, devel: bool) ![]const []const u8 {
    ui.title("Available updates");
    var foreign: std.ArrayList([]const u8) = .empty;
    var it = db.installed();
    var count: usize = 0;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (c.alpm_pkg_should_ignore(db.h, p) != 0) continue;
        if (try db.foreign(p)) try foreign.append(u.a, alpm.name(p)) else if (c.alpm_sync_get_new_version(p, c.alpm_get_syncdbs(db.h))) |next| {
            ui.print("repo  {s} {s} → {s} · built {s}\n", .{ ui.safe(alpm.name(p)), ui.safe(alpm.version(p)), ui.safe(alpm.version(next)), ui.date(c.alpm_pkg_get_builddate(next)) });
            count += 1;
        }
    }
    var targets: std.ArrayList([]const u8) = .empty;
    const remote = try aur.info(foreign.items);
    for (remote) |p| {
        const old = try db.local(p.Name) orelse continue;
        var vcs = false;
        if (devel) for ([_][]const u8{ "-git", "-svn", "-hg", "-bzr" }) |suffix| {
            if (std.mem.endsWith(u8, p.Name, suffix)) vcs = true;
        };
        if (c.alpm_pkg_vercmp((try u.z(p.Version)).ptr, (try u.z(alpm.version(old))).ptr) > 0 or vcs) {
            ui.print("AUR   {s} {s} → {s} · modified {s}{s}\n", .{ ui.safe(p.Name), ui.safe(alpm.version(old)), ui.safe(p.Version), ui.date(p.LastModified), if (vcs) " (VCS rebuild)" else "" });
            try targets.append(u.a, p.Name);
            count += 1;
        }
    }
    if (count == 0) ui.print("Everything is up to date in the cached databases.\n", .{});
    for (foreign.items) |name| {
        var found = false;
        for (remote) |p| if (std.mem.eql(u8, p.Name, name)) {
            found = true;
            break;
        };
        if (!found) ui.print("Foreign package {s} is absent from AUR; left unchanged.\n", .{ui.safe(name)});
    }
    return targets.toOwnedSlice(u.a);
}
fn help() void {
    ui.title("zap · native Zig AUR helper");
    ui.print(
        \\Usage: zap <command> [packages] [options]
        \\
        \\  search TERM       Search repositories and AUR                   -Ss
        \\  info PACKAGES     Package metadata, dependencies, update dates   -Si
        \\  install PACKAGES  Resolve, review, build and install              -S
        \\  build PACKAGES    Review and build AUR packages without installing
        \\  get PACKAGES      Fetch AUR build files into the user cache
        \\  upgrade           Refresh, upgrade repositories, then AUR         -Syu
        \\  updates           Check cached repository and live AUR updates    -Qu
        \\  list [PACKAGES]   Installed versions, dates and install reasons    -Q
        \\  foreign           Installed packages absent from repositories     -Qm
        \\  remove PACKAGES   Remove packages with native dependency checks   -R
        \\  orphans           List unneeded dependencies
        \\  autoremove        Review and remove orphan dependencies
        \\  clean             Clear the user's AUR build cache
        \\  version           Show version and libalpm ABI
        \\
        \\  --dry-run         Plan without builds, escalation or system changes
        \\  --devel           Include VCS rebuilds with updates/upgrade
        \\  --recursive       Remove unneeded dependencies with remove
        \\  --nosave          Discard backup configuration during removal (-Rns)
        \\
        \\AUR sources execute as your user only after review. System changes use
        \\systemd run0/polkit and libalpm, with a separate final confirmation.
        \\Dates are UTC. Repository update dates are build dates; AUR dates are
        \\last modifications. NO_COLOR disables styling.
        \\
    , .{});
}
test {
    _ = @import("util.zig");
    _ = @import("ui.zig");
    _ = @import("config.zig");
    _ = @import("aur.zig");
    _ = @import("srcinfo.zig");
    _ = @import("transaction.zig");
    _ = @import("native_tests.zig");
    _ = @import("builder.zig");
}
