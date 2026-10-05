const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const cli = @import("cli.zig");
const ui = @import("ui.zig");
pub const Match = struct { name: []const u8, source: cli.Scope };

pub fn search(db: *alpm.Alpm, o: cli.Options) ![]Match {
    const term = try std.mem.join(u.a, " ", o.names);
    var matches: std.ArrayList(Match) = .empty;
    if (o.scope != .aur) {
        if (!o.quiet) ui.title(try std.fmt.allocPrint(u.a, "Repository results for {s}", .{term}));
        const packages = try repositorySearch(db, term, false);
        for (packages) |p| {
            if (o.quiet) ui.print("{s}\n", .{ui.safe(alpm.name(p))}) else {
                alpm.showSearch(p, if (o.command == .select) matches.items.len + 1 else null, try installedVersion(db, alpm.name(p)));
            }
            try matches.append(u.a, .{ .name = alpm.name(p), .source = .repo });
        }
        if (packages.len == 0 and !o.quiet) ui.note(.muted, "No repository matches. Try a broader search term.");
    }
    if (o.scope != .repo) {
        if (!o.quiet) ui.title(try std.fmt.allocPrint(u.a, "AUR results for {s}", .{term}));
        const packages = try aur.search(term, o.search_by);
        std.mem.sort(aur.Package, packages, o.sort_by, sortAur);
        for (packages) |p| {
            if (o.quiet) ui.print("{s}\n", .{p.Name}) else {
                p.showSearch(if (o.command == .select) matches.items.len + 1 else null, try installedVersion(db, p.Name));
            }
            try matches.append(u.a, .{ .name = p.Name, .source = .aur });
        }
        if (packages.len == 0 and !o.quiet) ui.note(.muted, "No AUR matches. Try a broader search term.");
    }
    if (matches.items.len > 0 and !o.quiet) {
        ui.print("\n", .{});
        ui.text(try std.fmt.allocPrint(u.a, "{d} package{s} found", .{ matches.items.len, if (matches.items.len == 1) @as([]const u8, "") else "s" }), 2, .muted);
        if (o.command == .search) ui.text("Use zap info <name> for details or zap select <term> to install.", 2, .muted);
    }
    return matches.toOwnedSlice(u.a);
}
// A dependency provider is not necessarily the package shown in search results.
fn installedVersion(db: *alpm.Alpm, name: []const u8) !?[]const u8 {
    const p = c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(name)).ptr) orelse return null;
    return alpm.version(p);
}
fn sortAur(field: []const u8, a: aur.Package, b: aur.Package) bool {
    if (std.mem.eql(u8, field, "votes") and a.NumVotes != b.NumVotes) return a.NumVotes > b.NumVotes;
    if (std.mem.eql(u8, field, "popularity") and a.Popularity != b.Popularity) return a.Popularity > b.Popularity;
    if (std.mem.eql(u8, field, "modified") and a.LastModified != b.LastModified) return a.LastModified > b.LastModified;
    return std.mem.lessThan(u8, a.Name, b.Name);
}
pub fn repositorySearch(db: *alpm.Alpm, term: []const u8, local: bool) ![]alpm.Pkg {
    var needles: [*c]c.alpm_list_t = null;
    needles = c.alpm_list_add(needles, @constCast((try u.z(term)).ptr));
    defer c.alpm_list_free(needles);
    var packages: std.ArrayList(alpm.Pkg) = .empty;
    var dbs = if (local) c.alpm_list_add(null, c.alpm_get_localdb(db.h)) else c.alpm_get_syncdbs(db.h);
    defer if (local) c.alpm_list_free(dbs);
    var seen: std.StringHashMap(void) = .init(u.a);
    defer seen.deinit();
    while (dbs != null) : (dbs = dbs.*.next) {
        const database: *c.alpm_db_t = @ptrCast(@alignCast(dbs.*.data.?));
        if (!local) {
            var usage: c_int = 0;
            try db.check(c.alpm_db_get_usage(database, &usage));
            if (usage & c.ALPM_DB_USAGE_SEARCH == 0) continue;
        }
        var result: [*c]c.alpm_list_t = null;
        try db.check(c.alpm_db_search(database, needles, &result));
        defer c.alpm_list_free(result);
        var it = result;
        while (it != null) : (it = it.*.next) {
            const p = alpm.pkg(it.*.data);
            if (seen.contains(alpm.name(p))) continue;
            try seen.put(alpm.name(p), {});
            try packages.append(u.a, p);
        }
    }
    return packages.toOwnedSlice(u.a);
}
// Number selections are bounded, unique, and never used as shell arguments.
pub fn selection(reply: []const u8, count: usize) ![]usize {
    var selected: std.ArrayList(usize) = .empty;
    var words = std.mem.tokenizeAny(u8, reply, " ,\t");
    while (words.next()) |word| {
        const dash = std.mem.indexOfScalar(u8, word, '-');
        const first = std.fmt.parseInt(usize, if (dash) |d| word[0..d] else word, 10) catch return error.InvalidSelection;
        const last = if (dash) |d| std.fmt.parseInt(usize, word[d + 1 ..], 10) catch return error.InvalidSelection else first;
        if (first == 0 or first > last or last > count) return error.InvalidSelection;
        for (first - 1..last) |index| {
            if (std.mem.indexOfScalar(usize, selected.items, index) == null) try selected.append(u.a, index);
        }
    }
    if (selected.items.len == 0) return error.Cancelled;
    return selected.toOwnedSlice(u.a);
}
pub fn exactLocal(db: *alpm.Alpm, name: []const u8) !alpm.Pkg {
    return c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(name)).ptr) orelse error.PackageNotInstalled;
}
pub fn details(p: alpm.Pkg, local: bool) !void {
    if (local) ui.package(alpm.name(p), alpm.version(p), "installed", "Build date", c.alpm_pkg_get_builddate(p), u.str(c.alpm_pkg_get_desc(p)), null) else alpm.show(p);
    ui.print("\n", .{});
    ui.field("Installed size", try std.fmt.allocPrint(u.a, "{d:.1} MiB", .{@as(f64, @floatFromInt(c.alpm_pkg_get_isize(p))) / (1024 * 1024)}));
    ui.field("URL", u.str(c.alpm_pkg_get_url(p)));
    ui.field("Architecture", u.str(c.alpm_pkg_get_arch(p)));
    ui.field("Packager", u.str(c.alpm_pkg_get_packager(p)));
    if (local) {
        ui.field("Installed", ui.date(c.alpm_pkg_get_installdate(p)));
        ui.field("Reason", if (c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_EXPLICIT) "explicit" else "dependency");
    }
    dependencies(c.alpm_pkg_get_depends(p), "Dependencies");
    dependencies(c.alpm_pkg_get_optdepends(p), "Optional dependencies");
    dependencies(c.alpm_pkg_get_provides(p), "Provides");
    dependencies(c.alpm_pkg_get_conflicts(p), "Conflicts");
    strings(c.alpm_pkg_get_licenses(p), "Licenses");
    strings(c.alpm_pkg_get_groups(p), "Groups");
    if (local) {
        const required = c.alpm_pkg_compute_requiredby(p);
        defer freeStrings(required);
        strings(required, "Required by");
        const optional = c.alpm_pkg_compute_optionalfor(p);
        defer freeStrings(optional);
        strings(optional, "Optional for");
    }
    ui.print("\n", .{});
}
fn freeStrings(list: [*c]c.alpm_list_t) void {
    var it = list;
    while (it != null) : (it = it.*.next) c.free(it.*.data);
    c.alpm_list_free(list);
}
fn strings(list: [*c]c.alpm_list_t, label: []const u8) void {
    var values: std.ArrayList([]const u8) = .empty;
    defer values.deinit(u.a);
    var it = list;
    while (it != null) : (it = it.*.next) values.append(u.a, u.str(@ptrCast(it.*.data))) catch return;
    const joined = std.mem.join(u.a, "  ", values.items) catch return;
    defer u.a.free(joined);
    ui.field(label, joined);
}
pub fn dependencies(list: [*c]c.alpm_list_t, label: []const u8) void {
    var values: std.ArrayList(u8) = .empty;
    defer values.deinit(u.a);
    var it = list;
    while (it != null) : (it = it.*.next) {
        const dep: *c.alpm_depend_t = @ptrCast(@alignCast(it.*.data.?));
        const value = c.alpm_dep_compute_string(dep);
        defer c.free(value);
        if (values.items.len > 0) values.appendSlice(u.a, "  ") catch return;
        values.appendSlice(u.a, u.str(value)) catch return;
    }
    ui.field(label, values.items);
}
pub fn validFile(path: []const u8) bool {
    if (path.len == 0 or std.fs.path.isAbsolute(path)) return false;
    var parts = std.mem.splitScalar(u8, path, '/');
    while (parts.next()) |part| if (std.mem.eql(u8, part, "..") or std.mem.eql(u8, part, ".")) return false;
    return true;
}
pub fn filePath(db: *alpm.Alpm, path: []const u8) ![]const u8 {
    if (!validFile(path)) return error.InvalidPackageFile;
    return std.fs.path.join(u.a, &.{ db.cfg.root, path });
}
pub fn fileEntries(p: alpm.Pkg) ![]const c.alpm_file_t {
    const list = c.alpm_pkg_get_files(p) orelse return error.InvalidPackageFile;
    if (list.*.count == 0) return &.{};
    if (list.*.files == null) return error.InvalidPackageFile;
    return list.*.files[0..list.*.count];
}
pub fn files(db: *alpm.Alpm, names: []const []const u8, quiet: bool) !void {
    for (names) |name| {
        const p = try exactLocal(db, name);
        const package_files = try fileEntries(p);
        for (package_files) |file| {
            const path = try filePath(db, u.str(file.name));
            if (quiet) ui.print("{s}\n", .{ui.safe(path)}) else ui.print("{s} {s}\n", .{ ui.safe(name), ui.safe(path) });
        }
    }
}
pub fn owners(db: *alpm.Alpm, paths: []const []const u8, quiet: bool) !void {
    var missing = false;
    for (paths) |input| {
        const resolved = try std.fs.path.resolve(u.a, &.{ try std.process.getCwdAlloc(u.a), input });
        // Resolve directory aliases (/bin -> /usr/bin), preserving the final symlink.
        const parent = try std.fs.cwd().realpathAlloc(u.a, std.fs.path.dirname(resolved) orelse "/");
        const path = try std.fs.path.join(u.a, &.{ parent, std.fs.path.basename(resolved) });
        var found = false;
        var it = db.installed();
        while (it != null) : (it = it.*.next) {
            const p = alpm.pkg(it.*.data);
            const package_files = try fileEntries(p);
            for (package_files) |file| {
                const candidate = std.mem.trimEnd(u8, try filePath(db, u.str(file.name)), "/");
                if (!std.mem.eql(u8, candidate, path)) continue;
                if (quiet) ui.print("{s}\n", .{ui.safe(alpm.name(p))}) else ui.print("{s} is owned by {s} {s}\n", .{ ui.safe(input), ui.safe(alpm.name(p)), ui.safe(alpm.version(p)) });
                found = true;
                break;
            }
        }
        if (!found) {
            missing = true;
            if (!quiet) ui.print("No installed package owns {s}\n", .{ui.safe(input)});
        }
    }
    if (missing) return error.FileNotOwned;
}
pub fn check(db: *alpm.Alpm, names: []const []const u8) !void {
    if (names.len > 0) for (names) |name| {
        _ = try exactLocal(db, name);
    };
    var missing: usize = 0;
    var it = db.installed();
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (names.len > 0 and !@import("srcinfo.zig").contains(names, alpm.name(p))) continue;
        const package_files = try fileEntries(p);
        var absent: usize = 0;
        for (package_files) |file| {
            const path = try filePath(db, u.str(file.name));
            var st: c.struct_stat = undefined;
            if (c.lstat((try u.z(std.mem.trimEnd(u8, path, "/"))).ptr, &st) == 0) continue;
            switch (std.posix.errno(-1)) {
                .NOENT, .NOTDIR => {},
                else => return error.FileInspectionFailed,
            }
            ui.print("Missing {s}: {s}\n", .{ ui.safe(alpm.name(p)), ui.safe(path) });
            absent += 1;
        }
        missing += absent;
        ui.print("{s}: {d} files, {d} missing\n", .{ ui.safe(alpm.name(p)), package_files.len, absent });
    }
    if (missing != 0) return error.MissingInstalledFiles;
}
pub fn stats(db: *alpm.Alpm) !void {
    var total: usize = 0;
    var explicit: usize = 0;
    var foreign: usize = 0;
    var size: i64 = 0;
    var packages: std.ArrayList(alpm.Pkg) = .empty;
    var it = db.installed();
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        total += 1;
        if (c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_EXPLICIT) explicit += 1;
        if (try db.foreign(p)) foreign += 1;
        size += c.alpm_pkg_get_isize(p);
        try packages.append(u.a, p);
    }
    const orphans = try db.orphans();
    ui.title("Installed package statistics");
    inline for (.{ "Packages", "Explicit", "Dependencies", "Foreign", "Orphans" }, .{ total, explicit, total - explicit, foreign, orphans.items.len }) |label, count| {
        ui.field(label, try std.fmt.allocPrint(u.a, "{d}", .{count}));
    }
    ui.field("Installed size", try std.fmt.allocPrint(u.a, "{d:.2} GiB", .{@as(f64, @floatFromInt(size)) / (1024 * 1024 * 1024)}));
    std.mem.sort(alpm.Pkg, packages.items, {}, larger);
    ui.title("Largest packages");
    const largest = if (packages.items.len > 0) c.alpm_pkg_get_isize(packages.items[0]) else 0;
    for (packages.items[0..@min(10, packages.items.len)], 1..) |p, rank| {
        ui.text(try std.fmt.allocPrint(u.a, "{d: >2}. {s}", .{ rank, alpm.name(p) }), 2, .bold);
        const bar_width = @min(ui.columns() -| 20, 28);
        const filled = if (largest > 0) @as(usize, @intCast(@divTrunc(@as(i128, @max(c.alpm_pkg_get_isize(p), 0)) * @as(i128, @intCast(bar_width)), largest))) else 0;
        ui.print("      {s}", .{ui.style(.accent)});
        for (0..filled) |_| ui.print("━", .{});
        ui.print("{s}", .{ui.style(.muted)});
        for (filled..bar_width) |_| ui.print("─", .{});
        ui.print("{s}  {d:.1} MiB\n", .{ ui.style(.reset), @as(f64, @floatFromInt(c.alpm_pkg_get_isize(p))) / (1024 * 1024) });
    }
    ui.print("\n", .{});
}
fn larger(_: void, a: alpm.Pkg, b: alpm.Pkg) bool {
    return c.alpm_pkg_get_isize(a) > c.alpm_pkg_get_isize(b);
}
test "interactive selections reject invalid ranges and deduplicate" {
    try std.testing.expectEqualSlices(usize, &.{ 0, 1, 2, 4 }, try selection("1-3, 2 5", 5));
    for ([_][]const u8{ "0", "6", "3-1", "1-9999999999", "-1", "all", "1;id" }) |input| try std.testing.expectError(error.InvalidSelection, selection(input, 5));
    try std.testing.expectError(error.Cancelled, selection("", 5));
}
test "installed file paths cannot escape the configured root" {
    for ([_][]const u8{ "", "/etc/passwd", "../etc/passwd", "usr/../../etc", "usr/./bin" }) |path| try std.testing.expect(!validFile(path));
    try std.testing.expect(validFile("usr/bin/tool"));
    try std.testing.expect(validFile("usr/share/"));
}
test "AUR sort is deterministic with name tie breaks" {
    var packages = [_]aur.Package{
        .{ .Name = "z", .PackageBase = "z", .Version = "1", .NumVotes = 2 },
        .{ .Name = "a", .PackageBase = "a", .Version = "1", .NumVotes = 2 },
        .{ .Name = "b", .PackageBase = "b", .Version = "1", .NumVotes = 9 },
    };
    std.mem.sort(aur.Package, &packages, @as([]const u8, "votes"), sortAur);
    try std.testing.expectEqualStrings("b", packages[0].Name);
    try std.testing.expectEqualStrings("a", packages[1].Name);
}

