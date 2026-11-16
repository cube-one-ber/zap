//! Command dispatch: each command is a short function over the modules that
//! do the work.
const std = @import("std");
const builtin = @import("builtin");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const tx = @import("transaction.zig");
const cli = @import("cli.zig");
const query = @import("query.zig");
const errors = @import("errors.zig");
const Builder = @import("builder.zig").Builder;

pub fn run(o: cli.Options) !void {
    if (o.command == .help) return @import("help.zig").show(o.topic);
    if (o.dry) ui.note(.warning, "Dry run: nothing will be built, elevated or changed.");
    if (o.command == .version) {
        return ui.print("zap {s} · libalpm {s} · Zig {s}\n", .{ u.version, ui.safe(u.str(c.alpm_version())), builtin.zig_version_string });
    }
    if (c.geteuid() == 0) return error.BuildsMustNotRunAsRoot;
    tx.setElevator(o.elevate);
    // A misconfigured elevation tool is reported before any work is shown.
    // Dry runs refuse an unknown choice, but need no tool on this machine.
    if (cli.elevating.contains(o.command)) _ = if (o.dry) try tx.chosenElevator() else try tx.trustedElevator();
    if (o.command == .news) return @import("news.zig").show();
    try checkNames(o);

    var db = try alpm.Alpm.init();
    defer db.deinit();
    switch (o.command) {
        .search => _ = try query.search(&db, o),
        .select => try select(&db, o),
        .info, .localinfo => try query.info(&db, o),
        .list, .foreign => try query.list(&db, o),
        .local_search => try query.localSearch(&db, o),
        .files => try query.files(&db, o.names, o.quiet),
        .owns => try query.owners(&db, o.names, o.quiet),
        .check => try query.check(&db, o.names),
        .stats => try query.stats(&db),
        .updates => _ = try updates(&db, o),
        .upgrade => try upgrade(&db, o),
        .install, .build => try install(&db, o),
        .get, .pkgbuild => try fetch(&db, o),
        .build_local, .install_local => try installLocal(&db, o),
        .install_files => try installFiles(&db, o),
        .remove => try remove(&db, o),
        .reason => try reason(&db, o),
        .orphans, .autoremove => try orphans(&db, o),
        .clean => try clean(&db, o),
        .help, .version, .news => unreachable,
    }
}

/// Commands whose targets are package names accept only valid, distinct ones.
fn checkNames(o: cli.Options) !void {
    switch (o.command) {
        .install, .build, .get, .pkgbuild, .info, .localinfo, .files, .reason, .remove, .list, .foreign, .check, .clean => {},
        else => return,
    }
    for (o.names, 0..) |name, i| {
        if (!u.validName(name)) return errors.with(error.InvalidPackageName, "{s}", .{name});
        for (o.names[0..i]) |previous| if (std.mem.eql(u8, previous, name)) return errors.with(error.DuplicateTarget, "{s}", .{name});
    }
}

fn configure(builds: *Builder, o: cli.Options) void {
    builds.scope = o.scope;
    builds.needed = o.needed;
    builds.reason = o.reason;
    builds.sandbox = o.sandbox orelse true;
    builds.clean_after = o.clean_after;
    builds.rebuild_tree = o.rebuild_tree;
}

// ── Install and build ───────────────────────────────────────────────────────

fn install(db: *alpm.Alpm, o: cli.Options) !void {
    var builds = try Builder.init(db, o.names);
    defer builds.deinit();
    configure(&builds, o);
    try builds.plan();
    // A dry run must refuse what the real build would refuse.
    if (o.command == .build) try builds.requireInstalledDependencies();
    if (!o.dry) try builds.execute(o.command == .install);
}

fn installLocal(db: *alpm.Alpm, o: cli.Options) !void {
    var builds = try Builder.init(db, &.{});
    defer builds.deinit();
    configure(&builds, o);
    try builds.loadLocal(o.names);
    try builds.plan();
    if (o.command == .build_local) try builds.requireInstalledDependencies();
    if (!o.dry) try builds.execute(o.command == .install_local);
}

fn fetch(db: *alpm.Alpm, o: cli.Options) !void {
    var builds = try Builder.init(db, o.names);
    defer builds.deinit();
    if (o.command == .get) try builds.getSources() else try builds.printPkgbuilds();
}

