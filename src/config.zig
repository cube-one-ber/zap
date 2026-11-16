//! The subset of pacman.conf that zap honours. Settings zap cannot honour
//! faithfully are rejected instead of silently ignored.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const errors = @import("errors.zig");

const List = std.ArrayList([]const u8);

pub const Repo = struct {
    name: []const u8,
    servers: List = .empty,
    signatures: ?[]const u8 = null,
    usage: []const u8 = "All",
};

pub const Config = struct {
    root: []const u8 = "/",
    db: []const u8 = "/var/lib/pacman/",
    log: []const u8 = "/var/log/pacman.log",
    gpg: []const u8 = "/etc/pacman.d/gnupg/",
    arch: []const u8 = "x86_64",
    signature: []const u8 = "Required DatabaseOptional",
    local_signature: ?[]const u8 = null,
    sandbox_user: ?[]const u8 = null,
    caches: List = .empty,
    hooks: List = .empty,
    ignore: List = .empty,
    ignore_groups: List = .empty,
    hold: List = .empty,
    no_upgrade: List = .empty,
    no_extract: List = .empty,
    repos: std.ArrayList(Repo) = .empty,
    check_space: bool = false,
    parallel: u32 = 5,
    architectures: List = .empty,

    // Parser state.
    db_explicit: bool = false,
    log_explicit: bool = false,
    section: ?usize = null,
    in_options: bool = false,

    pub fn load() !Config {
        var cfg: Config = .{};
        var uts: c.struct_utsname = undefined;
        if (c.uname(&uts) == 0) cfg.arch = try u.a.dupe(u8, std.mem.sliceTo(&uts.machine, 0));
        try cfg.read("/etc/pacman.conf", 0);
        // DBPath and LogFile follow RootDir unless set explicitly.
        if (!cfg.db_explicit) cfg.db = try std.fs.path.join(u.a, &.{ cfg.root, "var/lib/pacman/" });
        if (!cfg.log_explicit) cfg.log = try std.fs.path.join(u.a, &.{ cfg.root, "var/log/pacman.log" });
        if (cfg.architectures.items.len == 0) try cfg.architectures.append(u.a, cfg.arch);
        if (cfg.parallel == 0 or cfg.parallel > 128) return error.InvalidParallelDownloads;
        for ([_][]const u8{ cfg.root, cfg.db, cfg.log, cfg.gpg }) |path| {
            if (!std.fs.path.isAbsolute(path)) return errors.with(error.InvalidConfigurationPath, "{s}", .{path});
        }
        if (cfg.caches.items.len == 0) try cfg.caches.append(u.a, "/var/cache/pacman/pkg/");
        return cfg;
    }

    fn read(self: *Config, path: []const u8, depth: usize) anyerror!void {
        if (depth > 16) return errors.with(error.ConfigurationIncludeCycle, "{s}", .{path});
        const bytes = u.readFile(path, 4 * 1024 * 1024) catch |err| return errors.with(err, "{s}", .{path});
        var lines = std.mem.splitScalar(u8, bytes, '\n');
        while (lines.next()) |raw| {
            const uncommented = raw[0 .. std.mem.indexOfScalar(u8, raw, '#') orelse raw.len];
            const s = std.mem.trim(u8, uncommented, " \r\t");
            if (s.len == 0) continue;
            if (s[0] == '[' and s[s.len - 1] == ']') {
                try self.enter(s[1 .. s.len - 1]);
                continue;
            }
            const equals = std.mem.indexOfScalar(u8, s, '=');
            const key = std.mem.trim(u8, s[0 .. equals orelse s.len], " \t");
            const value = if (equals) |at| std.mem.trim(u8, s[at + 1 ..], " \t") else "";
            if (std.mem.eql(u8, key, "Include")) {
                try self.include(path, value, depth);
            } else try self.option(key, value);
        }
    }

    fn enter(self: *Config, name: []const u8) !void {
        self.in_options = std.mem.eql(u8, name, "options");
        self.section = null;
        if (self.in_options) return;
        if (!u.validName(name)) return errors.with(error.InvalidRepositoryName, "{s}", .{name});
        // A repeated section continues the earlier one and keeps its priority.
        for (self.repos.items, 0..) |existing, i| if (std.mem.eql(u8, existing.name, name)) {
            self.section = i;
            return;
        };
        try self.repos.append(u.a, .{ .name = name });
        self.section = self.repos.items.len - 1;
    }

    fn include(self: *Config, from: []const u8, value: []const u8, depth: usize) !void {
        const pattern = if (std.fs.path.isAbsolute(value)) value else try std.fs.path.join(u.a, &.{ std.fs.path.dirname(from) orelse ".", value });
        var matches: c.glob_t = std.mem.zeroes(c.glob_t);
        const result = c.glob((try u.z(pattern)).ptr, 0, null, &matches);
        defer c.globfree(&matches);
        if (result != 0) return errors.with(error.ConfigurationIncludeFailed, "{s}", .{pattern});
        var i: usize = 0;
        while (i < matches.gl_pathc) : (i += 1) try self.read(u.str(matches.gl_pathv[i]), depth + 1);
    }

    fn option(self: *Config, key: []const u8, value: []const u8) !void {
        if (!self.in_options) {
            const index = self.section orelse return;
            const repo = &self.repos.items[index];
            if (std.mem.eql(u8, key, "Server")) {
                try repo.servers.append(u.a, value);
            } else if (std.mem.eql(u8, key, "SigLevel")) {
                repo.signatures = value;
            } else if (std.mem.eql(u8, key, "Usage")) {
                repo.usage = value;
            }
            return;
        }
        const scalars = .{ .{ "RootDir", "root" }, .{ "DBPath", "db" }, .{ "LogFile", "log" }, .{ "GPGDir", "gpg" }, .{ "SigLevel", "signature" }, .{ "LocalFileSigLevel", "local_signature" } };
        inline for (scalars) |pair| {
            if (std.mem.eql(u8, key, pair[0])) {
                @field(self, pair[1]) = value;
                if (std.mem.eql(u8, key, "DBPath")) self.db_explicit = true;
                if (std.mem.eql(u8, key, "LogFile")) self.log_explicit = true;
                return;
            }
        }
        const lists = .{ .{ "CacheDir", "caches" }, .{ "HookDir", "hooks" }, .{ "IgnorePkg", "ignore" }, .{ "IgnoreGroup", "ignore_groups" }, .{ "HoldPkg", "hold" }, .{ "NoUpgrade", "no_upgrade" }, .{ "NoExtract", "no_extract" } };
        inline for (lists) |pair| {
            if (std.mem.eql(u8, key, pair[0])) {
                var words = std.mem.tokenizeAny(u8, value, " \t");
                while (words.next()) |word| try @field(self, pair[1]).append(u.a, word);
                return;
            }
        }
        if (std.mem.eql(u8, key, "Architecture")) {
            var words = std.mem.tokenizeAny(u8, value, " \t");
            while (words.next()) |word| try self.architectures.append(u.a, if (std.mem.eql(u8, word, "auto")) self.arch else word);
            if (self.architectures.items.len > 0) self.arch = self.architectures.items[0];
        } else if (std.mem.eql(u8, key, "DownloadUser")) {
            self.sandbox_user = value;
        } else if (std.mem.eql(u8, key, "CheckSpace")) {
            self.check_space = true;
        } else if (std.mem.eql(u8, key, "ParallelDownloads")) {
            self.parallel = std.fmt.parseInt(u32, value, 10) catch return error.InvalidParallelDownloads;
        } else if (std.mem.eql(u8, key, "XferCommand") or std.mem.eql(u8, key, "AssumeInstalled")) {
            return errors.with(error.UnsupportedPacmanConfiguration, "{s}", .{key});
        }
    }
};

