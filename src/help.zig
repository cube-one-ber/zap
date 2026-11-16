//! Help pages, rendered from the command and option tables in cli.zig.
const std = @import("std");
const u = @import("util.zig");
const ui = @import("ui.zig");
const cli = @import("cli.zig");

const groups = [_]struct { group: cli.Group, title: []const u8 }{
    .{ .group = .everyday, .title = "Everyday" },
    .{ .group = .inspect, .title = "Inspect" },
    .{ .group = .build, .title = "Build" },
    .{ .group = .maintain, .title = "Maintain" },
    .{ .group = .about, .title = "About" },
};

// The options worth knowing before reading any command's own page.
const common = [_]cli.Flag{ .dry_run, .aur, .repo, .needed, .quiet, .color, .help };

pub fn show(topic: ?cli.Command) void {
    if (topic) |command| commandPage(cli.spec(command)) else overview();
}

fn syntax(entry: *const cli.Spec) []const u8 {
    if (entry.operands.len == 0) return entry.name;
    return std.fmt.allocPrint(u.a, "{s} {s}", .{ entry.name, entry.operands }) catch entry.name;
}

fn optionSyntax(entry: *const cli.OptionSpec) []const u8 {
    var out: std.ArrayList(u8) = .empty;
    out.appendSlice(u.a, entry.long) catch return entry.long;
    if (entry.short.len > 0) out.print(u.a, ", {s}", .{entry.short}) catch return entry.long;
    if (entry.value.len > 0) out.print(u.a, " {s}", .{entry.value}) catch return entry.long;
    return out.items;
}

fn overview() void {
    ui.print("\n  {s}⚡ zap{s} {s}{s}{s}\n", .{ ui.style(.accent), ui.reset(), ui.style(.bold), u.version, ui.reset() });
    ui.text("A native Arch Linux and AUR helper", 2, .muted);
    ui.blank();
    ui.text("zap <command> [targets] [options]", 2, .plain);

    var syntax_width: usize = 0;
    var alias_width: usize = 0;
    for (&cli.specs) |*entry| {
        syntax_width = @max(syntax_width, ui.displayWidth(syntax(entry)));
        alias_width = @max(alias_width, ui.displayWidth(entry.alias));
    }
    for (groups) |section| {
        ui.heading(section.title, "");
        for (&cli.specs) |*entry| {
            if (entry.group == section.group) ui.define(syntax(entry), entry.alias, entry.summary, syntax_width, alias_width);
        }
    }

    ui.heading("Common options", "");
    var option_width: usize = 0;
    for (&cli.options) |*entry| {
        if (std.mem.indexOfScalar(cli.Flag, &common, entry.flag) != null) option_width = @max(option_width, ui.displayWidth(optionSyntax(entry)));
    }
    for (&cli.options) |*entry| {
        if (std.mem.indexOfScalar(cli.Flag, &common, entry.flag) != null) ui.define(optionSyntax(entry), "", entry.summary, option_width, 0);
    }

    ui.heading("Examples", "");
    for ([_][]const u8{ "zap select browser", "zap install ripgrep --needed", "zap upgrade --devel --dry-run" }) |example| ui.line(example, 2, .accent);

    ui.blank();
    ui.note(.info, "zap help <command> shows one command's usage, options and examples.");
    ui.note(.info, "Build files run as your user only after you review them; system changes go through run0 or sudo and need a final confirmation.");
    ui.blank();
}

fn commandPage(entry: *const cli.Spec) void {
    const title = std.fmt.allocPrint(u.a, "zap {s}", .{syntax(entry)}) catch return;
    ui.heading(title, if (entry.alias.len > 0) std.fmt.allocPrint(u.a, "alias {s}", .{entry.alias}) catch "" else "");
    ui.text(entry.summary, 2, .plain);
    if (entry.about.len > 0) {
        ui.blank();
        ui.text(entry.about, 2, .muted);
    }

    var option_width: usize = 0;
    for (&cli.options) |*candidate| {
        if (candidate.commands.contains(entry.command)) option_width = @max(option_width, ui.displayWidth(optionSyntax(candidate)));
    }
    ui.heading("Options", "");
    for (&cli.options) |*candidate| {
        if (candidate.commands.contains(entry.command)) ui.define(optionSyntax(candidate), "", candidate.summary, option_width, 0);
    }
    if (cli.spec(entry.command).targets != .none) {
        ui.define("--", "", "Treat everything after it as a target, even if it starts with a hyphen", option_width, 0);
    }

    if (entry.examples.len > 0) {
        ui.heading("Examples", "");
        for (entry.examples) |example| ui.line(example, 2, .accent);
    }
    ui.blank();
}

test "every help page renders within the terminal width without control bytes" {
    _ = u.c.setlocale(u.c.LC_CTYPE, "C.UTF-8");
    defer ui.setCaps(null);
    for ([_]usize{ 40, 80, 120 }) |width| {
        ui.setCaps(.{ .width = width });
        ui.beginCapture();
        show(null);
        for (cli.specs) |entry| show(entry.command);
        const output = ui.endCapture();
        var lines = std.mem.splitScalar(u8, output, '\n');
        while (lines.next()) |text_line| {
            // Example command lines are printed whole so they can be copied.
            if (std.mem.startsWith(u8, text_line, "  zap ")) continue;
            try std.testing.expect(ui.displayWidth(text_line) <= width);
            for (text_line) |byte| try std.testing.expect(byte >= 32 and byte != 127);
        }
    }
}

test "a command page lists exactly the options that command accepts" {
    defer ui.setCaps(null);
    ui.setCaps(.{ .width = 100 });
    ui.beginCapture();
    show(.remove);
    const output = ui.endCapture();
    for ([_][]const u8{ "zap remove PACKAGES", "alias -R", "--recursive", "--nosave", "--dry-run", "--color WHEN", "zap remove some-package --recursive" }) |expected| {
        try std.testing.expect(std.mem.indexOf(u8, output, expected) != null);
    }
    for ([_][]const u8{ "--needed", "--aur", "--sandbox", "--searchby" }) |absent| {
        try std.testing.expect(std.mem.indexOf(u8, output, absent) == null);
    }
}
