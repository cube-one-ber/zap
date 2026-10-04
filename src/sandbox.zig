const std = @import("std");
const u = @import("util.zig");

// A fresh home and environment for every invocation, including metadata queries.
// Only the package checkout is writable on the host. Network access is retained
// for source downloads; this is filesystem isolation, not a clean build chroot.
pub fn argv(dir: []const u8, database: []const u8, command: []const []const u8) ![]const []const u8 {
    const real = try std.fs.cwd().realpathAlloc(u.a, dir);
    if (!std.mem.eql(u8, real, dir)) return error.UnsafeSandboxDirectory;
    if (std.mem.eql(u8, dir, "/") or std.mem.eql(u8, dir, "/tmp") or std.mem.eql(u8, dir, "/home") or
        under(dir, "/usr") or under(dir, "/etc") or under(dir, "/var") or under(dir, "/proc") or under(dir, "/dev") or under(dir, "/run")) return error.UnsafeSandboxDirectory;
    if (!std.fs.path.isAbsolute(database)) return error.UnsafeSandboxDirectory;
    const database_real = try std.fs.cwd().realpathAlloc(u.a, database);
    if (under(database_real, dir) or under(dir, database_real) or
        std.mem.eql(u8, database_real, "/") or std.mem.eql(u8, database_real, "/home") or std.mem.eql(u8, database_real, "/tmp")) return error.UnsafeSandboxDirectory;
    try std.fs.cwd().access("/usr/bin/bwrap", .{});
    var args: std.ArrayList([]const u8) = .empty;
    try args.appendSlice(u.a, &.{
        "/usr/bin/bwrap", "--unshare-all",       "--share-net",       "--die-with-parent", "--new-session", "--cap-drop",        "ALL",
        "--ro-bind",      "/usr",                "/usr",              "--symlink",         "usr/bin",       "/bin",              "--symlink",
        "usr/bin",        "/sbin",               "--symlink",         "usr/lib",           "/lib",          "--symlink",         "usr/lib",
        "/lib64",         "--ro-bind",           "/etc",              "/etc",              "--proc",        "/proc",             "--dev",
        "/dev",           "--tmpfs",             "/tmp",              "--dir",             "/tmp/zap-home", "--ro-bind",         database_real,
        database,         "--bind",              dir,                 dir,                 "--chdir",       dir,                 "--clearenv",
        "--setenv",       "PATH",                "/usr/bin:/bin",     "--setenv",          "HOME",          "/tmp/zap-home",     "--setenv",
        "LANG",           "C.UTF-8",             "--setenv",          "USER",              "zap",           "--setenv",          "LOGNAME",
        "zap",            "--setenv",            "GIT_CONFIG_GLOBAL", "/dev/null",         "--setenv",      "GIT_CONFIG_SYSTEM", "/dev/null",
        "--setenv",       "GIT_TERMINAL_PROMPT", "0",
    });
    const git_dir = try std.fs.path.join(u.a, &.{ dir, ".git" });
    if (std.fs.cwd().access(git_dir, .{})) |_| {
        const git_real = try std.fs.cwd().realpathAlloc(u.a, git_dir);
        if (!std.mem.eql(u8, git_real, git_dir)) return error.UnsafeSandboxDirectory;
        const st = try std.fs.cwd().statFile(git_dir);
        if (st.kind != .directory) return error.UnsafeSandboxDirectory;
        try args.appendSlice(u.a, &.{ "--ro-bind", git_dir, git_dir });
    } else |err| if (err != error.FileNotFound) return err;
    try args.append(u.a, "--");
    try args.appendSlice(u.a, command);
    return args.toOwnedSlice(u.a);
}
fn under(path: []const u8, parent: []const u8) bool {
    return std.mem.eql(u8, path, parent) or (std.mem.startsWith(u8, path, parent) and path.len > parent.len and path[parent.len] == '/');
}
test "sandbox blocks host home writes, hides secrets, and retains build outputs" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const build = try std.fs.path.join(u.a, &.{ root, "build" });
    const database = try std.fs.path.join(u.a, &.{ root, "db" });
    try tmp.dir.makeDir("build");
    try tmp.dir.makeDir("db");
    try tmp.dir.writeFile(.{ .sub_path = "secret", .data = "private" });
    try tmp.dir.writeFile(.{ .sub_path = "db/record", .data = "read only" });
    // These fixed shell instructions are test fixtures, never user interpolation.
    const script = try std.fmt.allocPrint(u.a, "test ! -e '{s}/secret' && test -z \"$SSH_AUTH_SOCK\" && test -z \"$DISPLAY\" && test ! -e /run/user && ! touch '{s}/record' 2>/dev/null && touch output && echo isolated", .{ root, database });
    const output = try u.capture(try argv(build, database, &.{ "/usr/bin/bash", "-c", script }), build, 4096);
    try std.testing.expectEqualStrings("isolated\n", output);
    try tmp.dir.access("build/output", .{});
    try std.testing.expectError(error.UnsafeSandboxDirectory, argv("/", database, &.{"/usr/bin/true"}));
    try std.testing.expectError(error.UnsafeSandboxDirectory, argv(build, root, &.{"/usr/bin/true"}));
    try std.testing.expectError(error.UnsafeSandboxDirectory, argv(build, build, &.{"/usr/bin/true"}));
}
