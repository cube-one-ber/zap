const std = @import("std");
pub var cancelled = std.atomic.Value(bool).init(false);
fn cancel(_: c_int) callconv(.c) void {
    cancelled.store(true, .monotonic);
}
pub fn install() void {
    const action: std.posix.Sigaction = .{ .handler = .{ .handler = cancel }, .mask = std.posix.sigemptyset(), .flags = 0 };
    std.posix.sigaction(std.posix.SIG.INT, &action, null);
    std.posix.sigaction(std.posix.SIG.TERM, &action, null);
}
pub fn check() !void {
    if (cancelled.load(.monotonic)) return error.Cancelled;
}