fn select(db: *alpm.Alpm, o: cli.Options) !void {
    const matches = try query.search(db, o);
    if (matches.len == 0) return errors.with(error.PackageNotFound, "{s}", .{try std.mem.join(u.a, " ", o.names)});
    const selected = while (true) {
        const reply = try ui.answer("Install which? Numbers or ranges such as 1 3-5; empty cancels:");
        break query.selection(reply, matches.len) catch |err| {
            if (err != error.InvalidSelection) return err;
            ui.notef(.warning, "Use numbers from 1 to {d}, or ranges such as 1-3.", .{matches.len});
            continue;
        };
    };
    var names: std.ArrayList([]const u8) = .empty;
    var aur_names: std.ArrayList([]const u8) = .empty;
    for (selected) |index| {
        const match = matches[index];
        // One name cannot be installed from both a repository and the AUR.
        for (selected) |other| {
            if (other != index and std.mem.eql(u8, match.name, matches[other].name)) return errors.with(error.ConflictingSelection, "{s}", .{match.name});
        }
        try names.append(u.a, match.name);
        // Keep the chosen source when a repository and the AUR share a name.
        if (match.source == .aur) try aur_names.append(u.a, match.name);
    }
    var builds = try Builder.init(db, names.items);
    defer builds.deinit();
    configure(&builds, o);
    builds.aur_targets = aur_names.items;
    try builds.plan();
    if (!o.dry) try builds.execute(true);
}

/// Hash and identify local archives by their own metadata. Nothing in them
/// is executed, and a final symlink is refused rather than followed.
pub fn archiveTargets(db: *alpm.Alpm, paths: []const []const u8, wanted_reason: ?cli.Reason) ![]tx.Target {
    var targets: std.ArrayList(tx.Target) = .empty;
    for (paths) |input| {
        const path = try std.fs.path.resolve(u.a, &.{ try std.process.getCwdAlloc(u.a), input });
        const digest = try tx.hashFile(path);
        var p: ?*c.alpm_pkg_t = null;
        db.check(c.alpm_pkg_load(db.h, (try u.z(path)).ptr, 1, 0, &p)) catch return errors.with(error.InvalidBuildArchive, "{s}", .{path});
        defer _ = c.alpm_pkg_free(p);
        // The metadata just read must describe the bytes that were hashed.
        if (!std.mem.eql(u8, digest, try tx.hashFile(path))) return errors.with(error.ArchiveChanged, "{s}", .{path});
        const name = try u.a.dupe(u8, alpm.name(p.?));
        for (targets.items) |previous| if (std.mem.eql(u8, previous.name, name)) return errors.with(error.DuplicateTarget, "{s}", .{name});
        try targets.append(u.a, .{ .name = name, .archive = path, .sha256 = digest, .reason = wanted_reason });
    }
    try tx.validate(.{ .operation = .install, .targets = targets.items });
    return targets.toOwnedSlice(u.a);
}

fn installFiles(db: *alpm.Alpm, o: cli.Options) !void {
    const targets = try archiveTargets(db, o.names, o.reason);
    ui.heading("Package archives", ui.count(targets.len, "archive", "archives"));
    for (targets) |target| {
        ui.text(target.name, 2, .bold);
        ui.fieldRaw("  Archive", target.archive.?);
        ui.fieldRaw("  SHA-256", target.sha256.?);
    }
    ui.blank();
    if (!o.dry) try tx.escalate(.{ .operation = .install, .targets = targets, .needed = o.needed });
}

// ── Updates ─────────────────────────────────────────────────────────────────

const vcs_suffixes = [_][]const u8{ "-git", "-svn", "-hg", "-bzr", "-cvs", "-darcs", "-fossil" };

fn isVcs(name: []const u8) bool {
    for (vcs_suffixes) |suffix| if (std.mem.endsWith(u8, name, suffix)) return true;
    return false;
}

