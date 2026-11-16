//! zap: a native Arch Linux and AUR helper.
//!
//! The same executable is the command-line tool and, when started by run0 or sudo as
//! `zap __transaction <request>`, the root transaction worker.
const std = @import("std");
const u = @import("util.zig");
const ui = @import("ui.zig");
const cli = @import("cli.zig");
const errors = @import("errors.zig");
const commands = @import("commands.zig");
const tx = @import("transaction.zig");

pub fn main() void {
    var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena.deinit();
    u.a = arena.allocator();
    _ = u.c.setlocale(u.c.LC_CTYPE, "");
    const status = run();
    ui.flush();
    if (status != 0) std.process.exit(status);
}

fn run() u8 {
    const args = std.process.argsAlloc(u.a) catch |err| return errors.report(err);
    if (args.len >= 2 and std.mem.eql(u8, args[1], "__transaction")) {
        if (args.len != 3) return errors.report(error.InvalidWorkerArguments);
        tx.worker(args[2]) catch |err| return errors.report(err);
        return 0;
    }
    ui.setColorMode(cli.colorPreference(args[1..]) orelse .auto);
    const options = cli.parse(args[1..]) catch |err| return errors.report(err);
    commands.run(options) catch |err| return errors.report(err);
    return 0;
}

test {
    _ = @import("util.zig");
    _ = @import("ui.zig");
    _ = @import("errors.zig");
    _ = @import("cli.zig");
    _ = @import("help.zig");
    _ = @import("config.zig");
    _ = @import("net.zig");
    _ = @import("aur.zig");
    _ = @import("news.zig");
    _ = @import("srcinfo.zig");
    _ = @import("alpm.zig");
    _ = @import("query.zig");
    _ = @import("sandbox.zig");
    _ = @import("artifacts.zig");
    _ = @import("transaction.zig");
    _ = @import("builder.zig");
    _ = @import("commands.zig");
    _ = @import("native_tests.zig");
}
