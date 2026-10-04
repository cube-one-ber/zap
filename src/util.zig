const std = @import("std");
pub var a: std.mem.Allocator = std.heap.page_allocator;
pub const c = @import("c.zig").c;
pub fn z(s: []const u8) ![:0]const u8 {
    return a.dupeZ(u8, s);
}
pub fn str(s: [*c]const u8) []const u8 {
    return if (s == null) "" else std.mem.span(s);
}
pub fn validName(s: []const u8) bool {
    if (s.len == 0 or s.len > 255 or s[0] == '-' or s[0] == '.') return false;
    for (s) |ch| if (!std.ascii.isAlphanumeric(ch) and std.mem.indexOfScalar(u8, "@._+-", ch) == null) return false;
    return true;
}
pub fn depName(s: []const u8) []const u8 {
    return s[0 .. std.mem.indexOfAny(u8, s, "<>=") orelse s.len];
}
pub fn readFile(path: []const u8, max: usize) ![]u8 {
    return std.fs.cwd().readFileAlloc(a, path, max);
}
pub fn run(args: []const []const u8, cwd: ?[]const u8) !void {
    // Zig's test runner reserves stdout for its protocol.
    if (@import("builtin").is_test) {
        const output = try capture(args, cwd, 16 * 1024 * 1024);
        defer a.free(output);
        std.debug.print("{s}", .{output});
        return;
    }
    var child = std.process.Child.init(args, a);
    child.cwd = cwd;
    var env = try safeEnv();
    defer env.deinit();
    child.env_map = &env;
    const term = try child.spawnAndWait();
    switch (term) {
        .Exited => |code| if (code != 0) return error.CommandFailed,
        else => return error.CommandFailed,
    }
}
pub fn capture(args: []const []const u8, cwd: ?[]const u8, max: usize) ![]u8 {
    var env = try safeEnv();
    defer env.deinit();
    const result = try std.process.Child.run(.{ .allocator = a, .argv = args, .cwd = cwd, .env_map = &env, .max_output_bytes = max });
    defer a.free(result.stderr);
    switch (result.term) {
        .Exited => |code| if (code != 0) {
            std.debug.print("{s}", .{result.stderr});
            a.free(result.stdout);
            return error.CommandFailed;
        },
        else => return error.CommandFailed,
    }
    return result.stdout;
}
fn safeEnv() !std.process.EnvMap {
    var env = std.process.EnvMap.init(a);
    try env.put("PATH", "/usr/bin:/bin");
    for ([_][]const u8{ "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TERM", "COLORTERM", "DISPLAY", "WAYLAND_DISPLAY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK", "MAKEFLAGS" }) |key| {
        if (std.posix.getenv(key)) |value| try env.put(key, value);
    }
    try env.put("GIT_CONFIG_GLOBAL", "/dev/null");
    try env.put("GIT_CONFIG_SYSTEM", "/dev/null");
    try env.put("GIT_TERMINAL_PROMPT", "0");
    return env;
}
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
