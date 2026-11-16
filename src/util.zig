//! Shared primitives: the process allocator, C interop, name validation and
//! child processes with an allowlisted environment.
const std = @import("std");
const builtin = @import("builtin");
const ui = @import("ui.zig");
const errors = @import("errors.zig");

pub const c = @import("c.zig").c;
pub const version: []const u8 = @import("build_options").version;

/// zap is a short-lived process: `main` points this at one arena and nothing
/// is freed individually. Tests keep the page allocator.
pub var a: std.mem.Allocator = std.heap.page_allocator;

pub fn z(s: []const u8) ![:0]const u8 {
    return a.dupeZ(u8, s);
}

pub fn str(s: [*c]const u8) []const u8 {
    return if (s == null) "" else std.mem.span(s);
}

/// Package, base and repository names. A valid name cannot be mistaken for an
/// option or escape the directory it is joined to.
pub fn validName(s: []const u8) bool {
    if (s.len == 0 or s.len > 255 or s[0] == '-' or s[0] == '.') return false;
    for (s) |ch| if (!std.ascii.isAlphanumeric(ch) and std.mem.indexOfScalar(u8, "@._+-", ch) == null) return false;
    return true;
}

/// The name part of a dependency such as `foo>=2`.
pub fn depName(s: []const u8) []const u8 {
    return s[0 .. std.mem.indexOfAny(u8, s, "<>=") orelse s.len];
}

pub fn contains(list: []const []const u8, value: []const u8) bool {
    for (list) |item| if (std.mem.eql(u8, item, value)) return true;
    return false;
}

pub fn appendUnique(list: *std.ArrayList([]const u8), value: []const u8) !void {
    if (!contains(list.items, value)) try list.append(a, value);
}

pub fn readFile(path: []const u8, max: usize) ![]u8 {
    return std.fs.cwd().readFileAlloc(a, path, max);
}

/// Run a command attached to the terminal.
pub fn run(args: []const []const u8, cwd: ?[]const u8) !void {
    if (try runStatus(args, cwd) != 0) return failed(args);
}

/// Run a command attached to the terminal and return its exit status. A
/// command ended by a signal reports 128 plus the signal number, as shells do.
pub fn runStatus(args: []const []const u8, cwd: ?[]const u8) !u8 {
    var env = try safeEnv();
    defer env.deinit();
    // Zig's test runner reserves standard output for its protocol.
    if (builtin.is_test) {
        const result = try std.process.Child.run(.{ .allocator = a, .argv = args, .cwd = cwd, .env_map = &env, .max_output_bytes = 16 * 1024 * 1024 });
        std.debug.print("{s}{s}", .{ result.stdout, result.stderr });
        return exitStatus(result.term);
    }
    ui.flush();
    var child = std.process.Child.init(args, a);
    child.cwd = cwd;
    child.env_map = &env;
    return exitStatus(try child.spawnAndWait());
}

fn exitStatus(term: std.process.Child.Term) u8 {
    return switch (term) {
        .Exited => |code| code,
        .Signal => |signal| @truncate(128 +| signal),
        else => 255,
    };
}

/// Run a command and return its standard output. Standard error is shown,
/// sanitized, only when the command fails.
pub fn capture(args: []const []const u8, cwd: ?[]const u8, max: usize) ![]u8 {
    ui.flush();
    var env = try safeEnv();
    defer env.deinit();
    const result = try std.process.Child.run(.{ .allocator = a, .argv = args, .cwd = cwd, .env_map = &env, .max_output_bytes = max });
    defer a.free(result.stderr);
    const ok = switch (result.term) {
        .Exited => |status| status == 0,
        else => false,
    };
    if (ok) return result.stdout;
    a.free(result.stdout);
    // Tool output can quote remote or package-controlled text.
    var lines = std.mem.tokenizeScalar(u8, result.stderr, '\n');
    while (lines.next()) |line| ui.eprint("    {s}\n", .{ui.safe(line)});
    return failed(args);
}

fn failed(args: []const []const u8) error{CommandFailed} {
    return errors.with(error.CommandFailed, "{s}", .{commandLabel(args)});
}

/// The tool a command line runs, looking through the sandbox wrapper and
/// including the subcommand of multi-purpose tools.
fn commandLabel(args: []const []const u8) []const u8 {
    var rest = args;
    if (rest.len > 0 and std.mem.eql(u8, std.fs.path.basename(rest[0]), "bwrap")) {
        for (rest, 0..) |arg, i| if (std.mem.eql(u8, arg, "--")) {
            rest = rest[i + 1 ..];
            break;
        };
    }
    if (rest.len == 0) return "command";
    const tool = std.fs.path.basename(rest[0]);
    if (!std.mem.eql(u8, tool, "git")) return tool;
    // Skip `-c key=value` pairs to reach the git subcommand.
    var i: usize = 1;
    while (i + 1 < rest.len and std.mem.eql(u8, rest[i], "-c")) i += 2;
    if (i >= rest.len) return tool;
    return std.fmt.allocPrint(a, "git {s}", .{rest[i]}) catch tool;
}

/// Children never inherit the caller's full environment.
fn safeEnv() !std.process.EnvMap {
    var env = std.process.EnvMap.init(a);
    try env.put("PATH", "/usr/bin:/bin");
    const inherited = [_][]const u8{
        "HOME",    "USER",            "LOGNAME",         "LANG",                     "LC_ALL",        "TERM",      "COLORTERM",
        "DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK", "MAKEFLAGS",
    };
    for (inherited) |key| {
        if (std.posix.getenv(key)) |value| try env.put(key, value);
    }
    try env.put("GIT_CONFIG_GLOBAL", "/dev/null");
    try env.put("GIT_CONFIG_SYSTEM", "/dev/null");
    try env.put("GIT_TERMINAL_PROMPT", "0");
    return env;
}

/// Create a private directory, refusing one owned by another user or
/// replaced by a symlink.
pub fn secureDir(path: []const u8) !void {
    try std.fs.cwd().makePath(path);
    var st: c.struct_stat = undefined;
    if (c.lstat((try z(path)).ptr, &st) != 0 or st.st_uid != c.getuid() or st.st_mode & c.S_IFMT != c.S_IFDIR) return error.UnsafeCacheDirectory;
    if (c.chmod((try z(path)).ptr, 0o700) != 0) return error.UnsafeCacheDirectory;
}

test "package names cannot become flags or paths" {
    for ([_][]const u8{ "", "-oops", "../etc", ".hidden", "foo/bar", "x\n", "a;id", "a b" }) |name| try std.testing.expect(!validName(name));
    try std.testing.expect(validName("libfoo-git"));
    try std.testing.expectEqualStrings("foo", depName("foo>=2:1.0"));
}

test "failed commands are named by the tool the user would recognise" {
    try std.testing.expectEqualStrings("git fetch", commandLabel(&.{ "/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-c", "a=b", "fetch", "--", "url" }));
    try std.testing.expectEqualStrings("makepkg", commandLabel(&.{ "/usr/bin/bwrap", "--unshare-all", "--", "/usr/bin/makepkg", "--printsrcinfo" }));
    try std.testing.expectEqualStrings("run0", commandLabel(&.{ "/usr/bin/run0", "--pty" }));
    try std.testing.expectEqualStrings("command", commandLabel(&.{}));
}
