//! Static .SRCINFO metadata. zap plans builds from it without sourcing the
//! PKGBUILD, then verifies after review that the two agree.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;

pub const Metadata = struct {
    base: []const u8 = "",
    version: []const u8 = "",
    names: std.ArrayList([]const u8) = .empty,
    /// Runtime, make and check dependencies of every package in the base.
    deps: std.ArrayList([]const u8) = .empty,
    provides: std.ArrayList([]const u8) = .empty,
    scoped_provides: std.ArrayList(Provide) = .empty,

    /// A `provides` entry and the split package that declares it; entries in
    /// the pkgbase section apply to every package.
    const Provide = struct { owner: ?[]const u8, spec: []const u8 };

    const dependency_fields = [_][]const u8{ "depends", "makedepends", "checkdepends", "provides" };

    pub fn parse(bytes: []const u8, arch: []const u8) !Metadata {
        var result: Metadata = .{};
        var ver: []const u8 = "";
        var rel: []const u8 = "";
        var epoch: []const u8 = "";
        var current_package: ?[]const u8 = null;
        var lines = std.mem.splitScalar(u8, bytes, '\n');
        while (lines.next()) |line| {
            const s = std.mem.trim(u8, line, " \r\t");
            if (s.len == 0 or s[0] == '#') continue;
            const equals = std.mem.indexOfScalar(u8, s, '=') orelse return error.InvalidSrcInfo;
            const key = std.mem.trim(u8, s[0..equals], " \t");
            const value = std.mem.trim(u8, s[equals + 1 ..], " \t");
            if (std.mem.eql(u8, key, "pkgbase")) {
                result.base = value;
            } else if (std.mem.eql(u8, key, "pkgname")) {
                if (!u.validName(value)) return error.InvalidSrcInfo;
                try u.appendUnique(&result.names, value);
                current_package = value;
            } else if (std.mem.eql(u8, key, "pkgver")) {
                ver = value;
            } else if (std.mem.eql(u8, key, "pkgrel")) {
                rel = value;
            } else if (std.mem.eql(u8, key, "epoch")) {
                epoch = value;
            } else for (dependency_fields) |field| {
                if (!fieldApplies(key, field, arch)) continue;
                if (!u.validName(u.depName(value))) return error.InvalidSrcInfo;
                if (std.mem.eql(u8, field, "provides")) {
                    try u.appendUnique(&result.provides, value);
                    try result.scoped_provides.append(u.a, .{ .owner = current_package, .spec = value });
                } else try u.appendUnique(&result.deps, value);
                break;
            }
        }
        if (!u.validName(result.base) or result.names.items.len == 0 or ver.len == 0 or rel.len == 0) return error.InvalidSrcInfo;
        result.version = if (epoch.len > 0 and !std.mem.eql(u8, epoch, "0"))
            try std.fmt.allocPrint(u.a, "{s}:{s}-{s}", .{ epoch, ver, rel })
        else
            try std.fmt.allocPrint(u.a, "{s}-{s}", .{ ver, rel });
        return result;
    }

    /// Whether `key` is `field` itself or its variant for this architecture,
    /// such as `depends_x86_64`.
    fn fieldApplies(key: []const u8, field: []const u8, arch: []const u8) bool {
        if (std.mem.eql(u8, key, field)) return true;
        return std.mem.startsWith(u8, key, field) and key.len > field.len + 1 and key[field.len] == '_' and std.mem.eql(u8, key[field.len + 1 ..], arch);
    }

    pub fn providesFor(self: Metadata, package: []const u8) ![]const []const u8 {
        var list: std.ArrayList([]const u8) = .empty;
        for (self.scoped_provides.items) |item| {
            if (item.owner == null or std.mem.eql(u8, item.owner.?, package)) try u.appendUnique(&list, item.spec);
        }
        return list.toOwnedSlice(u.a);
    }

    /// Whether `package` from this base satisfies the dependency.
    pub fn matches(self: Metadata, dep: []const u8, package: []const u8) !bool {
        return satisfies(dep, package, self.version, try self.providesFor(package));
    }

    /// The package of this base that satisfies the dependency, if any.
    pub fn provider(self: Metadata, dep: []const u8) !?[]const u8 {
        for (self.names.items) |package| if (try self.matches(dep, package)) return package;
        return null;
    }

    fn sameProviders(self: Metadata, other: Metadata) bool {
        if (self.scoped_provides.items.len != other.scoped_provides.items.len) return false;
        for (self.scoped_provides.items) |item| {
            var found = false;
            for (other.scoped_provides.items) |candidate| {
                const owner_equal = if (item.owner) |owner| candidate.owner != null and std.mem.eql(u8, owner, candidate.owner.?) else candidate.owner == null;
                if (owner_equal and std.mem.eql(u8, item.spec, candidate.spec)) {
                    found = true;
                    break;
                }
            }
            if (!found) return false;
        }
        return true;
    }

    /// Whether two parses describe the same base, version, packages,
    /// dependencies and providers, in any order.
    pub fn equivalent(self: Metadata, other: Metadata) bool {
        return std.mem.eql(u8, self.base, other.base) and
            std.mem.eql(u8, self.version, other.version) and
            setEqual(self.names.items, other.names.items) and
            setEqual(self.deps.items, other.deps.items) and
            setEqual(self.provides.items, other.provides.items) and
            self.sameProviders(other);
    }
};

