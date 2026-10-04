const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const signals = @import("signals.zig");
pub fn print(comptime fmt: []const u8, args: anytype) void {
    const s = std.fmt.allocPrint(u.a, fmt, args) catch return;
    defer u.a.free(s);
    var offset: usize = 0;
    while (offset < s.len) {
        const n = std.posix.write(if (@import("builtin").is_test) 2 else 1, s[offset..]) catch return;
        if (n == 0) return;
        offset += n;
    }
}
pub fn color(s: []const u8) []const u8 {
    return if (c.isatty(1) != 0 and std.posix.getenv("NO_COLOR") == null) s else "";
}
pub fn title(s: []const u8) void {
    print("\n{s}⚡ {s}{s}\n\n", .{ color("\x1b[1;36m"), s, color("\x1b[0m") });
}
pub fn safe(s: []const u8) []const u8 {
    const result = u.a.dupe(u8, s) catch return "";
    for (result) |*ch| if (ch.* < 32 or ch.* == 127) {
        ch.* = ' ';
    };
    return result;
}
pub fn date(timestamp: i64) []const u8 {
    if (timestamp <= 0) return "unknown";
    var t: c.time_t = @intCast(timestamp);
    var tm: c.struct_tm = undefined;
    if (c.gmtime_r(&t, &tm) == null) return "unknown";
    var buf: [64]u8 = undefined;
    const len = c.strftime(&buf, buf.len, "%Y-%m-%d %H:%M UTC", &tm);
    return u.a.dupe(u8, buf[0..len]) catch "unknown";
}
pub fn answer(prompt: []const u8) ![]const u8 {
    print("{s}", .{prompt});
    var buf: [4096]u8 = undefined;
    var len: usize = 0;
    while (true) {
        try signals.check();
        var fds = [_]std.posix.pollfd{.{ .fd = 0, .events = std.posix.POLL.IN, .revents = 0 }};
        if (try std.posix.poll(&fds, 100) == 0) continue;
        try signals.check();
        var byte: [1]u8 = undefined;
        const n = try std.posix.read(0, &byte);
        if (n == 0) return error.ConfirmationRequired;
        if (byte[0] == '\n') break;
        if (len == buf.len) return error.InputTooLong;
        buf[len] = byte[0];
        len += 1;
    }
    return u.a.dupe(u8, std.mem.trim(u8, buf[0..len], " \r\t"));
}
pub fn confirm(prompt: []const u8) bool {
    const reply = answer(prompt) catch return false;
    defer u.a.free(reply);
    return std.ascii.eqlIgnoreCase(reply, "y") or std.ascii.eqlIgnoreCase(reply, "yes");
}
pub fn require(prompt: []const u8) !void {
    if (!confirm(prompt)) return error.Cancelled;
}
pub fn package(name: []const u8, version: []const u8, source: []const u8, updated: i64, description: []const u8) void {
    print("{s}{s}{s}  {s}  [{s}]\n  Updated {s}\n  {s}\n\n", .{ color("\x1b[1m"), safe(name), color("\x1b[0m"), safe(version), safe(source), date(updated), safe(description) });
}
test "remote metadata cannot inject terminal escapes" {
    const result = safe("abc\x1b[31m\nxyz\x7f");
    defer u.a.free(result);
    try std.testing.expectEqualStrings("abc [31m xyz ", result);
}