test "installed search badges require an exact identity, not a virtual provider" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    try native.installedFixture(tmp.dir, "replacement", "");
    const desc_path = "db/local/replacement-1.0-1/desc";
    const desc = try tmp.dir.readFileAlloc(u.a, desc_path, 4096);
    try tmp.dir.writeFile(.{ .sub_path = desc_path, .data = try std.fmt.allocPrint(u.a, "{s}%PROVIDES%\noriginal\n\n", .{desc}) });
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    try std.testing.expect(try db.local("original") != null);
    try std.testing.expect(try installedVersion(&db, "original") == null);
    try std.testing.expectEqualStrings("1.0-1", (try installedVersion(&db, "replacement")).?);
}

test "file queries tolerate empty package lists and resolve directory aliases" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    try native.installedFixture(tmp.dir, "empty", "");
    try native.installedFixture(tmp.dir, "owner", "usr/bin/tool");
    try tmp.dir.makePath("usr/bin");
    try tmp.dir.writeFile(.{ .sub_path = "usr/bin/tool", .data = "tool" });
    try tmp.dir.symLink("usr/bin", "bin", .{});
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{ .root = root } };
    defer db.deinit();
    try std.testing.expectEqual(@as(usize, 0), (try fileEntries(try exactLocal(&db, "empty"))).len);
    try owners(&db, &.{try std.fs.path.join(u.a, &.{ root, "bin/tool" })}, true);
    try check(&db, &.{ "empty", "owner" });
    try tmp.dir.deleteFile("usr/bin/tool");
    try std.testing.expectError(error.MissingInstalledFiles, check(&db, &.{"owner"}));
    try std.testing.expectError(error.FileNotOwned, owners(&db, &.{try std.fs.path.join(u.a, &.{ root, "usr/bin/missing" })}, true));
}