fn setEqual(a: []const []const u8, b: []const []const u8) bool {
    if (a.len != b.len) return false;
    for (a) |s| if (!u.contains(b, s)) return false;
    return true;
}

/// Whether a package with this name, version and provides list satisfies a
/// possibly versioned dependency. An unversioned provide never satisfies a
/// versioned dependency.
pub fn satisfies(dep: []const u8, name: []const u8, version: []const u8, provides: []const []const u8) !bool {
    if (std.mem.eql(u8, u.depName(dep), name) and try versionMatches(dep, version)) return true;
    for (provides) |provided| {
        if (!std.mem.eql(u8, u.depName(dep), u.depName(provided))) continue;
        if (std.mem.indexOfAny(u8, dep, "<>=") == null) return true;
        if (std.mem.indexOfScalar(u8, provided, '=')) |at| if (try versionMatches(dep, provided[at + 1 ..])) return true;
    }
    return false;
}

fn versionMatches(spec: []const u8, version: []const u8) !bool {
    const dep = c.alpm_dep_from_string((try u.z(spec)).ptr) orelse return error.InvalidDependency;
    defer c.alpm_dep_free(dep);
    if (dep.*.mod == c.ALPM_DEP_MOD_ANY) return true;
    const cmp = c.alpm_pkg_vercmp((try u.z(version)).ptr, dep.*.version);
    return switch (dep.*.mod) {
        c.ALPM_DEP_MOD_EQ => cmp == 0,
        c.ALPM_DEP_MOD_GE => cmp >= 0,
        c.ALPM_DEP_MOD_GT => cmp > 0,
        c.ALPM_DEP_MOD_LE => cmp <= 0,
        c.ALPM_DEP_MOD_LT => cmp < 0,
        else => false,
    };
}

test "SRCINFO supports epochs, split packages and architecture-specific dependencies" {
    const meta = try Metadata.parse("pkgbase = foo\npkgver = 2.0\npkgrel = 1\nepoch = 3\nmakedepends = zig\ndepends_x86_64 = libfoo>=1\ndepends_aarch64 = wrong\npkgname = foo-cli\nprovides = foo=2.0\npkgname = foo-lib\ndepends = libc", "x86_64");
    try std.testing.expectEqualStrings("3:2.0-1", meta.version);
    try std.testing.expectEqual(@as(usize, 2), meta.names.items.len);
    try std.testing.expect(u.contains(meta.deps.items, "libfoo>=1"));
    try std.testing.expect(!u.contains(meta.deps.items, "wrong"));
    try std.testing.expect(try satisfies("foo>=2", "foo-cli", meta.version, meta.provides.items));
    try std.testing.expect(!try satisfies("foo>=3", "foo-cli", meta.version, meta.provides.items));
    try std.testing.expect(!try satisfies("foo>=1", "bar", "5", &.{"foo"}));
}

test "virtual providers stay attached to their split package" {
    const meta = try Metadata.parse("pkgbase = suite\npkgver = 1\npkgrel = 1\npkgname = suite-a\nprovides = virtual-a=1\npkgname = suite-b\nprovides = virtual-b=1", "x86_64");
    try std.testing.expect(try meta.matches("virtual-b>=1", "suite-b"));
    try std.testing.expect(!try meta.matches("virtual-b>=1", "suite-a"));
    try std.testing.expectEqualStrings("suite-b", (try meta.provider("virtual-b")).?);
    try std.testing.expect(try meta.provider("unrelated") == null);
}

test "metadata comparison ignores order and notices every substantive difference" {
    const text = "pkgbase = suite\npkgver = 1\npkgrel = 1\ndepends = a\ndepends = b\npkgname = suite\nprovides = virtual=1\n";
    const original = try Metadata.parse(text, "x86_64");
    const reordered = try Metadata.parse("pkgbase = suite\npkgver = 1\npkgrel = 1\ndepends = b\ndepends = a\npkgname = suite\nprovides = virtual=1\n", "x86_64");
    try std.testing.expect(original.equivalent(reordered));
    for ([_][2][]const u8{ .{ "pkgver = 1", "pkgver = 2" }, .{ "depends = b", "depends = c" }, .{ "virtual=1", "virtual=2" }, .{ "pkgname = suite", "pkgname = other" } }) |change| {
        const altered = try Metadata.parse(try std.mem.replaceOwned(u8, u.a, text, change[0], change[1]), "x86_64");
        try std.testing.expect(!original.equivalent(altered));
    }
}