/// Show pending updates and return the AUR packages to rebuild.
fn updates(db: *alpm.Alpm, o: cli.Options) ![]const []const u8 {
    var table = ui.Table.init(&.{ .{ .title = "Package", .tone = .bold }, .{ .title = "Current" }, .{ .title = "New", .tone = .success, .joiner = " → " }, .{ .title = "Source" }, .{ .title = "Updated", .tone = .muted, .label = "updated" } });
    var names: std.ArrayList([]const u8) = .empty;
    var foreign: std.ArrayList([]const u8) = .empty;
    var it = db.installed();
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (c.alpm_pkg_should_ignore(db.h, p) != 0) continue;
        if (db.foreign(p)) {
            if (o.scope != .repo) try foreign.append(u.a, alpm.name(p));
        } else if (o.scope != .aur) if (c.alpm_sync_get_new_version(p, c.alpm_get_syncdbs(db.h))) |next| {
            try names.append(u.a, alpm.name(p));
            table.add(&.{
                .{ .text = alpm.name(p) },
                .{ .text = alpm.version(p) },
                .{ .text = alpm.version(next) },
                .{ .text = alpm.repository(next), .tone = .accent },
                .{ .text = ui.date(c.alpm_pkg_get_builddate(next)) },
            });
        };
    }

    var targets: std.ArrayList([]const u8) = .empty;
    var absent: std.ArrayList([]const u8) = .empty;
    const remote = try aur.info(foreign.items);
    for (foreign.items) |name| {
        const p = for (remote) |candidate| {
            if (std.mem.eql(u8, candidate.Name, name)) break candidate;
        } else {
            try absent.append(u.a, name);
            continue;
        };
        const old = try db.installedPkg(name) orelse continue;
        const rebuild = o.devel and isVcs(name);
        const newer = c.alpm_pkg_vercmp((try u.z(p.Version)).ptr, c.alpm_pkg_get_version(old)) > 0;
        if (!newer and !rebuild) continue;
        try names.append(u.a, name);
        try targets.append(u.a, name);
        table.add(&.{
            .{ .text = name },
            .{ .text = alpm.version(old) },
            .{ .text = p.Version },
            .{ .text = if (newer) "aur" else "aur · rebuild", .tone = if (newer) .special else .warning },
            .{ .text = ui.date(p.LastModified) },
        });
    }

    if (o.quiet) {
        ui.hold();
        defer ui.release();
        for (names.items) |name| ui.print("{s}\n", .{ui.safe(name)});
        return targets.toOwnedSlice(u.a);
    }
    ui.heading("Updates", ui.count(names.items.len, "package", "packages"));
    table.render();
    if (names.items.len == 0) {
        ui.note(.success, if (o.scope == .aur) "AUR packages are up to date." else "Everything is up to date.");
    } else if (o.command == .updates) {
        ui.blank();
        ui.note(.info, "zap upgrade applies these updates.");
    }
    if (o.command == .updates and o.scope != .aur) {
        const synced = db.syncedAt();
        if (synced == 0) {
            ui.note(.info, "Repository databases are as fresh as the last zap upgrade.");
        } else ui.notef(.info, "The oldest repository database dates from {s}; zap upgrade refreshes them.", .{ui.age(synced, std.time.timestamp())});
    }
    if (absent.items.len > 0) {
        ui.notef(.info, "Not in the AUR, left as they are: {s}", .{try std.mem.join(u.a, ", ", absent.items)});
    }
    ui.blank();
    return targets.toOwnedSlice(u.a);
}

fn upgrade(db: *alpm.Alpm, o: cli.Options) !void {
    if (o.scope != .aur) {
        if (o.dry) {
            ui.note(.info, "A dry run compares against the repository databases as last refreshed.");
        } else {
            try tx.escalate(.{ .operation = .upgrade });
            try db.reload();
        }
    }
    const targets = try updates(db, o);
    if (targets.len == 0) return;
    var builds = try Builder.init(db, targets);
    defer builds.deinit();
    configure(&builds, o);
    builds.scope = .aur;
    builds.preserve_reasons = true;
    try builds.plan();
    if (!o.dry) try builds.execute(true);
}

// ── Remove and maintain ─────────────────────────────────────────────────────

/// List packages with their sizes and return the total installed size.
fn packageTable(packages: []const alpm.Pkg) i64 {
    var table = ui.Table.init(&.{ .{ .title = "Package", .tone = .bold }, .{ .title = "Version" }, .{ .title = "Installed", .tone = .muted, .label = "installed" }, .{ .title = "Size", .alignment = .right } });
    var total: i64 = 0;
    for (packages) |p| {
        total += c.alpm_pkg_get_isize(p);
        table.addText(&.{ alpm.name(p), alpm.version(p), ui.date(c.alpm_pkg_get_installdate(p)), ui.size(c.alpm_pkg_get_isize(p)) });
    }
    table.render();
    return total;
}

