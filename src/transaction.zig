const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const signals = @import("signals.zig");
pub const Operation = enum { install, remove, upgrade };
pub const Target = struct { name: []const u8, archive: ?[]const u8 = null, sha256: ?[]const u8 = null, dependency: bool = false };
pub const Request = struct { operation: Operation, targets: []const Target = &.{}, recursive: bool = false, nosave: bool = false };
pub fn escalate(request: Request) !void {
    try validate(request);
    try trustedExecutable();
    if (c.isatty(0) == 0) return error.InteractiveTerminalRequired;
    const json = try std.json.Stringify.valueAlloc(u.a, request, .{});
    if (json.len > 96 * 1024) return error.TransactionTooLarge;
    try u.run(&.{ "/usr/bin/run0", "--pty", "--background=", "--chdir=/", "--", "/usr/bin/zap", "__transaction", json }, null);
}
pub fn trustedExecutable() !void {
    // All components of the executable path must be controlled by root.
    for ([_][]const u8{ "/", "/usr", "/usr/bin", "/usr/bin/zap" }) |path| {
        var st: c.struct_stat = undefined;
        if (c.lstat((try u.z(path)).ptr, &st) != 0) {
            ui.print("Install zap to /usr/bin/zap using the supplied PKGBUILD before making system changes.\n", .{});
            return error.WorkerNotInstalled;
        }
        if (st.st_uid != 0 or st.st_mode & 0o022 != 0 or st.st_mode & c.S_IFMT == c.S_IFLNK) return error.UntrustedWorkerPath;
    }
}
pub fn validate(request: Request) !void {
    if (request.operation != .remove and (request.recursive or request.nosave)) return error.InvalidTransactionFlags;
    if (request.targets.len > 1024) return error.TransactionTooLarge;
    for (request.targets, 0..) |target, i| {
        if (!u.validName(target.name)) return error.InvalidPackageName;
        for (request.targets[0..i]) |previous| if (std.mem.eql(u8, target.name, previous.name)) return error.DuplicateTarget;
        if (target.archive) |path| {
            if (request.operation != .install or !std.fs.path.isAbsolute(path) or !std.mem.startsWith(u8, std.fs.path.basename(path), target.name)) return error.InvalidArchive;
            const hash = target.sha256 orelse return error.MissingDigest;
            if (hash.len != 64) return error.InvalidDigest;
            for (hash) |ch| if (!std.ascii.isHex(ch)) return error.InvalidDigest;
        } else if (target.sha256 != null) return error.InvalidDigest;
    }
    if (request.operation != .upgrade and request.targets.len == 0) return error.NoTargets;
}
pub fn hashFile(path: []const u8) ![]const u8 {
    return copyHash(path, null, 4 * 1024 * 1024 * 1024, false);
}
pub fn hashSourceFile(path: []const u8) ![]const u8 {
    return copyHash(path, null, 16 * 1024 * 1024, true);
}
fn copyHash(path: []const u8, dest: ?[]const u8, limit: u64, allow_empty: bool) ![]const u8 {
    const fd = c.open((try u.z(path)).ptr, @as(c_int, c.O_RDONLY | c.O_CLOEXEC | c.O_NOFOLLOW | c.O_NONBLOCK));
    if (fd < 0) return error.UnsafeArchive;
    defer _ = c.close(fd);
    var st: c.struct_stat = undefined;
    if (c.fstat(fd, &st) != 0 or st.st_mode & c.S_IFMT != c.S_IFREG or st.st_size < 0 or (!allow_empty and st.st_size == 0) or st.st_size > limit) return error.UnsafeArchive;
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
        if (total > limit) return error.ArchiveTooLarge;
        hash.update(buf[0..n]);
        if (out) |file| try file.writeAll(buf[0..n]);
    }
    if (total != st.st_size) return error.ArchiveChanged;
    const digest = hash.finalResult();
    return std.fmt.allocPrint(u.a, "{s}", .{std.fmt.bytesToHex(digest, .lower)});
}
pub fn worker(bytes: []const u8) !void {
    if (c.geteuid() != 0 or c.getuid() != 0) return error.RootWorkerRequired;
    if (c.isatty(0) == 0) return error.InteractiveTerminalRequired;
    if (bytes.len > 96 * 1024) return error.TransactionTooLarge;
    const parsed = try std.json.parseFromSlice(Request, u.a, bytes, .{});
    const request = parsed.value;
    signals.install();
    try validate(request);
    var db = try alpm.Alpm.init();
    defer db.deinit();
    const template = try u.a.dupeZ(u8, "/var/tmp/zap-transaction-XXXXXX");
    if (c.mkdtemp(template.ptr) == null) return error.StagingFailed;
    const staging = std.mem.sliceTo(template, 0);
    defer std.fs.cwd().deleteTree(staging) catch {};
    // Snapshot before parsing packages; user-writable artifacts are never used by commit.
    var archives: std.ArrayList(?[]const u8) = .empty;
    for (request.targets, 0..) |target, i| {
        var snapshot: ?[]const u8 = null;
        if (target.archive) |path| {
            snapshot = try std.fmt.allocPrint(u.a, "{s}/{d}.pkg.tar", .{ staging, i });
            const actual = try copyHash(path, snapshot, 4 * 1024 * 1024 * 1024, false);
            if (!std.ascii.eqlIgnoreCase(actual, target.sha256.?)) return error.ArchiveDigestMismatch;
            const sig_source = try std.fmt.allocPrint(u.a, "{s}.sig", .{path});
            var st: c.struct_stat = undefined;
            if (c.lstat((try u.z(sig_source)).ptr, &st) == 0) {
                _ = try copyHash(sig_source, try std.fmt.allocPrint(u.a, "{s}.sig", .{snapshot.?}), 64 * 1024, false);
            }
        }
        try archives.append(u.a, snapshot);
    }
    try signals.check();
    if (request.operation == .upgrade) try db.refresh();
    try signals.check();
    // Acquire the normal ALPM lock. Never delete another process's lock.
    try db.check(c.alpm_trans_init(db.h, c.ALPM_TRANS_FLAG_NEEDED | (if (request.operation == .remove and request.recursive) @as(c_int, c.ALPM_TRANS_FLAG_RECURSE) else @as(c_int, 0)) | (if (request.nosave) @as(c_int, c.ALPM_TRANS_FLAG_NOSAVE) else @as(c_int, 0))));
    defer _ = c.alpm_trans_release(db.h);
    if (request.operation == .upgrade) try db.check(c.alpm_sync_sysupgrade(db.h, 0));
    var old_explicit: std.StringHashMap(void) = .init(u.a);
    var local_it = db.installed();
    while (local_it != null) : (local_it = local_it.*.next) {
        const p = alpm.pkg(local_it.*.data);
        if (c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_EXPLICIT) try old_explicit.put(try u.a.dupe(u8, alpm.name(p)), {});
    }
    for (request.targets, 0..) |target, i| {
        if (request.operation == .remove) {
            const p = c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(target.name)).ptr) orelse return error.PackageNotInstalled;
            try db.check(c.alpm_remove_pkg(db.h, p));
        } else {
            var p: ?*c.alpm_pkg_t = null;
            if (archives.items[i]) |path| {
                try db.check(c.alpm_pkg_load(db.h, (try u.z(path)).ptr, 1, c.alpm_option_get_local_file_siglevel(db.h), &p));
                if (!std.mem.eql(u8, alpm.name(p.?), target.name)) {
                    _ = c.alpm_pkg_free(p);
                    return error.ArchiveNameMismatch;
                }
            } else p = try db.repo(target.name) orelse return error.RepositoryPackageNotFound;
            const result = c.alpm_add_pkg(db.h, p.?);
            if (result < 0) {
                if (archives.items[i] != null) _ = c.alpm_pkg_free(p);
                try db.check(result);
            }
        }
    }
    var data: [*c]c.alpm_list_t = null;
    if (c.alpm_trans_prepare(db.h, &data) < 0) {
        db.errors(data);
        try db.check(-1);
    }
    try signals.check();
    const additions = c.alpm_trans_get_add(db.h);
    const removals = c.alpm_trans_get_remove(db.h);
    var promotions: std.ArrayList([]const u8) = .empty;
    if (request.operation != .remove) for (request.targets) |target| {
        if (!target.dependency) if (c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(target.name)).ptr)) |p| {
            if (c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_DEPEND) try promotions.append(u.a, target.name);
        };
    };
    if (additions == null and removals == null) {
        if (promotions.items.len == 0) {
            ui.print("Everything is up to date.\n", .{});
            return;
        }
        ui.title("Installation reasons");
        for (promotions.items) |n| ui.print("  Mark {s} as explicitly installed\n", .{ui.safe(n)});
        try ui.require("Apply these installation reason changes? [y/N] ");
        try signals.check();
        for (promotions.items) |n| try db.check(c.alpm_pkg_set_reason(c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(n)).ptr), c.ALPM_PKG_REASON_EXPLICIT));
        ui.print("Installation reasons updated.\n", .{});
        return;
    }
    ui.title("System transaction");
    for (promotions.items) |n| ui.print("  Mark {s} as explicitly installed\n", .{ui.safe(n)});
    var size: i64 = 0;
    var it = additions;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        size += c.alpm_pkg_get_isize(p);
        const old = c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), c.alpm_pkg_get_name(p));
        if (old) |previous| size -= c.alpm_pkg_get_isize(previous);
        ui.print("  + {s} {s} → {s}\n", .{ ui.safe(alpm.name(p)), if (old) |previous| ui.safe(alpm.version(previous)) else "new", ui.safe(alpm.version(p)) });
    }
    it = removals;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        size -= c.alpm_pkg_get_isize(p);
        ui.print("  - {s} {s}\n", .{ ui.safe(alpm.name(p)), ui.safe(alpm.version(p)) });
        for (db.cfg.hold.items) |held| if (c.fnmatch((try u.z(held)).ptr, c.alpm_pkg_get_name(p), 0) == 0) {
            ui.print("Protected by HoldPkg: {s}\n", .{ui.safe(held)});
            return error.ProtectedPackage;
        };
    }
    ui.print("\n  Installed size change: {d} MiB\n", .{@divTrunc(size, 1024 * 1024)});
    try ui.require("\nCommit this transaction, including package scripts and hooks? [y/N] ");
    try signals.check();
    data = null;
    if (c.alpm_trans_commit(db.h, &data) < 0) {
        db.errors(data);
        try db.check(-1);
    }
    if (request.operation != .remove) for (request.targets) |target| {
        if (c.alpm_db_get_pkg(c.alpm_get_localdb(db.h), (try u.z(target.name)).ptr)) |p| {
            const reason: c.alpm_pkgreason_t = if (!target.dependency or old_explicit.contains(target.name)) c.ALPM_PKG_REASON_EXPLICIT else c.ALPM_PKG_REASON_DEPEND;
            try db.check(c.alpm_pkg_set_reason(p, reason));
        }
    };
    ui.print("\nTransaction complete.\n", .{});
}
test "worker rejects unsafe manifests" {
    try std.testing.expectError(error.InvalidPackageName, validate(.{ .operation = .install, .targets = &.{.{ .name = "../bad" }} }));
    try std.testing.expectError(error.InvalidDigest, validate(.{ .operation = .install, .targets = &.{.{ .name = "foo", .archive = "/tmp/foo.pkg.tar.zst", .sha256 = "short" }} }));
    try std.testing.expectError(error.DuplicateTarget, validate(.{ .operation = .remove, .targets = &.{ .{ .name = "foo" }, .{ .name = "foo" } } }));
}
test "snapshot hash rejects symlinks and detects changed content" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "pkg", .data = "package bytes" });
    const path = try tmp.dir.realpathAlloc(u.a, "pkg");
    defer u.a.free(path);
    const hash = try hashFile(path);
    defer u.a.free(hash);
    try std.testing.expectEqual(@as(usize, 64), hash.len);
    try tmp.dir.symLink("pkg", "link", .{});
    const link = try std.fs.path.join(u.a, &.{ std.fs.path.dirname(path).?, "link" });
    defer u.a.free(link);
    try std.testing.expectError(error.UnsafeArchive, hashFile(link));
    try tmp.dir.writeFile(.{ .sub_path = "pkg", .data = "different bytes" });
    const snapshot = try std.fs.path.join(u.a, &.{ std.fs.path.dirname(path).?, "snapshot" });
    defer u.a.free(snapshot);
    const snapshot_hash = try copyHash(path, snapshot, 1024, false);
    defer u.a.free(snapshot_hash);
    try tmp.dir.writeFile(.{ .sub_path = "pkg", .data = "changed again" });
    const retained = try hashFile(snapshot);
    defer u.a.free(retained);
    try std.testing.expectEqualStrings(snapshot_hash, retained);
    const fifo = try std.fs.path.join(u.a, &.{ std.fs.path.dirname(path).?, "fifo" });
    defer u.a.free(fifo);
    try std.testing.expectEqual(@as(c_int, 0), c.mkfifo((try u.z(fifo)).ptr, 0o600));
    try std.testing.expectError(error.UnsafeArchive, hashFile(fifo));
    const changed = try hashFile(path);
    defer u.a.free(changed);
    try std.testing.expect(!std.mem.eql(u8, changed, hash));
}
