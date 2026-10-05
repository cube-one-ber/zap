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
    if (c.isatty(1) == 0 or std.posix.getenv("NO_COLOR") != null) return "";
    if (std.posix.getenv("TERM")) |term| if (std.mem.eql(u8, term, "dumb")) return "";
    return s;
}
pub const Tone = enum { reset, bold, muted, accent, success, warning, danger };
pub fn style(tone: Tone) []const u8 {
    return color(switch (tone) {
        .reset => "\x1b[0m",
        .bold => "\x1b[1m",
        .muted => "\x1b[90m",
        .accent => "\x1b[1;36m",
        .success => "\x1b[32m",
        .warning => "\x1b[33m",
        .danger => "\x1b[1;31m",
    });
}
pub fn columns() usize {
    var size: c.struct_winsize = std.mem.zeroes(c.struct_winsize);
    if (c.isatty(1) != 0 and c.ioctl(1, c.TIOCGWINSZ, &size) == 0 and size.ws_col > 0) return @min(size.ws_col, 120);
    return 80;
}
pub fn title(s: []const u8) void {
    const heading = std.fmt.allocPrint(u.a, "› {s}", .{s}) catch return;
    defer u.a.free(heading);
    print("\n", .{});
    text(heading, 0, .accent);
}
pub fn safe(s: []const u8) []const u8 {
    const result = u.a.dupe(u8, s) catch return "";
    for (result) |*ch| if (ch.* < 32 or ch.* == 127) {
        ch.* = ' ';
    };
    return result;
}
// libc accounts for wide and combining characters in the active terminal locale.
extern fn wcwidth(c.wchar_t) c_int;
fn cellWidth(bytes: []const u8) usize {
    const cp = std.unicode.utf8Decode(bytes) catch return 1;
    const width = wcwidth(@intCast(cp));
    return if (width < 0) 1 else @intCast(width);
}
pub fn displayWidth(s: []const u8) usize {
    var width: usize = 0;
    var i: usize = 0;
    while (i < s.len) {
        const len = std.unicode.utf8ByteSequenceLength(s[i]) catch 1;
        const end = @min(i + len, s.len);
        width += cellWidth(s[i..end]);
        i = end;
    }
    return width;
}
// Wrap at word boundaries, splitting long tokens without splitting UTF-8 bytes.
// Continuation lines align with the value rather than its label.
pub fn wrapped(s: []const u8, width: usize, indent: usize) ![]const u8 {
    var out: std.ArrayList(u8) = .empty;
    errdefer out.deinit(u.a);
    const available = @max(width -| indent, 1);
    var rest = std.mem.trim(u8, s, " ");
    while (rest.len > 0) {
        var end: usize = 0;
        var cells: usize = 0;
        var space: ?usize = null;
        while (end < rest.len) {
            const len = std.unicode.utf8ByteSequenceLength(rest[end]) catch 1;
            const next = @min(end + len, rest.len);
            const count = cellWidth(rest[end..next]);
            if (cells + count > available and end > 0) break;
            if (rest[end] == ' ') space = end;
            cells += count;
            end = next;
        }
        if (end < rest.len and rest[end] != ' ') if (space) |last| {
            if (last > 0) end = last;
        };
        try out.appendSlice(u.a, std.mem.trimEnd(u8, rest[0..end], " "));
        rest = std.mem.trimStart(u8, rest[end..], " ");
        if (rest.len > 0) {
            try out.append(u.a, '\n');
            try out.appendNTimes(u.a, ' ', indent);
        }
    }
    return out.toOwnedSlice(u.a);
}
pub fn text(s: []const u8, indent: usize, tone: Tone) void {
    const clean = safe(s);
    defer u.a.free(clean);
    const content = wrapped(clean, columns(), indent) catch return;
    defer u.a.free(content);
    print("{s}{s}{s}{s}\n", .{ style(tone), padding(indent), content, style(.reset) });
}
fn padding(count: usize) []const u8 {
    const spaces = "                                                                                                                                ";
    return spaces[0..@min(count, spaces.len)];
}
pub fn field(label: []const u8, value: []const u8) void {
    const clean = safe(if (value.len == 0) "none" else value);
    defer u.a.free(clean);
    const stacked = columns() < 64;
    const indent = if (stacked) @as(usize, 4) else 26;
    const content = wrapped(clean, columns(), indent) catch return;
    defer u.a.free(content);
    if (stacked) {
        print("  {s}{s}{s}\n    {s}\n", .{ style(.muted), label, style(.reset), content });
    } else {
        print("  {s}{s}{s}{s}{s}\n", .{ style(.muted), label, padding(24 -| displayWidth(label)), style(.reset), content });
    }
}
pub fn note(tone: Tone, message: []const u8) void {
    const label = switch (tone) {
        .success => "✓ ",
        .warning => "! ",
        .danger => "× ",
        else => "· ",
    };
    const content = std.fmt.allocPrint(u.a, "{s}{s}", .{ label, message }) catch return;
    defer u.a.free(content);
    text(content, 2, tone);
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
    const clean = safe(std.mem.trim(u8, prompt, " \n"));
    defer u.a.free(clean);
    const content = wrapped(clean, columns() -| 2, 4) catch return error.OutOfMemory;
    defer u.a.free(content);
    print("  {s}?{s} {s}{s}{s} ", .{ style(.warning), style(.reset), style(.bold), content, style(.reset) });
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
pub fn packageHeader(name: []const u8, version: []const u8, source: []const u8, index: ?usize, annotation: []const u8) void {
    const clean_name = safe(name);
    defer u.a.free(clean_name);
    const clean_version = safe(version);
    defer u.a.free(clean_version);
    const clean_source = safe(source);
    defer u.a.free(clean_source);
    const clean_annotation = safe(annotation);
    defer u.a.free(clean_annotation);
    const prefix = if (index) |n| std.fmt.allocPrint(u.a, "{d: >3} ", .{n}) catch return else u.a.dupe(u8, "") catch return;
    defer u.a.free(prefix);
    const heading = std.fmt.allocPrint(u.a, "{s}/{s}", .{ clean_source, clean_name }) catch return;
    defer u.a.free(heading);
    const base_width = displayWidth(prefix) + displayWidth(heading) + displayWidth(clean_version) + 1;
    if (base_width <= columns()) {
        print("{s}{s}{s}{s}{s}/{s}{s}{s}{s} {s}{s}{s}", .{ style(.muted), prefix, style(.reset), style(.accent), clean_source, style(.reset), style(.bold), clean_name, style(.reset), style(.success), clean_version, style(.reset) });
        if (clean_annotation.len > 0 and base_width + displayWidth(clean_annotation) + 2 <= columns()) {
            print("  {s}{s}{s}\n", .{ style(.muted), clean_annotation, style(.reset) });
        } else {
            print("\n", .{});
            if (clean_annotation.len > 0) text(clean_annotation, 4, .muted);
        }
    } else {
        const numbered = std.fmt.allocPrint(u.a, "{s}{s}", .{ prefix, heading }) catch return;
        defer u.a.free(numbered);
        text(numbered, 0, .bold);
        text(clean_version, 4, .success);
        if (clean_annotation.len > 0) text(clean_annotation, 4, .muted);
    }
}
pub fn package(name: []const u8, version: []const u8, source: []const u8, date_label: []const u8, updated: i64, description: []const u8, index: ?usize) void {
    packageHeader(name, version, source, index, "");
    if (description.len > 0) text(description, 4, .reset);
    if (date_label.len > 0) {
        const stamp = std.fmt.allocPrint(u.a, "{s} {s}", .{ date_label, date(updated) }) catch return;
        defer u.a.free(stamp);
        text(stamp, 4, .muted);
    }
}
pub fn change(name: []const u8, old: []const u8, new: []const u8, source: []const u8) void {
    const clean_name = safe(name);
    defer u.a.free(clean_name);
    const clean_old = safe(old);
    defer u.a.free(clean_old);
    const clean_new = safe(new);
    defer u.a.free(clean_new);
    const clean_source = safe(source);
    defer u.a.free(clean_source);
    const line = std.fmt.allocPrint(u.a, "{s}  {s} → {s}  [{s}]", .{ clean_name, clean_old, clean_new, clean_source }) catch return;
    defer u.a.free(line);
    if (displayWidth(line) + 2 <= columns()) {
        print("  {s}{s}{s}  {s}{s}{s} → {s}{s}{s}  {s}[{s}]{s}\n", .{ style(.bold), clean_name, style(.reset), style(.muted), clean_old, style(.reset), style(.success), clean_new, style(.reset), style(.muted), clean_source, style(.reset) });
    } else text(line, 2, .success);
}
pub fn installed(name: []const u8, version: []const u8, timestamp: i64, reason: []const u8) void {
    const clean_name = safe(name);
    defer u.a.free(clean_name);
    const clean_version = safe(version);
    defer u.a.free(clean_version);
    const stamp = date(timestamp);
    const metadata = std.fmt.allocPrint(u.a, "{s} · installed {s}", .{ reason, stamp }) catch return;
    defer u.a.free(metadata);
    if (displayWidth(clean_name) + displayWidth(clean_version) + displayWidth(metadata) + 6 <= columns()) {
        print("  {s}{s}{s} {s}  {s}{s}{s}\n", .{ style(.bold), clean_name, style(.reset), clean_version, style(.muted), metadata, style(.reset) });
    } else {
        const heading = std.fmt.allocPrint(u.a, "{s} {s}", .{ clean_name, clean_version }) catch return;
        defer u.a.free(heading);
        text(heading, 2, .bold);
        text(metadata, 4, .muted);
    }
}
pub fn command(syntax: []const u8, alias: []const u8, description: []const u8) void {
    if (columns() < 80) {
        print("  {s}{s}{s}", .{ style(.bold), syntax, style(.reset) });
        if (alias.len > 0) print("  {s}{s}{s}", .{ style(.muted), alias, style(.reset) });
        print("\n", .{});
        text(description, 4, .muted);
    } else {
        print("  {s}{s}{s}{s}{s}{s}{s}{s}", .{ style(.bold), syntax, style(.reset), padding(25 -| displayWidth(syntax)), style(.muted), alias, style(.reset), padding(7 -| displayWidth(alias)) });
        const content = wrapped(description, columns(), 34) catch return;
        defer u.a.free(content);
        print("{s}\n", .{content});
    }
}

test "remote metadata cannot inject terminal escapes" {
    const result = safe("abc\x1b[31m\nxyz\x7f");
    defer u.a.free(result);
    try std.testing.expectEqualStrings("abc [31m xyz ", result);
}
test "terminal wrapping preserves words, long tokens and UTF-8" {
    const cases = .{
        .{ "one two three", 11, 4, "one two\n    three" },
        .{ "one two three", 12, 4, "one two\n    three" },
        .{ "abcdefghij", 8, 2, "abcdef\n  ghij" },
        .{ "café café", 7, 2, "café\n  café" },
        .{ "  one   two  ", 10, 2, "one\n  two" },
        .{ "abc", 2, 4, "a\n    b\n    c" },
        .{ "", 80, 4, "" },
    };
    inline for (cases) |case| {
        const result = try wrapped(case[0], case[1], case[2]);
        defer u.a.free(result);
        try std.testing.expectEqualStrings(case[3], result);
    }
}
