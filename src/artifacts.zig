//! Built package archives. makepkg predicts the paths it will write; zap
//! trusts none of that and identifies each archive by its own metadata.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const alpm = @import("alpm.zig");
const tx = @import("transaction.zig");
const errors = @import("errors.zig");

pub const Options = struct {
    base: []const u8,
    dir: []const u8,
    /// The packages of this base that the plan needs, in installation order.
    wanted: []const []const u8,
    explicit_names: []const []const u8 = &.{},
    /// Keep the reason an already installed package has (upgrades).
    preserve_reasons: bool = false,
};

pub const Result = struct {
    targets: []tx.Target,
    versions: []const []const u8,
};

const Entry = struct { target: tx.Target, version: []const u8 };

/// makepkg predicts package paths, including optional debug archives. Only the
/// automatic debug output may be absent. Validate actual metadata and exact
/// required-name coverage before returning targets in the requested order.
pub fn collect(db: *alpm.Alpm, options: Options, paths: []const u8) !Result {
    if (options.wanted.len == 0) return error.NoTargets;
    for (options.wanted, 0..) |name, i| {
        if (!u.validName(name)) return errors.with(error.InvalidPackageName, "{s}", .{name});
        for (options.wanted[0..i]) |previous| if (std.mem.eql(u8, name, previous)) return errors.with(error.DuplicateTarget, "{s}", .{name});
    }
    const debug_name = try std.fmt.allocPrint(u.a, "{s}-debug", .{options.base});
    const debug_prefix = try std.fmt.allocPrint(u.a, "{s}-", .{debug_name});
    var entries: std.ArrayList(Entry) = .empty;
    var by_name = std.StringHashMap(usize).init(u.a);
    var lines = std.mem.tokenizeScalar(u8, paths, '\n');
    while (lines.next()) |line| {
        const path = try std.fs.path.resolve(u.a, &.{ options.dir, line });
        _ = std.posix.fstatat(std.posix.AT.FDCWD, path, std.posix.AT.SYMLINK_NOFOLLOW) catch |err| {
            // Only the automatic debug package may be predicted but absent.
            const filename = std.fs.path.basename(path);
            const optional_debug = !u.contains(options.wanted, debug_name) and
                std.mem.startsWith(u8, filename, debug_prefix) and std.mem.indexOf(u8, filename, ".pkg.tar") != null;
            if (err == error.FileNotFound and optional_debug) continue;
            return errors.with(if (err == error.FileNotFound) error.MissingBuildArtifacts else err, "{s}", .{path});
        };
        const digest = try tx.hashFile(path);
        var package: ?*c.alpm_pkg_t = null;
        db.check(c.alpm_pkg_load(db.h, (try u.z(path)).ptr, 1, 0, &package)) catch |err| {
            return if (err == error.AlpmOperationFailed) errors.with(error.InvalidBuildArchive, "{s}", .{path}) else err;
        };
        defer _ = c.alpm_pkg_free(package);
        const name = alpm.name(package.?);
        if (!u.contains(options.wanted, name)) continue;
        if (by_name.contains(name)) return errors.with(error.DuplicateBuildArtifact, "{s} ({s})", .{ name, path });
        const declared_base = u.str(c.alpm_pkg_get_base(package));
        const base = if (declared_base.len == 0) name else declared_base;
        if (!std.mem.eql(u8, base, options.base)) {
            return errors.with(error.ArchiveBaseMismatch, "{s} belongs to {s}, expected {s}", .{ path, base, options.base });
        }
        const requested = u.contains(options.explicit_names, name);
        const dependency = if (options.preserve_reasons)
            (if (try db.local(name)) |old| !alpm.explicit(old) else !requested)
        else
            !requested;
        const owned_name = try u.a.dupe(u8, name);
        try by_name.put(owned_name, entries.items.len);
        try entries.append(u.a, .{
            .target = .{ .name = owned_name, .archive = path, .sha256 = digest, .dependency = dependency },
            .version = try u.a.dupe(u8, alpm.version(package.?)),
        });
    }
    var missing: std.ArrayList([]const u8) = .empty;
    for (options.wanted) |name| if (!by_name.contains(name)) try missing.append(u.a, name);
    if (missing.items.len > 0) {
        return errors.with(error.MissingBuildArtifacts, "{s} did not produce {s}", .{ options.base, try std.mem.join(u.a, ", ", missing.items) });
    }
    const targets = try u.a.alloc(tx.Target, options.wanted.len);
    const versions = try u.a.alloc([]const u8, options.wanted.len);
    for (options.wanted, targets, versions) |name, *target, *version| {
        const entry = entries.items[by_name.get(name).?];
        target.* = entry.target;
        version.* = entry.version;
    }
    return .{ .targets = targets, .versions = versions };
}

