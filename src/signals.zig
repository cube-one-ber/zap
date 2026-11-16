//! Interrupts. The worker turns SIGINT and SIGTERM into a cancellation it
//! checks between steps; the unprivileged process defers them while the
//! worker runs, so it can report how the worker ended.
const std = @import("std");
const posix = std.posix;

pub var cancelled = std.atomic.Value(bool).init(false);

fn cancel(_: c_int) callconv(.c) void {
    cancelled.store(true, .monotonic);
}

fn ignore(_: c_int) callconv(.c) void {}

const handled = [_]u8{ posix.SIG.INT, posix.SIG.TERM, posix.SIG.QUIT };
var saved: [handled.len]posix.Sigaction = undefined;

/// In the worker: record SIGINT and SIGTERM for `check`.
pub fn install() void {
    const action: posix.Sigaction = .{ .handler = .{ .handler = cancel }, .mask = posix.sigemptyset(), .flags = 0 };
    posix.sigaction(posix.SIG.INT, &action, null);
    posix.sigaction(posix.SIG.TERM, &action, null);
}

/// Survive the terminal's interrupts until `restore`. A handler, unlike
/// SIG_IGN, is reset by exec, so child processes still receive them.
pub fn deferInterrupts() void {
    const action: posix.Sigaction = .{ .handler = .{ .handler = ignore }, .mask = posix.sigemptyset(), .flags = posix.SA.RESTART };
    for (handled, &saved) |signal, *previous| posix.sigaction(signal, &action, previous);
}

pub fn restore() void {
    for (handled, &saved) |signal, *previous| posix.sigaction(signal, previous, null);
}

pub fn check() !void {
    if (cancelled.load(.monotonic)) return error.Cancelled;
}