fn remove(db: *alpm.Alpm, o: cli.Options) !void {
    var targets: std.ArrayList(tx.Target) = .empty;
    var packages: std.ArrayList(alpm.Pkg) = .empty;
    for (o.names) |name| {
        try packages.append(u.a, try query.installed(db, name));
        try targets.append(u.a, .{ .name = name });
    }
    // Refuse before asking for root what libalpm or HoldPkg would refuse.
    for (packages.items) |p| if (try tx.held(db, alpm.name(p))) |pattern| {
        return errors.with(error.ProtectedPackage, "{s} (HoldPkg {s})", .{ alpm.name(p), pattern });
    };
    var table = ui.Table.init(&.{
        .{ .title = "Package", .tone = .bold },
        .{ .title = "Version" },
        .{ .title = "Size", .alignment = .right },
        .{ .title = "Used by", .tone = .muted, .label = "used by" },
    });
    var total: i64 = 0;
    for (packages.items) |p| {
        // Informational: another installed provider may still satisfy these.
        const required = c.alpm_pkg_compute_requiredby(p);
        defer alpm.freeStrings(required);
        var others: std.ArrayList([]const u8) = .empty;
        for (try alpm.strings(required)) |dependant| {
            if (!u.contains(o.names, dependant)) try others.append(u.a, try u.a.dupe(u8, dependant));
        }
        total += c.alpm_pkg_get_isize(p);
        table.add(&.{
            .{ .text = alpm.name(p) },
            .{ .text = alpm.version(p) },
            .{ .text = ui.size(c.alpm_pkg_get_isize(p)) },
            .{ .text = try usedBy(others.items) },
        });
    }
    ui.heading(if (o.dry) "Would remove" else "Remove", ui.count(packages.items.len, "package", "packages"));
    table.render();
    ui.blank();
    // What would actually break, as libalpm decides it: a dependency that no
    // remaining installed package satisfies.
    const broken = try brokenBy(db, packages.items);
    if (broken.len > 0) {
        const named = broken[0..@min(broken.len, 5)];
        const more = if (broken.len > named.len) try std.fmt.allocPrint(u.a, "; and {d} more", .{broken.len - named.len}) else "";
        return errors.with(error.RequiredByOthers, "{s}{s}", .{ try std.mem.join(u.a, "; ", named), more });
    }
    ui.notef(.info, "Frees {s}{s}{s}.", .{
        ui.size(total),
        if (o.recursive) " plus any dependencies nothing else needs" else "",
        if (o.nosave) "; configuration files are deleted, not kept as .pacsave" else "",
    });
    if (o.dry) return ui.blank();
    try tx.escalate(.{ .operation = .remove, .targets = targets.items, .recursive = o.recursive, .nosave = o.nosave });
}

/// The "Used by" cell: the first few dependants, then how many more.
fn usedBy(dependants: []const []const u8) ![]const u8 {
    if (dependants.len == 0) return "nothing";
    const named = try std.mem.join(u.a, ", ", dependants[0..@min(dependants.len, 8)]);
    if (dependants.len <= 8) return named;
    return std.fmt.allocPrint(u.a, "{s} and {d} more", .{ named, dependants.len - 8 });
}

/// Installed packages whose dependencies would no longer be satisfied once
/// `removals` are gone, as `dependant needs dependency`.
fn brokenBy(db: *alpm.Alpm, removals: []const alpm.Pkg) ![]const []const u8 {
    var targets: alpm.List = null;
    defer c.alpm_list_free(targets);
    for (removals) |p| targets = c.alpm_list_add(targets, p);
    const missing = c.alpm_checkdeps(db.h, db.installed(), targets, null, 1);
    defer {
        var it = missing;
        while (it != null) : (it = it.*.next) c.alpm_depmissing_free(@ptrCast(@alignCast(it.*.data.?)));
        c.alpm_list_free(missing);
    }
    var result: std.ArrayList([]const u8) = .empty;
    var it = missing;
    while (it != null) : (it = it.*.next) {
        const entry: *c.alpm_depmissing_t = @ptrCast(@alignCast(it.*.data.?));
        const dep = c.alpm_dep_compute_string(entry.depend);
        defer c.free(dep);
        try result.append(u.a, try std.fmt.allocPrint(u.a, "{s} needs {s}", .{ u.str(entry.target), u.str(dep) }));
    }
    return result.toOwnedSlice(u.a);
}

fn reason(db: *alpm.Alpm, o: cli.Options) !void {
    var targets: std.ArrayList(tx.Target) = .empty;
    var table = ui.Table.init(&.{ .{ .title = "Package", .tone = .bold }, .{ .title = "Current" }, .{ .title = "New", .tone = .accent, .joiner = " → " } });
    for (o.names) |name| {
        const p = try query.installed(db, name);
        try targets.append(u.a, .{ .name = name, .reason = o.reason });
        table.addText(&.{ name, alpm.reason(p), @tagName(o.reason.?) });
    }
    if (o.dry) {
        ui.heading("Would change installation reasons", ui.count(targets.items.len, "package", "packages"));
        table.render();
        return ui.blank();
    }
    try tx.escalate(.{ .operation = .reason, .targets = targets.items });
}