const Fixture = struct {
    tmp: std.testing.TmpDir,
    root: []u8,
    db: alpm.Alpm,
    fn init() !Fixture {
        var tmp = std.testing.tmpDir(.{});
        errdefer tmp.cleanup();
        const root = try tmp.dir.realpathAlloc(u.a, ".");
        return .{ .tmp = tmp, .root = root, .db = .{ .h = try @import("native_tests.zig").handle(root), .cfg = .{} } };
    }
    fn deinit(self: *Fixture) void {
        self.db.deinit();
        self.tmp.cleanup();
    }
    fn archive(self: *Fixture, name: []const u8, base: []const u8) ![]const u8 {
        return @import("native_tests.zig").fixture(self.tmp.dir, self.root, name, try std.fmt.allocPrint(u.a, "pkgbase = {s}\n", .{base}));
    }
    fn options(self: *Fixture, wanted: []const []const u8) Options {
        return .{ .base = "suite", .dir = self.root, .wanted = wanted, .explicit_names = &.{"alpha"} };
    }
};

test "artifact selection uses exact names, requested order, and optional debug predictions" {
    var f = try Fixture.init();
    defer f.deinit();
    const alpha = try f.archive("alpha", "suite");
    const beta = try f.archive("beta", "suite");
    const paths = try std.fmt.allocPrint(u.a, "{s}\n{s}\n{s}/suite-debug-1.0-1-any.pkg.tar.zst\n", .{ beta, alpha, f.root });
    const result = try collect(&f.db, f.options(&.{ "alpha", "beta" }), paths);
    try std.testing.expectEqualStrings("alpha", result.targets[0].name);
    try std.testing.expectEqualStrings("beta", result.targets[1].name);
    try std.testing.expect(!result.targets[0].dependency);
    try std.testing.expect(result.targets[1].dependency);
    try std.testing.expectEqualStrings("1.0-1", result.versions[0]);
    try std.testing.expectEqual(@as(usize, 64), result.targets[0].sha256.?.len);
}
test "missing required packages and non-debug predicted files fail" {
    var f = try Fixture.init();
    defer f.deinit();
    const alpha = try f.archive("alpha", "suite");
    try std.testing.expectError(error.MissingBuildArtifacts, collect(&f.db, f.options(&.{ "alpha", "beta" }), alpha));
    const missing = try std.fmt.allocPrint(u.a, "{s}/beta-1.0-1-any.pkg.tar", .{f.root});
    try std.testing.expectError(error.MissingBuildArtifacts, collect(&f.db, f.options(&.{"beta"}), missing));
    const debug = try std.fmt.allocPrint(u.a, "{s}/suite-debug-1.0-1-any.pkg.tar", .{f.root});
    try std.testing.expectError(error.MissingBuildArtifacts, collect(&f.db, f.options(&.{"suite-debug"}), debug));
}
test "duplicate metadata cannot conceal a missing split package" {
    var f = try Fixture.init();
    defer f.deinit();
    const alpha = try f.archive("alpha", "suite");
    const repeated = try std.fmt.allocPrint(u.a, "{s}\n{s}", .{ alpha, alpha });
    try std.testing.expectError(error.DuplicateBuildArtifact, collect(&f.db, f.options(&.{ "alpha", "beta" }), repeated));
    try f.tmp.dir.copyFile(std.fs.path.basename(alpha), f.tmp.dir, "alias.pkg.tar", .{});
    const aliases = try std.fmt.allocPrint(u.a, "{s}\nalias.pkg.tar", .{alpha});
    try std.testing.expectError(error.DuplicateBuildArtifact, collect(&f.db, f.options(&.{ "alpha", "beta" }), aliases));
}
test "symlinks and special files are rejected, including optional debug outputs" {
    var f = try Fixture.init();
    defer f.deinit();
    const alpha = try f.archive("alpha", "suite");
    try f.tmp.dir.symLink(std.fs.path.basename(alpha), "suite-debug-1.0-1-any.pkg.tar", .{});
    const paths = try std.fmt.allocPrint(u.a, "{s}\nsuite-debug-1.0-1-any.pkg.tar", .{alpha});
    try std.testing.expectError(error.UnsafeArchive, collect(&f.db, f.options(&.{"alpha"}), paths));
    const fifo = try std.fs.path.join(u.a, &.{ f.root, "pipe.pkg.tar" });
    try std.testing.expectEqual(@as(c_int, 0), c.mkfifo((try u.z(fifo)).ptr, 0o600));
    try std.testing.expectError(error.UnsafeArchive, collect(&f.db, f.options(&.{"alpha"}), fifo));
}
test "corrupt archives and mismatched package bases fail before returning targets" {
    var f = try Fixture.init();
    defer f.deinit();
    try f.tmp.dir.writeFile(.{ .sub_path = "corrupt.pkg.tar", .data = "not a package archive" });
    try std.testing.expectError(error.InvalidBuildArchive, collect(&f.db, f.options(&.{"alpha"}), "corrupt.pkg.tar"));
    const wrong = try f.archive("alpha", "different-base");
    try std.testing.expectError(error.ArchiveBaseMismatch, collect(&f.db, f.options(&.{"alpha"}), wrong));
}
test "existing debug packages are selected only when explicitly required" {
    var f = try Fixture.init();
    defer f.deinit();
    const alpha = try f.archive("alpha", "suite");
    const debug = try f.archive("suite-debug", "suite");
    const paths = try std.fmt.allocPrint(u.a, "{s}\n{s}", .{ alpha, debug });
    const regular = try collect(&f.db, f.options(&.{"alpha"}), paths);
    try std.testing.expectEqual(@as(usize, 1), regular.targets.len);
    const both = try collect(&f.db, f.options(&.{ "alpha", "suite-debug" }), paths);
    try std.testing.expectEqual(@as(usize, 2), both.targets.len);
}
test "artifact paths preserve spaces and resolve relative package destinations" {
    var f = try Fixture.init();
    defer f.deinit();
    const alpha = try f.archive("alpha", "suite");
    try f.tmp.dir.makeDir("package output");
    try f.tmp.dir.copyFile(std.fs.path.basename(alpha), f.tmp.dir, "package output/alpha.pkg.tar", .{});
    const result = try collect(&f.db, f.options(&.{"alpha"}), "./package output/alpha.pkg.tar\n");
    try std.testing.expectEqualStrings("alpha", result.targets[0].name);
    try std.testing.expect(std.fs.path.isAbsolute(result.targets[0].archive.?));
    try std.testing.expect(std.mem.indexOf(u8, result.targets[0].archive.?, "package output") != null);
}

test "duplicate or invalid required names cannot produce ambiguous owned results" {
    var f = try Fixture.init();
    defer f.deinit();
    try std.testing.expectError(error.DuplicateTarget, collect(&f.db, f.options(&.{ "alpha", "alpha" }), ""));
    try std.testing.expectError(error.InvalidPackageName, collect(&f.db, f.options(&.{"../alpha"}), ""));
    try std.testing.expectError(error.NoTargets, collect(&f.db, f.options(&.{}), ""));
}
