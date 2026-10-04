const std = @import("std");
const u = @import("util.zig");
const c = u.c;
pub fn fixture(dir: std.fs.Dir, root: []const u8, name: []const u8, extra: []const u8) ![]const u8 {
    try dir.writeFile(.{ .sub_path = ".PKGINFO", .data = try std.fmt.allocPrint(u.a, "pkgname = {s}\npkgver = 1.0-1\npkgdesc = isolated transaction fixture\nurl = https://example.org\nbuilddate = 1750000000\npackager = zap tests\nsize = 1\narch = any\n{s}", .{ name, extra }) });
    const path = try std.fmt.allocPrint(u.a, "{s}/{s}-1.0-1-any.pkg.tar", .{ root, name });
    try u.run(&.{ "/usr/bin/bsdtar", "-cf", path, "-C", root, ".PKGINFO" }, null);
    return path;
}
pub fn handle(root: []const u8) !*c.alpm_handle_t {
    const dbpath = try std.fs.path.join(u.a, &.{ root, "db" });
    try std.fs.cwd().makePath(dbpath);
    var err: c.alpm_errno_t = 0;
    const h = c.alpm_initialize((try u.z(root)).ptr, (try u.z(dbpath)).ptr, &err) orelse return error.TestHandleFailed;
    if (c.alpm_option_add_architecture(h, "any") != 0) return error.TestHandleFailed;
    return h;
}
fn add(h: *c.alpm_handle_t, path: []const u8) !void {
    var p: ?*c.alpm_pkg_t = null;
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_pkg_load(h, (try u.z(path)).ptr, 1, 0, &p));
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_add_pkg(h, p));
}
test "native ALPM prepares packages and releases its isolated database lock" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    defer u.a.free(root);
    const archive = try fixture(tmp.dir, root, "zap-fixture", "");
    const h = try handle(root);
    defer _ = c.alpm_release(h);
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_init(h, 0));
    try add(h, archive);
    var data: [*c]c.alpm_list_t = null;
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_prepare(h, &data));
    const added = c.alpm_trans_get_add(h);
    try std.testing.expect(added != null);
    try std.testing.expectEqual(@as(usize, 1), c.alpm_list_count(added));
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_release(h));
    try std.testing.expectError(error.FileNotFound, tmp.dir.access("db/db.lck", .{}));
}
test "native ALPM reports versioned missing dependencies" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    defer u.a.free(root);
    const archive = try fixture(tmp.dir, root, "zap-fixture", "depend = nonexistent>=2\n");
    const h = try handle(root);
    defer _ = c.alpm_release(h);
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_init(h, 0));
    defer _ = c.alpm_trans_release(h);
    try add(h, archive);
    var data: [*c]c.alpm_list_t = null;
    try std.testing.expectEqual(@as(c_int, -1), c.alpm_trans_prepare(h, &data));
    try std.testing.expectEqual(@as(c.alpm_errno_t, c.ALPM_ERR_UNSATISFIED_DEPS), c.alpm_errno(h));
    try std.testing.expect(data != null);
    var it = data;
    while (it != null) : (it = it.*.next) c.alpm_depmissing_free(@ptrCast(@alignCast(it.*.data.?)));
    c.alpm_list_free(data);
}
test "native ALPM refuses conflicting packages" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    defer u.a.free(root);
    const first = try fixture(tmp.dir, root, "zap-fixture-a", "conflict = zap-fixture-b\n");
    const second = try fixture(tmp.dir, root, "zap-fixture-b", "");
    const h = try handle(root);
    defer _ = c.alpm_release(h);
    try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_init(h, 0));
    defer _ = c.alpm_trans_release(h);
    try add(h, first);
    try add(h, second);
    var data: [*c]c.alpm_list_t = null;
    try std.testing.expectEqual(@as(c_int, -1), c.alpm_trans_prepare(h, &data));
    try std.testing.expectEqual(@as(c.alpm_errno_t, c.ALPM_ERR_CONFLICTING_DEPS), c.alpm_errno(h));
    var it = data;
    while (it != null) : (it = it.*.next) c.alpm_conflict_free(@ptrCast(@alignCast(it.*.data.?)));
    c.alpm_list_free(data);
}

// A local database fixture; writing these files never changes the host database.
pub fn installedFixture(dir: std.fs.Dir, name: []const u8, files: []const u8) !void {
    const path = try std.fmt.allocPrint(u.a, "db/local/{s}-1.0-1", .{name});
    try dir.makePath(path);
    try dir.writeFile(.{ .sub_path = "db/local/ALPM_DB_VERSION", .data = "9\n" });
    try dir.writeFile(.{ .sub_path = try std.fs.path.join(u.a, &.{ path, "desc" }), .data = try std.fmt.allocPrint(u.a, "%NAME%\n{s}\n\n%VERSION%\n1.0-1\n\n%DESC%\nInstalled fixture\n\n%ARCH%\nany\n\n%SIZE%\n1\n\n%REASON%\n1\n\n", .{name}) });
    try dir.writeFile(.{ .sub_path = try std.fs.path.join(u.a, &.{ path, "files" }), .data = try std.fmt.allocPrint(u.a, "%FILES%\n{s}\n\n", .{files}) });
}
test "same-version archives reinstall unless needed is requested" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    try installedFixture(tmp.dir, "zap-reinstall-fixture", "");
    const archive = try fixture(tmp.dir, root, "zap-reinstall-fixture", "");
    const h = try handle(root);
    defer _ = c.alpm_release(h);
    inline for (.{ false, true }) |needed| {
        try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_init(h, @import("transaction.zig").flags(.{ .operation = .install, .needed = needed })));
        try add(h, archive);
        var data: [*c]c.alpm_list_t = null;
        try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_prepare(h, &data));
        try std.testing.expectEqual(@as(usize, if (needed) 0 else 1), c.alpm_list_count(c.alpm_trans_get_add(h)));
        try std.testing.expectEqual(@as(c_int, 0), c.alpm_trans_release(h));
    }
}