/// Combine SigLevel tokens with an inherited level. `Package` and `Database`
/// prefixes restrict a token to one kind of signature.
pub fn sigLevel(tokens: []const u8, initial: c_int) !c_int {
    var result = initial;
    var words = std.mem.tokenizeAny(u8, tokens, " \t");
    while (words.next()) |word| {
        var token = word;
        var package = true;
        var database = true;
        if (std.mem.startsWith(u8, token, "Package")) {
            database = false;
            token = token["Package".len..];
        } else if (std.mem.startsWith(u8, token, "Database")) {
            package = false;
            token = token["Database".len..];
        }
        for ([_]bool{ package, database }, 0..) |enabled, kind| {
            if (!enabled) continue;
            const required: c_int = if (kind == 0) c.ALPM_SIG_PACKAGE else c.ALPM_SIG_DATABASE;
            const optional: c_int = if (kind == 0) c.ALPM_SIG_PACKAGE_OPTIONAL else c.ALPM_SIG_DATABASE_OPTIONAL;
            const marginal: c_int = if (kind == 0) c.ALPM_SIG_PACKAGE_MARGINAL_OK else c.ALPM_SIG_DATABASE_MARGINAL_OK;
            const unknown: c_int = if (kind == 0) c.ALPM_SIG_PACKAGE_UNKNOWN_OK else c.ALPM_SIG_DATABASE_UNKNOWN_OK;
            if (std.mem.eql(u8, token, "Never")) {
                result &= ~(required | optional);
            } else if (std.mem.eql(u8, token, "Optional")) {
                result |= required | optional;
            } else if (std.mem.eql(u8, token, "Required")) {
                result |= required;
                result &= ~optional;
            } else if (std.mem.eql(u8, token, "TrustedOnly")) {
                result &= ~(marginal | unknown);
            } else if (std.mem.eql(u8, token, "TrustAll")) {
                result |= marginal | unknown;
            } else return errors.with(error.InvalidSignaturePolicy, "{s}", .{word});
        }
    }
    return result;
}

