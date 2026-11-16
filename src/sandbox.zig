//! Build isolation with bubblewrap.
//!
//! Every makepkg invocation, including metadata queries, gets a fresh home
//! and environment. Only the package checkout is writable on the host; the
//! system, the package database and the checkout's Git metadata are
//! read-only. Network access is kept for source downloads: this is filesystem
//! isolation, not a clean build chroot.
const std = @import("std");
const u = @import("util.zig");

const home = "/tmp/zap-home";

/// Wrap `command` so that it runs isolated inside `dir`.
pub fn argv(dir: []const u8, database: []const u8, command: []const []const u8) ![]const []const u8 {
    // The checkout must be a real path that cannot shadow system locations.
    const real = try std.fs.cwd().realpathAlloc(u.a, dir);
    if (!std.mem.eql(u8, real, dir)) return error.UnsafeSandboxDirectory;
    for ([_][]const u8{ "/", "/tmp", "/home" }) |shared| if (std.mem.eql(u8, dir, shared)) return error.UnsafeSandboxDirectory;
    for ([_][]const u8{ "/usr", "/etc", "/var", "/proc", "/dev", "/run" }) |system| if (under(dir, system)) return error.UnsafeSandboxDirectory;

    if (!std.fs.path.isAbsolute(database)) return error.UnsafeSandboxDirectory;
    const database_real = try std.fs.cwd().realpathAlloc(u.a, database);
    if (under(database_real, dir) or under(dir, database_real)) return error.UnsafeSandboxDirectory;
    for ([_][]const u8{ "/", "/tmp", "/home" }) |shared| if (std.mem.eql(u8, database_real, shared)) return error.UnsafeSandboxDirectory;

    try std.fs.cwd().access("/usr/bin/bwrap", .{});
    var args: std.ArrayList([]const u8) = .empty;
    try args.appendSlice(u.a, &.{ "/usr/bin/bwrap", "--unshare-all", "--share-net", "--die-with-parent", "--new-session", "--cap-drop", "ALL" });
    // A read-only system with the usual merged-/usr links.
    try args.appendSlice(u.a, &.{ "--ro-bind", "/usr", "/usr" });
    try args.appendSlice(u.a, &.{ "--symlink", "usr/bin", "/bin", "--symlink", "usr/bin", "/sbin" });
    try args.appendSlice(u.a, &.{ "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib", "/lib64" });
    try args.appendSlice(u.a, &.{ "--ro-bind", "/etc", "/etc", "--proc", "/proc", "--dev", "/dev" });
    // resolv.conf is usually a symlink into systemd-resolved's directory.
    // Mount only that, so downloads can resolve names without exposing /run/user.
    try args.appendSlice(u.a, &.{ "--dir", "/run", "--dir", "/run/systemd", "--ro-bind-try", "/run/systemd/resolve", "/run/systemd/resolve" });
    // Private scratch space and home; nothing from the user's home is visible.
    try args.appendSlice(u.a, &.{ "--tmpfs", "/tmp", "--dir", home });
    try args.appendSlice(u.a, &.{ "--ro-bind", database_real, database });
    try args.appendSlice(u.a, &.{ "--bind", dir, dir, "--chdir", dir });
    // No credentials, agents or desktop sockets reach the build.
    try args.append(u.a, "--clearenv");
    const environment = [_][2][]const u8{
        .{ "PATH", "/usr/bin:/bin" },
        .{ "HOME", home },
        .{ "LANG", "C.UTF-8" },
        .{ "USER", "zap" },
        .{ "LOGNAME", "zap" },
        .{ "GIT_CONFIG_GLOBAL", "/dev/null" },
        .{ "GIT_CONFIG_SYSTEM", "/dev/null" },
        .{ "GIT_TERMINAL_PROMPT", "0" },
    };
    for (environment) |pair| try args.appendSlice(u.a, &.{ "--setenv", pair[0], pair[1] });

    // Reviewed history stays immutable: a build cannot rewrite what git reports.
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
    const script = try std.fmt.allocPrint(u.a, "test ! -e '{s}/secret' && test -z \"$SSH_AUTH_SOCK\" && test -z \"$DISPLAY\" && test ! -e /run/user && test -r /etc/resolv.conf && ! touch '{s}/record' 2>/dev/null && touch output && echo isolated", .{ root, database });
    const output = try u.capture(try argv(build, database, &.{ "/usr/bin/bash", "-c", script }), build, 4096);
    try std.testing.expectEqualStrings("isolated\n", output);
    try tmp.dir.access("build/output", .{});
    try std.testing.expectError(error.UnsafeSandboxDirectory, argv("/", database, &.{"/usr/bin/true"}));
    try std.testing.expectError(error.UnsafeSandboxDirectory, argv(build, root, &.{"/usr/bin/true"}));
    try std.testing.expectError(error.UnsafeSandboxDirectory, argv(build, build, &.{"/usr/bin/true"}));
}

test "sandbox arguments keep the system read-only and only the checkout writable" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    try tmp.dir.makePath("build/.git");
    try tmp.dir.makeDir("db");
    const build = try std.fs.path.join(u.a, &.{ root, "build" });
    const database = try std.fs.path.join(u.a, &.{ root, "db" });
    const args = try argv(build, database, &.{ "/usr/bin/makepkg", "--printsrcinfo" });
    const joined = try std.mem.join(u.a, " ", args);
    const git_dir = try std.fs.path.join(u.a, &.{ build, ".git" });
    const expectations = [_][]const u8{
        "/usr/bin/bwrap --unshare-all --share-net --die-with-parent --new-session --cap-drop ALL",
        "--ro-bind /usr /usr",
        "--ro-bind /etc /etc",
        "--dir /run --dir /run/systemd --ro-bind-try /run/systemd/resolve /run/systemd/resolve",
        "--tmpfs /tmp --dir /tmp/zap-home",
        try std.fmt.allocPrint(u.a, "--ro-bind {s} {s}", .{ database, database }),
        try std.fmt.allocPrint(u.a, "--bind {s} {s} --chdir {s} --clearenv", .{ build, build, build }),
        "--setenv HOME /tmp/zap-home",
        try std.fmt.allocPrint(u.a, "--ro-bind {s} {s} -- /usr/bin/makepkg --printsrcinfo", .{ git_dir, git_dir }),
    };
    for (expectations) |expected| try std.testing.expect(std.mem.indexOf(u8, joined, expected) != null);
    // Exactly one writable bind, and no host environment passes through.
    try std.testing.expectEqual(@as(usize, 1), std.mem.count(u8, joined, " --bind "));
    try std.testing.expect(std.mem.indexOf(u8, joined, "SSH_AUTH_SOCK") == null);
}