fn orphans(db: *alpm.Alpm, o: cli.Options) !void {
    const packages = try db.orphans();
    if (o.quiet) {
        ui.hold();
        defer ui.release();
        for (packages) |p| ui.print("{s}\n", .{ui.safe(alpm.name(p))});
        return;
    }
    if (packages.len == 0) return ui.note(.success, "No orphaned dependencies.");
    ui.heading(if (o.command == .orphans) "Orphaned dependencies" else if (o.dry) "Would remove orphans" else "Orphans to remove", ui.count(packages.len, "package", "packages"));
    const total = packageTable(packages);
    ui.blank();
    if (o.command == .orphans) {
        ui.notef(.info, "{s} in total · zap autoremove removes them.", .{ui.size(total)});
        return ui.blank();
    }
    ui.notef(.info, "Frees at least {s}; their own unneeded dependencies are removed too.", .{ui.size(total)});
    ui.blank();
    if (o.dry) return;
    var targets: std.ArrayList(tx.Target) = .empty;
    for (packages) |p| try targets.append(u.a, .{ .name = alpm.name(p) });
    try tx.escalate(.{ .operation = .remove, .targets = targets.items, .recursive = true });
}

fn clean(db: *alpm.Alpm, o: cli.Options) !void {
    var builds = try Builder.init(db, o.names);
    defer builds.deinit();
    try builds.clean(o.dry);
}

test "local archives use metadata identities and reject duplicates and symlinks" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    const archive = try native.fixture(tmp.dir, root, "zap-archive-fixture", "");
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    try tmp.dir.copyFile(std.fs.path.basename(archive), tmp.dir, "arbitrary name.pkg.tar", .{});
    const arbitrary = try std.fs.path.join(u.a, &.{ root, "arbitrary name.pkg.tar" });
    const targets = try archiveTargets(&db, &.{arbitrary}, .dependency);
    try std.testing.expectEqualStrings("zap-archive-fixture", targets[0].name);
    try std.testing.expectEqual(cli.Reason.dependency, targets[0].reason.?);
    try std.testing.expectError(error.DuplicateTarget, archiveTargets(&db, &.{ archive, arbitrary }, null));
    try tmp.dir.symLink(std.fs.path.basename(archive), "alias.pkg.tar", .{});
    try std.testing.expectError(error.UnsafeArchive, archiveTargets(&db, &.{try std.fs.path.join(u.a, &.{ root, "alias.pkg.tar" })}, null));
    try tmp.dir.writeFile(.{ .sub_path = "not-a-package.pkg.tar", .data = "plain text" });
    try std.testing.expectError(error.InvalidBuildArchive, archiveTargets(&db, &.{try std.fs.path.join(u.a, &.{ root, "not-a-package.pkg.tar" })}, null));
}

test "removal is refused only when no remaining package satisfies a dependency" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    // Two providers of a virtual dependency, and a package that needs it.
    for ([_][2][]const u8{ .{ "jdk-a", "%PROVIDES%\njava-runtime\n\n" }, .{ "jdk-b", "%PROVIDES%\njava-runtime\n\n" }, .{ "app", "%DEPENDS%\njava-runtime\n\n" } }) |fixture| {
        try native.installedFixture(tmp.dir, fixture[0], "");
        const desc_path = try std.fmt.allocPrint(u.a, "db/local/{s}-1.0-1/desc", .{fixture[0]});
        const desc = try tmp.dir.readFileAlloc(u.a, desc_path, 4096);
        try tmp.dir.writeFile(.{ .sub_path = desc_path, .data = try std.fmt.allocPrint(u.a, "{s}{s}", .{ desc, fixture[1] }) });
    }
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    const a = try query.installed(&db, "jdk-a");
    const b = try query.installed(&db, "jdk-b");
    // jdk-b still provides java-runtime: nothing breaks.
    try std.testing.expectEqual(@as(usize, 0), (try brokenBy(&db, &.{a})).len);
    const both = try brokenBy(&db, &.{ a, b });
    try std.testing.expectEqual(@as(usize, 1), both.len);
    try std.testing.expectEqualStrings("app needs java-runtime", both[0]);
}

test "only version-control suffixes mark a package for --devel rebuilds" {
    try std.testing.expect(isVcs("neovim-git"));
    try std.testing.expect(isVcs("tool-fossil"));
    try std.testing.expect(!isVcs("git"));
    try std.testing.expect(!isVcs("gitui"));
    try std.testing.expect(!isVcs("some-git-tool"));
}