/// A repository's Usage tokens as libalpm flags.
pub fn usage(tokens: []const u8) !c_int {
    var result: c_int = 0;
    var words = std.mem.tokenizeAny(u8, tokens, " \t");
    while (words.next()) |word| {
        result |= if (std.mem.eql(u8, word, "All"))
            c.ALPM_DB_USAGE_ALL
        else if (std.mem.eql(u8, word, "Sync"))
            c.ALPM_DB_USAGE_SYNC
        else if (std.mem.eql(u8, word, "Search"))
            c.ALPM_DB_USAGE_SEARCH
        else if (std.mem.eql(u8, word, "Install"))
            c.ALPM_DB_USAGE_INSTALL
        else if (std.mem.eql(u8, word, "Upgrade"))
            c.ALPM_DB_USAGE_UPGRADE
        else
            return errors.with(error.InvalidRepositoryUsage, "{s}", .{word});
    }
    return result;
}

test "signature policy preserves independent database and package requirements" {
    const required = try sigLevel("Required DatabaseOptional", 0);
    try std.testing.expect(required & c.ALPM_SIG_PACKAGE != 0);
    try std.testing.expect(required & c.ALPM_SIG_PACKAGE_OPTIONAL == 0);
    try std.testing.expect(required & c.ALPM_SIG_DATABASE_OPTIONAL != 0);
    try std.testing.expectError(error.InvalidSignaturePolicy, sigLevel("Bogus", required));
    const local = try sigLevel("Optional", 0);
    try std.testing.expect(local & c.ALPM_SIG_PACKAGE_OPTIONAL != 0);
}

test "repository usage combines tokens and rejects unknown ones" {
    try std.testing.expectEqual(@as(c_int, c.ALPM_DB_USAGE_ALL), try usage("All"));
    try std.testing.expectEqual(@as(c_int, c.ALPM_DB_USAGE_SYNC | c.ALPM_DB_USAGE_SEARCH), try usage("Sync  Search"));
    try std.testing.expectError(error.InvalidRepositoryUsage, usage("Sync Everything"));
}

test "configuration includes preserve repository order, signatures, and architecture lists" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "mirrors", .data = "Server = https://mirror.example/$repo/os/$arch\n" });
    try tmp.dir.writeFile(.{ .sub_path = "pacman.conf", .data = "[options]\nSigLevel = Required DatabaseOptional\nLocalFileSigLevel = Required\nArchitecture = x86_64 aarch64\nIgnorePkg = protected*\n[priority]\nInclude = mirrors\n[extra]\nServer = https://other.example/$repo/os/$arch\n" });
    const path = try tmp.dir.realpathAlloc(u.a, "pacman.conf");
    defer u.a.free(path);
    var cfg: Config = .{};
    try cfg.read(path, 0);
    try std.testing.expectEqual(@as(usize, 2), cfg.repos.items.len);
    try std.testing.expectEqualStrings("priority", cfg.repos.items[0].name);
    try std.testing.expectEqualStrings("https://mirror.example/$repo/os/$arch", cfg.repos.items[0].servers.items[0]);
    try std.testing.expectEqualStrings("Required", cfg.local_signature.?);
    try std.testing.expectEqual(@as(usize, 2), cfg.architectures.items.len);
    try std.testing.expectEqualStrings("protected*", cfg.ignore.items[0]);
}

test "configuration include cycles and unsafe policy overrides fail explicitly" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    try tmp.dir.writeFile(.{ .sub_path = "loop", .data = "Include = loop\n" });
    const path = try tmp.dir.realpathAlloc(u.a, "loop");
    defer u.a.free(path);
    var cfg: Config = .{ .in_options = true };
    try std.testing.expectError(error.ConfigurationIncludeCycle, cfg.read(path, 0));
    try std.testing.expectError(error.UnsupportedPacmanConfiguration, cfg.option("XferCommand", "sh -c something"));
    try std.testing.expectError(error.UnsupportedPacmanConfiguration, cfg.option("AssumeInstalled", "missing=1"));
    try std.testing.expectEqual(@as(?[]const u8, null), cfg.local_signature);
}
