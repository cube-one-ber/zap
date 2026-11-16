//! Terminal presentation: capability detection, buffered output, and the small
//! set of components every command renders with.
//!
//! Every component sanitizes the text it is given, so package metadata, file
//! contents and remote responses cannot inject terminal control sequences.
const std = @import("std");
const builtin = @import("builtin");
const u = @import("util.zig");
const c = u.c;
const signals = @import("signals.zig");

pub const Tone = enum { plain, bold, muted, accent, special, success, warning, danger, underline };
pub const ColorMode = enum { auto, always, never };

// ── Capabilities ────────────────────────────────────────────────────────────

pub const Caps = struct {
    /// Styling on standard output and standard error respectively.
    color: bool = false,
    color_err: bool = false,
    /// Standard output is a terminal that supports cursor control.
    live: bool = false,
    /// The locale and terminal can render the non-ASCII glyphs zap draws with.
    unicode: bool = true,
    /// The locale encodes text as UTF-8. Otherwise only ASCII is written: in
    /// an 8-bit locale, UTF-8 continuation bytes are C1 control codes.
    utf8: bool = true,
    /// Fixed width; null queries the terminal on demand.
    width: ?usize = null,
};

var color_mode: ColorMode = .auto;
var caps_cache: ?Caps = null;

pub fn setColorMode(mode: ColorMode) void {
    color_mode = mode;
    caps_cache = null;
}

/// Pin capabilities, for deterministic rendering in tests. Null detects again.
pub fn setCaps(value: ?Caps) void {
    caps_cache = value;
}

fn caps() Caps {
    if (caps_cache) |value| return value;
    const term = std.posix.getenv("TERM") orelse "";
    const dumb = std.mem.eql(u8, term, "dumb");
    // https://no-color.org: any non-empty value disables color.
    const no_color = if (std.posix.getenv("NO_COLOR")) |value| value.len > 0 else false;
    const out_tty = c.isatty(1) != 0;
    const err_tty = c.isatty(2) != 0;
    const codeset = u.str(c.nl_langinfo(c.CODESET));
    const value: Caps = .{
        .color = switch (color_mode) {
            .always => true,
            .never => false,
            .auto => out_tty and !dumb and !no_color,
        },
        .color_err = switch (color_mode) {
            .always => true,
            .never => false,
            .auto => err_tty and !dumb and !no_color,
        },
        .live = out_tty and !dumb,
        // The Linux console font lacks the box and symbol glyphs.
        .unicode = std.ascii.eqlIgnoreCase(codeset, "UTF-8") and !std.mem.eql(u8, term, "linux"),
        .utf8 = std.ascii.eqlIgnoreCase(codeset, "UTF-8"),
    };
    caps_cache = value;
    return value;
}

pub fn columns() usize {
    if (caps().width) |fixed| return fixed;
    var size_info: c.struct_winsize = std.mem.zeroes(c.struct_winsize);
    if (c.isatty(1) != 0 and c.ioctl(1, c.TIOCGWINSZ, &size_info) == 0 and size_info.ws_col > 0) {
        return std.math.clamp(@as(usize, size_info.ws_col), 20, 120);
    }
    if (std.posix.getenv("COLUMNS")) |value| {
        if (std.fmt.parseInt(usize, value, 10)) |parsed| return std.math.clamp(parsed, 20, 120) else |_| {}
    }
    return 80;
}

fn code(tone: Tone) []const u8 {
    return switch (tone) {
        .plain => "",
        .bold => "\x1b[1m",
        .muted => "\x1b[90m",
        .accent => "\x1b[1;36m",
        .special => "\x1b[35m",
        .success => "\x1b[32m",
        .warning => "\x1b[33m",
        .danger => "\x1b[1;31m",
        .underline => "\x1b[4m",
    };
}

pub fn style(tone: Tone) []const u8 {
    return if (caps().color) code(tone) else "";
}

pub fn reset() []const u8 {
    return if (caps().color) "\x1b[0m" else "";
}

/// Whether standard output is being styled.
pub fn styled() bool {
    return caps().color;
}

// ── Output ──────────────────────────────────────────────────────────────────

const Buf = std.ArrayList(u8);
const flush_threshold = 32 * 1024;

var pending: Buf = .empty;
var depth: usize = 0;
var capturing = false;
var progress_active = false;

fn begin() void {
    if (depth == 0 and progress_active) {
        put("\r\x1b[2K");
        progress_active = false;
    }
    depth += 1;
}

fn end() void {
    depth -= 1;
    // Each component is written as it completes, so output is never stale
    // while a child process or a prompt has the terminal.
    if (depth == 0 or pending.items.len >= flush_threshold) flush();
}

/// Batch the output of a bulk listing into few writes, until `release`.
pub fn hold() void {
    begin();
}

pub fn release() void {
    end();
}

fn put(bytes: []const u8) void {
    pending.appendSlice(u.a, bytes) catch {};
}

fn putf(comptime fmt: []const u8, args: anytype) void {
    pending.print(u.a, fmt, args) catch {};
}

fn pad(cells: usize) void {
    pending.appendNTimes(u.a, ' ', cells) catch {};
}

fn repeat(glyph: []const u8, times: usize) void {
    for (0..times) |_| put(glyph);
}

fn paint(tone: Tone, text_bytes: []const u8) void {
    if (text_bytes.len == 0) return;
    if (tone == .plain or !caps().color) return put(text_bytes);
    put(code(tone));
    put(text_bytes);
    put("\x1b[0m");
}

/// Write buffered standard output. Components do this themselves; call it
/// before starting a child process or exiting while output may be held.
pub fn flush() void {
    if (capturing or pending.items.len == 0) return;
    writeAll(if (builtin.is_test) 2 else 1, plain(pending.items));
    pending.clearRetainingCapacity();
}

fn writeAll(fd: std.posix.fd_t, bytes: []const u8) void {
    var offset: usize = 0;
    while (offset < bytes.len) {
        const n = std.posix.write(fd, bytes[offset..]) catch return;
        if (n == 0) return;
        offset += n;
    }
}

// Single-cell ASCII stand-ins keep alignment when the glyphs cannot render.
const fallbacks = [_][2][]const u8{
    .{ "▌", ">" }, .{ "─", "-" }, .{ "━", "#" }, .{ "·", "-" },  .{ "→", ">" }, .{ "—", "-" },
    .{ "…", "~" }, .{ "✓", "+" }, .{ "×", "x" },  .{ "⚡", "*" }, .{ "◆", "*" }, .{ "\u{fffd}", "?" },
};

fn plain(bytes: []const u8) []const u8 {
    const current = caps();
    if (current.unicode and current.utf8) return bytes;
    var result: []const u8 = bytes;
    for (fallbacks) |pair| {
        if (std.mem.indexOf(u8, result, pair[0]) == null) continue;
        result = std.mem.replaceOwned(u8, u.a, result, pair[0], pair[1]) catch return result;
    }
    if (current.utf8) return result;
    // One '?' per character keeps the cell count wcwidth reported in this locale.
    var ascii: Buf = .empty;
    var i: usize = 0;
    while (i < result.len) {
        if (result[i] < 0x80) {
            ascii.append(u.a, result[i]) catch return "";
            i += 1;
            continue;
        }
        ascii.append(u.a, '?') catch return "";
        const len = std.unicode.utf8ByteSequenceLength(result[i]) catch 1;
        i += @max(1, @min(len, result.len - i));
    }
    return ascii.items;
}

/// Collect output in memory instead of writing it, for tests.
pub fn beginCapture() void {
    flush();
    capturing = true;
}

pub fn endCapture() []const u8 {
    capturing = false;
    const result = u.a.dupe(u8, plain(pending.items)) catch "";
    pending.clearRetainingCapacity();
    return result;
}

/// Formatted standard output. Arguments are written as given: sanitize
/// untrusted values with `safe` first.
pub fn print(comptime fmt: []const u8, args: anytype) void {
    begin();
    defer end();
    putf(fmt, args);
}

pub fn blank() void {
    print("\n", .{});
}

/// Formatted standard error, ordered after pending standard output.
pub fn eprint(comptime fmt: []const u8, args: anytype) void {
    begin();
    depth -= 1;
    flush();
    const message = std.fmt.allocPrint(u.a, fmt, args) catch return;
    defer u.a.free(message);
    if (capturing) return put(message);
    writeAll(2, plain(message));
}

// ── Text ────────────────────────────────────────────────────────────────────

/// Neutralize terminal control: C0/C1 control characters become spaces,
/// bidirectional overrides and invalid UTF-8 become visible placeholders.
/// The result has the same byte length as the input.
pub fn safe(s: []const u8) []const u8 {
    const result = u.a.dupe(u8, s) catch return "";
    var i: usize = 0;
    while (i < result.len) {
        const byte = result[i];
        if (byte < 0x80) {
            if (byte < 32 or byte == 127) result[i] = ' ';
            i += 1;
            continue;
        }
        const len = std.unicode.utf8ByteSequenceLength(byte) catch {
            result[i] = '?';
            i += 1;
            continue;
        };
        if (i + len > result.len) {
            result[i] = '?';
            i += 1;
            continue;
        }
        const point = std.unicode.utf8Decode(result[i .. i + len]) catch {
            result[i] = '?';
            i += 1;
            continue;
        };
        if (point >= 0x80 and point <= 0x9f) {
            @memset(result[i .. i + len], ' ');
        } else if (reordering(point)) {
            // U+FFFD where it fits the sequence's length; U+061C is shorter.
            if (len == 3) @memcpy(result[i .. i + len], "\u{fffd}") else @memset(result[i .. i + len], '?');
        }
        i += len;
    }
    return result;
}

/// Characters that reorder or hide neighbouring text when displayed.
fn reordering(point: u21) bool {
    return switch (point) {
        0x061c, 0x200e, 0x200f, 0x202a...0x202e, 0x2066...0x2069, 0x2028, 0x2029 => true,
        else => false,
    };
}

// libc accounts for wide and combining characters in the active terminal locale.
extern fn wcwidth(c.wchar_t) c_int;

fn cellWidth(bytes: []const u8) usize {
    const point = std.unicode.utf8Decode(bytes) catch return 1;
    const width = wcwidth(@intCast(point));
    return if (width < 0) 1 else @intCast(width);
}

pub fn displayWidth(s: []const u8) usize {
    var width: usize = 0;
    var i: usize = 0;
    while (i < s.len) {
        const len = std.unicode.utf8ByteSequenceLength(s[i]) catch 1;
        const stop = @min(i + len, s.len);
        width += cellWidth(s[i..stop]);
        i = stop;
    }
    return width;
}

/// Wrap at word boundaries, splitting long tokens without splitting UTF-8
/// sequences. Continuation lines are indented; the first line is not.
pub fn wrapped(s: []const u8, width: usize, indent: usize) ![]const u8 {
    var out: Buf = .empty;
    errdefer out.deinit(u.a);
    const available = @max(width -| indent, 1);
    var rest = std.mem.trim(u8, s, " ");
    while (rest.len > 0) {
        var stop: usize = 0;
        var cells: usize = 0;
        var space: ?usize = null;
        while (stop < rest.len) {
            const len = std.unicode.utf8ByteSequenceLength(rest[stop]) catch 1;
            const next = @min(stop + len, rest.len);
            const wide = cellWidth(rest[stop..next]);
            if (cells + wide > available and stop > 0) break;
            if (rest[stop] == ' ') space = stop;
            cells += wide;
            stop = next;
        }
        if (stop < rest.len and rest[stop] != ' ') if (space) |last| {
            if (last > 0) stop = last;
        };
        try out.appendSlice(u.a, std.mem.trimEnd(u8, rest[0..stop], " "));
        rest = std.mem.trimStart(u8, rest[stop..], " ");
        if (rest.len > 0) {
            try out.append(u.a, '\n');
            try out.appendNTimes(u.a, ' ', indent);
        }
    }
    return out.toOwnedSlice(u.a);
}

/// Shorten to a cell budget with an ellipsis, without splitting UTF-8.
pub fn clipped(s: []const u8, width: usize) ![]const u8 {
    if (displayWidth(s) <= width) return u.a.dupe(u8, s);
    if (width == 0) return u.a.dupe(u8, "");
    var stop: usize = 0;
    var cells: usize = 0;
    while (stop < s.len) {
        const len = std.unicode.utf8ByteSequenceLength(s[stop]) catch 1;
        const next = @min(stop + len, s.len);
        const wide = cellWidth(s[stop..next]);
        if (cells + wide > width - 1) break;
        cells += wide;
        stop = next;
    }
    return std.fmt.allocPrint(u.a, "{s}…", .{s[0..stop]});
}

/// A sanitized, wrapped paragraph.
pub fn text(s: []const u8, indent: usize, tone: Tone) void {
    const content = wrapped(safe(s), columns(), indent) catch return;
    begin();
    defer end();
    pad(indent);
    paint(tone, content);
    put("\n");
}

/// Sanitized text on one line, never wrapped: links and paths stay intact.
pub fn line(s: []const u8, indent: usize, tone: Tone) void {
    begin();
    defer end();
    pad(indent);
    paint(tone, safe(s));
    put("\n");
}

// ── Components ──────────────────────────────────────────────────────────────

/// Section heading with an optional muted detail, such as a count.
/// Neither part is ever cut: a detail that does not fit beside the title
/// goes on its own line beneath it.
pub fn heading(title_text: []const u8, detail: []const u8) void {
    const width = columns();
    const clean_title = safe(title_text);
    const clean_detail = safe(detail);
    begin();
    defer end();
    put("\n");
    paint(.accent, "▌");
    put(" ");
    paint(.bold, wrapped(clean_title, width, 2) catch return);
    const last_line = clean_title[if (std.mem.lastIndexOfScalar(u8, clean_title, '\n')) |at| at + 1 else 0..];
    const used = 2 + @min(displayWidth(last_line), width);
    if (clean_detail.len > 0 and used + 2 + displayWidth(clean_detail) <= width) {
        put("  ");
        paint(.muted, clean_detail);
    } else if (clean_detail.len > 0) {
        put("\n");
        pad(2);
        paint(.muted, wrapped(clean_detail, width, 2) catch return);
    }
    put("\n");
}

/// A labelled horizontal rule that separates long bodies, such as files.
pub fn rule(label: []const u8) void {
    begin();
    defer end();
    const width = columns();
    const clean = safe(label);
    put("\n");
    // A label that does not fit beside the rule, such as a long path, is
    // printed whole above a plain one rather than cut.
    if (displayWidth(clean) + 8 > width) {
        paint(.bold, clean);
        put("\n");
        if (caps().color) put(code(.muted));
        repeat("─", width);
        put(reset());
        put("\n");
        return;
    }
    if (caps().color) put(code(.muted));
    put("── ");
    if (caps().color) put("\x1b[0m" ++ "\x1b[1m");
    put(clean);
    if (caps().color) put("\x1b[0m" ++ "\x1b[90m");
    put(" ");
    repeat("─", width -| (displayWidth(clean) + 4));
    put(reset());
    put("\n");
}

pub const NoteKind = enum { info, success, warning, failure };

pub fn note(kind: NoteKind, message: []const u8) void {
    const mark: []const u8, const tone: Tone = switch (kind) {
        .info => .{ "·", .muted },
        .success => .{ "✓", .success },
        .warning => .{ "!", .warning },
        .failure => .{ "×", .danger },
    };
    marked(mark, tone, message, if (kind == .info) .muted else .plain);
}

/// A line led by a one-column mark in its own tone, wrapping beneath its
/// text: the shape of notes and of flagged list items.
pub fn marked(mark: []const u8, mark_tone: Tone, message: []const u8, tone: Tone) void {
    const content = wrapped(safe(message), columns(), 4) catch return;
    begin();
    defer end();
    pad(2);
    paint(mark_tone, mark);
    put(" ");
    paint(tone, content);
    put("\n");
}

pub fn notef(kind: NoteKind, comptime fmt: []const u8, args: anytype) void {
    note(kind, std.fmt.allocPrint(u.a, fmt, args) catch return);
}

/// A problem report on standard error: what failed, then how to proceed.
pub fn problem(kind: NoteKind, message: []const u8, hint: []const u8) void {
    const colored = caps().color_err;
    const tone: Tone = if (kind == .warning) .warning else .danger;
    const mark: []const u8 = if (kind == .warning) "!" else "×";
    const width = columns();
    const content = wrapped(safe(message), width, 4) catch return;
    eprint("  {s}{s}{s} {s}{s}{s}\n", .{
        if (colored) code(tone) else "",  mark,    if (colored) "\x1b[0m" else "",
        if (colored) code(.bold) else "", content, if (colored) "\x1b[0m" else "",
    });
    if (hint.len == 0) return;
    const advice = wrapped(safe(hint), width, 4) catch return;
    eprint("    {s}{s}{s}\n", .{ if (colored) code(.muted) else "", advice, if (colored) "\x1b[0m" else "" });
}

pub fn warn(comptime fmt: []const u8, args: anytype) void {
    problem(.warning, std.fmt.allocPrint(u.a, fmt, args) catch return, "");
}

/// Library diagnostics stay on standard error and never overwrite a live
/// progress line.
pub fn diagnostic(severe: bool, message: []const u8) void {
    const clean = std.mem.trim(u8, message, " \r\n");
    if (clean.len == 0) return;
    problem(if (severe) .failure else .warning, std.fmt.allocPrint(u.a, "libalpm: {s}", .{clean}) catch return, "");
}

const label_width = 20;

/// A labelled value. Long values wrap beneath themselves, not the label.
pub fn field(label: []const u8, value: []const u8) void {
    fieldLines(label, &.{if (value.len == 0) "none" else value});
}

/// A labelled value that is omitted when empty.
pub fn fieldOptional(label: []const u8, value: []const u8) void {
    if (value.len > 0) fieldLines(label, &.{value});
}

/// A labelled list rendered as one wrapped line; omitted when empty.
pub fn fieldList(label: []const u8, values: []const []const u8) void {
    if (values.len == 0) return;
    fieldLines(label, &.{std.mem.join(u.a, "  ", values) catch return});
}

/// A labelled value that must stay whole, such as a path or a digest. It
/// moves beneath its label when it does not fit beside it, and runs past the
/// edge rather than wrapping inside the token.
pub fn fieldRaw(label: []const u8, value: []const u8) void {
    const width = columns();
    const clean = safe(value);
    begin();
    defer end();
    pad(2);
    paint(.muted, label);
    if (width >= 56 and 2 + label_width + displayWidth(clean) <= width) {
        pad(label_width -| displayWidth(label));
    } else {
        put("\n");
        pad(4);
    }
    put(clean);
    put("\n");
}

/// A labelled value with one entry per line.
pub fn fieldLines(label: []const u8, values: []const []const u8) void {
    if (values.len == 0) return;
    const width = columns();
    const stacked = width < 56;
    const indent: usize = if (stacked) 4 else 2 + label_width;
    begin();
    defer end();
    pad(2);
    paint(.muted, label);
    if (stacked) put("\n");
    for (values, 0..) |value, i| {
        const content = wrapped(safe(value), width, indent) catch return;
        if (stacked or i > 0) pad(indent) else pad(label_width -| displayWidth(label));
        put(content);
        put("\n");
    }
}

/// A term, an optional secondary term, and a description that wraps beneath
/// itself. Widths are supplied so that separate groups share one alignment.
pub fn define(term: []const u8, extra: []const u8, description: []const u8, term_width: usize, extra_width: usize) void {
    const width = columns();
    const offset = 2 + term_width + 2 + (if (extra_width > 0) extra_width + 2 else 0);
    begin();
    defer end();
    pad(2);
    paint(.bold, term);
    if (width < offset + 24) {
        if (extra.len > 0) {
            put("  ");
            paint(.muted, extra);
        }
        put("\n");
        pad(6);
        put(wrapped(description, width, 6) catch return);
        put("\n");
        return;
    }
    pad(2 + term_width -| displayWidth(term));
    if (extra_width > 0) {
        paint(.muted, extra);
        pad(2 + extra_width -| displayWidth(extra));
    }
    put(wrapped(description, width, offset) catch return);
    put("\n");
}

pub const Align = enum { left, right };
pub const Column = struct {
    title: []const u8 = "",
    tone: Tone = .plain,
    alignment: Align = .left,
    /// What precedes this cell when a narrow terminal stacks the row, such as
    /// " → " between a current and a new version.
    joiner: []const u8 = " · ",
    /// Names the cell when a stacked row loses its column title, such as
    /// "updated" before a bare date.
    label: []const u8 = "",
};
pub const Cell = struct { text: []const u8 = "", tone: ?Tone = null };

/// Rows aligned in columns. On a terminal too narrow for columns at all,
/// each row is stacked: its first cell, then the rest beneath it.
pub const Table = struct {
    columns: []const Column,
    rows: std.ArrayList([]Cell) = .empty,
    indent: usize = 2,

    pub fn init(column_specs: []const Column) Table {
        return .{ .columns = column_specs };
    }

    pub fn add(self: *Table, cells: []const Cell) void {
        std.debug.assert(cells.len == self.columns.len);
        const row = u.a.alloc(Cell, cells.len) catch return;
        for (cells, row) |cell, *slot| slot.* = .{ .text = safe(cell.text), .tone = cell.tone };
        self.rows.append(u.a, row) catch {};
    }

    pub fn addText(self: *Table, values: []const []const u8) void {
        std.debug.assert(values.len == self.columns.len);
        const row = u.a.alloc(Cell, values.len) catch return;
        for (values, row) |value, *slot| slot.* = .{ .text = safe(value) };
        self.rows.append(u.a, row) catch {};
    }

    /// Column widths for a terminal `available` cells wide, or null when the
    /// rows must be stacked instead. If a few outlying rows are all that keep
    /// the table from fitting, the widest columns give way and those rows
    /// overflow on their own lines: one long value neither breaks the layout
    /// for every row nor is ever cut.
    fn layout(self: *const Table, available: usize) ?[]usize {
        const result = u.a.alloc(usize, self.columns.len) catch return null;
        const floors = u.a.alloc(usize, self.columns.len) catch return null;
        for (self.columns, result) |column, *width| width.* = displayWidth(column.title);
        for (self.rows.items) |row| {
            for (row, result) |cell, *width| width.* = @max(width.*, displayWidth(cell.text));
        }
        var total: usize = self.indent + 2 * (self.columns.len - 1);
        for (self.columns, result, floors) |column, width, *floor| {
            floor.* = @min(width, @max(displayWidth(column.title), 8));
            total += width;
        }
        if (total <= available) return result;
        while (total > available) {
            var widest: ?usize = null;
            for (result, floors, 0..) |width, floor, i| {
                if (width > floor and (widest == null or width > result[widest.?])) widest = i;
            }
            result[widest orelse return null] -= 1;
            total -= 1;
        }
        // Worth it only when the long values are outliers.
        var overflowing: usize = 0;
        for (self.rows.items) |row| {
            for (row, result) |cell, width| if (displayWidth(cell.text) > width) {
                overflowing += 1;
                break;
            };
        }
        return if (overflowing * 8 > self.rows.items.len) null else result;
    }

    pub fn fits(self: *const Table, width: usize) bool {
        return self.layout(width) != null;
    }

    pub fn render(self: *const Table) void {
        if (self.rows.items.len == 0) return;
        begin();
        defer end();
        const column_widths = self.layout(columns()) orelse return self.renderStacked();
        var titled = false;
        for (self.columns) |column| titled = titled or column.title.len > 0;
        if (titled) {
            const titles = u.a.alloc(Cell, self.columns.len) catch return;
            for (self.columns, titles) |column, *slot| slot.* = .{ .text = column.title, .tone = .underline };
            self.renderRow(titles, column_widths);
        }
        for (self.rows.items) |row| self.renderRow(row, column_widths);
    }

    fn renderRow(self: *const Table, row: []const Cell, column_widths: []const usize) void {
        // Nothing is padded after the last cell with content.
        var filled: usize = 0;
        for (row, 0..) |cell, i| {
            if (cell.text.len > 0) filled = i + 1;
        }
        pad(self.indent);
        for (row[0..filled], self.columns[0..filled], column_widths[0..filled], 0..) |cell, column, width, i| {
            const gap = width -| displayWidth(cell.text);
            if (column.alignment == .right) pad(gap);
            paint(cell.tone orelse column.tone, cell.text);
            if (i + 1 < filled) pad(2 + if (column.alignment == .left) gap else 0);
        }
        put("\n");
    }

    fn renderStacked(self: *const Table) void {
        const width = columns();
        for (self.rows.items) |row| {
            pad(self.indent);
            paint(row[0].tone orelse self.columns[0].tone, wrapped(row[0].text, width, self.indent) catch return);
            put("\n");
            // The other cells follow on one line, each in its own tone and
            // labelled where a bare value would be ambiguous, wrapping only
            // between cells.
            const indent = self.indent + 2;
            var used: usize = indent;
            var first = true;
            for (row[1..], self.columns[1..]) |cell, column| {
                if (cell.text.len == 0) continue;
                const joiner = if (first) "" else column.joiner;
                const label_cells = if (column.label.len > 0) displayWidth(column.label) + 1 else 0;
                const cells = displayWidth(joiner) + label_cells + displayWidth(cell.text);
                if (first) {
                    pad(indent);
                } else if (used + cells > width) {
                    // A wrapped cell starts its own line; only a joiner that
                    // means something, such as an arrow, is carried over.
                    put("\n");
                    pad(indent);
                    used = indent;
                    const lead = std.mem.trim(u8, joiner, " ");
                    if (!std.mem.eql(u8, lead, "·")) {
                        paint(.muted, lead);
                        put(" ");
                        used += displayWidth(lead) + 1;
                    }
                } else {
                    paint(.muted, joiner);
                    used += displayWidth(joiner);
                }
                if (column.label.len > 0) {
                    paint(.muted, column.label);
                    put(" ");
                    used += label_cells;
                }
                paint(cell.tone orelse column.tone, cell.text);
                used += displayWidth(cell.text);
                first = false;
            }
            if (!first) put("\n");
        }
    }
};

pub const Badge = struct { text: []const u8, tone: Tone = .muted };

/// A package in a result list: identity, badges, description and a muted
/// metadata line.
pub const Entry = struct {
    index: ?usize = null,
    source: []const u8,
    name: []const u8,
    /// A web page for the package; styled output makes the name a link to it.
    link: []const u8 = "",
    version: []const u8,
    badges: []const Badge = &.{},
    description: []const u8 = "",
    meta: []const u8 = "",
};

pub fn entry(e: Entry) void {
    const width = columns();
    const source = safe(e.source);
    const name = safe(e.name);
    const version = safe(e.version);
    begin();
    defer end();
    var used: usize = 2;
    if (e.index) |n| {
        const number = std.fmt.allocPrint(u.a, "{d: >4} ", .{n}) catch return;
        paint(.muted, number);
        used = displayWidth(number);
    } else pad(2);
    const body = used + 4;
    paint(if (std.mem.eql(u8, source, "aur")) .special else .accent, source);
    paint(.muted, "/");
    // An OSC 8 hyperlink takes no columns. Only a plain https URL is
    // written into the escape sequence.
    const linked = caps().color and std.mem.startsWith(u8, e.link, "https://") and for (e.link) |byte| {
        if (byte < 0x21 or byte > 0x7e) break false;
    } else true;
    if (linked) {
        put("\x1b]8;;");
        put(e.link);
        put("\x1b\\");
    }
    paint(.bold, name);
    if (linked) put("\x1b]8;;\x1b\\");
    used += displayWidth(source) + 1 + displayWidth(name);
    if (used + 1 + displayWidth(version) > width) {
        put("\n");
        pad(body);
        used = body;
    } else {
        put(" ");
        used += 1;
    }
    paint(.success, version);
    used += displayWidth(version);
    for (e.badges) |badge| {
        const label = safe(badge.text);
        if (used + 2 + displayWidth(label) > width) {
            put("\n");
            pad(body);
            used = body;
        } else {
            put("  ");
            used += 2;
        }
        paint(badge.tone, label);
        used += displayWidth(label);
    }
    put("\n");
    for ([_][]const u8{ e.description, e.meta }, [_]Tone{ .plain, .muted }) |value, tone| {
        if (value.len == 0) continue;
        pad(body);
        paint(tone, wrapped(safe(value), width, body) catch return);
        put("\n");
    }
}

/// A bar whose length is `part / whole` of `width` cells; any positive part
/// shows at least one cell.
pub fn bar(part: i64, whole: i64, width: usize) []const u8 {
    if (whole <= 0 or part <= 0 or width == 0) return "";
    const scaled = @divTrunc(@as(i128, part) * @as(i128, @intCast(width)), whole);
    const cells: usize = @intCast(std.math.clamp(scaled, 1, @as(i128, @intCast(width))));
    var out: Buf = .empty;
    for (0..cells) |_| out.appendSlice(u.a, "━") catch return "";
    return out.items;
}

// ── Progress ────────────────────────────────────────────────────────────────

var progress_key: ?u64 = null;
var progress_percent: usize = 0;
var progress_time: i64 = 0;

/// A single live status line; a completed step leaves one finished line.
pub fn progress(label: []const u8, name: []const u8, percent: usize, current: usize, total: usize) void {
    const value: usize = @min(percent, 100);
    const key = progressKey(label, name, current, total);
    const now = std.time.milliTimestamp();
    const same = progress_key != null and progress_key.? == key;
    if (same and (progress_percent == value or (value < 100 and now - progress_time < 100))) return;
    progress_key = key;
    progress_percent = value;
    progress_time = now;
    const message = safe(std.fmt.allocPrint(u.a, "{s}{s}{s}", .{ label, if (name.len > 0) @as([]const u8, " ") else "", name }) catch return);
    const width = columns();
    if (!caps().live or width < 40) {
        if (value == 100) note(.success, message);
        return;
    }
    const position = if (total > 0) std.fmt.allocPrint(u.a, " · {d}/{d}", .{ current, total }) catch return else "";
    const bar_width: usize = @min(width / 5, 18);
    const available = width -| (bar_width + displayWidth(position) + 11);
    const caption = clipped(message, available) catch return;
    const filled = value * bar_width / 100;
    begin();
    put("\r\x1b[2K  ");
    put(caption);
    pad(available -| displayWidth(caption));
    put("  ");
    if (caps().color) put(code(.accent));
    repeat("━", filled);
    if (caps().color) put("\x1b[0m" ++ "\x1b[90m");
    repeat("─", bar_width - filled);
    put(reset());
    putf(" {d: >3}%", .{value});
    paint(.muted, position);
    if (value == 100) put("\n");
    progress_active = value < 100;
    end();
}

fn progressKey(label: []const u8, name: []const u8, current: usize, total: usize) u64 {
    const counters = [2]usize{ current, total };
    const seed = std.hash.Wyhash.hash(std.hash.Wyhash.hash(0, label), name);
    return std.hash.Wyhash.hash(seed, std.mem.asBytes(&counters));
}

// ── Prompts ─────────────────────────────────────────────────────────────────

/// Ask a question and read one line from standard input. End of input is
/// never treated as consent.
pub fn answer(prompt: []const u8) ![]const u8 {
    const content = wrapped(safe(std.mem.trim(u8, prompt, " \n")), columns() -| 2, 4) catch return error.OutOfMemory;
    begin();
    pad(2);
    paint(.warning, "?");
    put(" ");
    paint(.bold, content);
    put(" ");
    end();
    flush();
    var buf: [4096]u8 = undefined;
    var len: usize = 0;
    while (true) {
        try signals.check();
        var fds = [_]std.posix.pollfd{.{ .fd = 0, .events = std.posix.POLL.IN, .revents = 0 }};
        if (try std.posix.poll(&fds, 100) == 0) continue;
        try signals.check();
        var byte: [1]u8 = undefined;
        const n = try std.posix.read(0, &byte);
        if (n == 0) {
            // No newline was echoed; leave the prompt's line.
            blank();
            return error.ConfirmationRequired;
        }
        if (byte[0] == '\n') break;
        if (len == buf.len) return error.InputTooLong;
        buf[len] = byte[0];
        len += 1;
    }
    return u.a.dupe(u8, std.mem.trim(u8, buf[0..len], " \r\t"));
}

pub fn confirm(prompt: []const u8) bool {
    const reply = answer(prompt) catch return false;
    return std.ascii.eqlIgnoreCase(reply, "y") or std.ascii.eqlIgnoreCase(reply, "yes");
}

/// Ask, and refuse unless the answer is yes. Input that ends first is
/// ConfirmationRequired, as at any other prompt.
pub fn require(prompt: []const u8) !void {
    const reply = try answer(prompt);
    if (!std.ascii.eqlIgnoreCase(reply, "y") and !std.ascii.eqlIgnoreCase(reply, "yes")) return error.Cancelled;
}

// ── Values ──────────────────────────────────────────────────────────────────

/// Binary size with a unit that keeps two or three significant digits.
pub fn size(bytes: i64) []const u8 {
    const magnitude: f64 = @floatFromInt(@abs(bytes));
    const sign: []const u8 = if (bytes < 0) "-" else "";
    const kib = 1024.0;
    const result = if (magnitude < kib)
        std.fmt.allocPrint(u.a, "{s}{d} B", .{ sign, @abs(bytes) })
    else if (magnitude < kib * kib)
        std.fmt.allocPrint(u.a, "{s}{d:.1} KiB", .{ sign, magnitude / kib })
    else if (magnitude < kib * kib * kib)
        std.fmt.allocPrint(u.a, "{s}{d:.1} MiB", .{ sign, magnitude / (kib * kib) })
    else
        std.fmt.allocPrint(u.a, "{s}{d:.2} GiB", .{ sign, magnitude / (kib * kib * kib) });
    return result catch "";
}

/// A size change with an explicit sign.
pub fn sizeDelta(bytes: i64) []const u8 {
    if (bytes <= 0) return size(bytes);
    return std.fmt.allocPrint(u.a, "+{s}", .{size(bytes)}) catch "";
}

pub fn count(n: usize, singular: []const u8, plural: []const u8) []const u8 {
    return std.fmt.allocPrint(u.a, "{d} {s}", .{ n, if (n == 1) singular else plural }) catch "";
}

fn stamp(timestamp: i64, comptime format: [:0]const u8) []const u8 {
    if (timestamp <= 0) return "unknown";
    var t: c.time_t = @intCast(timestamp);
    var tm: c.struct_tm = undefined;
    if (c.gmtime_r(&t, &tm) == null) return "unknown";
    var buf: [64]u8 = undefined;
    const len = c.strftime(&buf, buf.len, format.ptr, &tm);
    return u.a.dupe(u8, buf[0..len]) catch "unknown";
}

/// Calendar date in UTC.
pub fn date(timestamp: i64) []const u8 {
    return stamp(timestamp, "%Y-%m-%d");
}

/// Date and time in UTC.
pub fn datetime(timestamp: i64) []const u8 {
    return stamp(timestamp, "%Y-%m-%d %H:%M UTC");
}

/// Coarse distance between two timestamps, for orientation beside a date.
pub fn age(timestamp: i64, now: i64) []const u8 {
    if (timestamp <= 0) return "unknown";
    const days: usize = @intCast(@divFloor(@max(now - timestamp, 0), 86400));
    if (days == 0) return "today";
    if (days == 1) return "yesterday";
    const result = if (days < 14)
        std.fmt.allocPrint(u.a, "{d} days ago", .{days})
    else if (days < 63)
        std.fmt.allocPrint(u.a, "{d} weeks ago", .{days / 7})
    else if (days < 730)
        std.fmt.allocPrint(u.a, "{d} months ago", .{days / 30})
    else
        std.fmt.allocPrint(u.a, "{d} years ago", .{days / 365});
    return result catch "unknown";
}

/// A date with its age, such as `2026-07-16 · 4 months ago`.
pub fn dated(timestamp: i64) []const u8 {
    if (timestamp <= 0) return "unknown";
    return std.fmt.allocPrint(u.a, "{s} · {s}", .{ date(timestamp), age(timestamp, std.time.timestamp()) }) catch "unknown";
}

// ── Tests ───────────────────────────────────────────────────────────────────

fn rendered(width: usize) void {
    setCaps(.{ .width = width });
    beginCapture();
}

test "remote metadata cannot inject terminal escapes" {
    try std.testing.expectEqualStrings("abc [31m xyz ", safe("abc\x1b[31m\nxyz\x7f"));
    // C1 controls include an 8-bit CSI that some terminals honour.
    try std.testing.expectEqualStrings("a  [31mb", safe("a\u{9b}[31mb"));
    // Bidirectional overrides would let reviewed source display misleadingly.
    try std.testing.expectEqualStrings("a\u{fffd}b\u{fffd}c", safe("a\u{202e}b\u{2066}c"));
    try std.testing.expectEqualStrings("a\u{fffd}b??c", safe("a\u{200f}b\u{061c}c"));
    try std.testing.expectEqualStrings("a?b", safe("a\xffb"));
    try std.testing.expectEqualStrings("ok?", safe("ok\xe2"));
    try std.testing.expectEqualStrings("日本語 café", safe("日本語 café"));
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
        try std.testing.expectEqualStrings(case[3], try wrapped(case[0], case[1], case[2]));
    }
}

test "status captions keep valid Unicode within a terminal cell budget" {
    _ = c.setlocale(c.LC_CTYPE, "C.UTF-8");
    for ([_][]const u8{ "a long package name", "日本語のパッケージ", "café e\u{0301}clair" }) |caption| {
        for (0..12) |width| {
            const result = try clipped(caption, width);
            try std.testing.expect(std.unicode.utf8ValidateSlice(result));
            try std.testing.expect(displayWidth(result) <= width);
        }
    }
}

test "tables align Unicode, sanitize cells and stack when the terminal is narrow" {
    _ = c.setlocale(c.LC_CTYPE, "C.UTF-8");
    defer setCaps(null);
    var table = Table.init(&.{ .{ .title = "Package" }, .{ .title = "Size", .alignment = .right }, .{ .title = "Change" } });
    table.addText(&.{ "日本語-package\x1b", "4.3 MiB", "upgrade" });
    table.addText(&.{ "b", "12 B", "remove" });
    try std.testing.expect(!table.fits(24));
    try std.testing.expect(table.fits(40));
    rendered(40);
    table.render();
    try std.testing.expectEqualStrings(
        \\  Package             Size  Change
        \\  日本語-package   4.3 MiB  upgrade
        \\  b                   12 B  remove
        \\
    , endCapture());
    rendered(24);
    table.render();
    try std.testing.expectEqualStrings(
        \\  日本語-package
        \\    4.3 MiB · upgrade
        \\  b
        \\    12 B · remove
        \\
    , endCapture());
}

test "one outlying row overflows instead of forcing every row to stack" {
    defer setCaps(null);
    var table = Table.init(&.{ .{ .title = "Package" }, .{ .title = "Version" } });
    for (0..8) |_| table.addText(&.{ "short", "1.0-1" });
    table.addText(&.{ "a-package-with-a-long-name", "2.0-1" });
    rendered(24);
    table.render();
    const output = endCapture();
    try std.testing.expect(std.mem.startsWith(u8, output, "  Package        Version\n  short          1.0-1\n"));
    try std.testing.expect(std.mem.endsWith(u8, output, "  a-package-with-a-long-name  2.0-1\n"));
    // When most rows are long, stacking reads better than a ragged table.
    var ragged = Table.init(&.{ .{ .title = "Package" }, .{ .title = "Version" } });
    ragged.addText(&.{ "short", "1.0-1" });
    ragged.addText(&.{ "a-package-with-a-long-name", "2.0-1" });
    try std.testing.expect(!ragged.fits(24));
    try std.testing.expect(ragged.fits(40));
}

test "fields wrap beneath their value and skip empty optional values" {
    defer setCaps(null);
    rendered(60);
    field("Depends on", "glibc  libgcc  pcre2  a-rather-long-name");
    field("Groups", "");
    fieldOptional("Conflicts", "");
    fieldList("Provides", &.{});
    fieldLines("Optional", &.{ "sudo: elevation", "doas: elevation" });
    try std.testing.expectEqualStrings(
        \\  Depends on          glibc  libgcc  pcre2
        \\                      a-rather-long-name
        \\  Groups              none
        \\  Optional            sudo: elevation
        \\                      doas: elevation
        \\
    , endCapture());
    // Narrow terminals stack the value beneath its label.
    rendered(30);
    field("Depends on", "glibc  libgcc");
    try std.testing.expectEqualStrings("  Depends on\n    glibc  libgcc\n", endCapture());
}

test "entries keep identity, badges and metadata readable at any width" {
    defer setCaps(null);
    const sample: Entry = .{
        .index = 3,
        .source = "extra",
        .name = "ripgrep",
        .version = "15.2.0-1",
        .badges = &.{.{ .text = "✓ installed", .tone = .success }},
        .description = "A search tool",
        .meta = "4.3 MiB · built 2026-07-16",
    };
    rendered(80);
    entry(sample);
    try std.testing.expectEqualStrings(
        \\   3 extra/ripgrep 15.2.0-1  ✓ installed
        \\         A search tool
        \\         4.3 MiB · built 2026-07-16
        \\
    , endCapture());
    rendered(24);
    entry(sample);
    try std.testing.expectEqualStrings(
        \\   3 extra/ripgrep
        \\         15.2.0-1
        \\         ✓ installed
        \\         A search tool
        \\         4.3 MiB · built
        \\         2026-07-16
        \\
    , endCapture());
}

test "glyphs fall back to single-cell ASCII without changing alignment" {
    defer setCaps(null);
    setCaps(.{ .width = 40, .unicode = false });
    beginCapture();
    heading("Updates", "2 packages");
    note(.success, "Done · 1 → 2");
    const output = endCapture();
    try std.testing.expectEqualStrings("\n> Updates  2 packages\n  + Done - 1 > 2\n", output);
    for (output) |byte| try std.testing.expect(byte < 0x80);
}

test "an 8-bit locale receives ASCII only, so UTF-8 bytes cannot act as C1 controls" {
    defer setCaps(null);
    setCaps(.{ .width = 40, .unicode = false, .utf8 = false });
    beginCapture();
    // U+011B is C4 9B; 0x9B alone is CSI on an 8-bit terminal.
    text("caf\u{e9} \u{11b} \u{65e5}", 2, .plain);
    try std.testing.expectEqualStrings("  caf? ? ?\n", endCapture());
}

test "headings never cut their title or detail, and raw fields keep tokens whole" {
    _ = c.setlocale(c.LC_CTYPE, "C.UTF-8");
    defer setCaps(null);
    rendered(44);
    heading("Review 1/2 · some-package", "1.0-1");
    heading("Review 1/2 · a-package-with-a-long-name", "2026.11.14.r123.gabcdef-1");
    try std.testing.expectEqualStrings(
        \\
        \\▌ Review 1/2 · some-package  1.0-1
        \\
        \\▌ Review 1/2 · a-package-with-a-long-name
        \\  2026.11.14.r123.gabcdef-1
        \\
    , endCapture());
    const digest = "9400afb49bd60984062972cbaf220808f39011fcc5946c089d6efccc87ef047d";
    rendered(90);
    fieldRaw("SHA-256", digest);
    rendered(60);
    fieldRaw("SHA-256", digest);
    const expected = "  SHA-256             " ++ digest ++ "\n" ++ "  SHA-256\n    " ++ digest ++ "\n";
    try std.testing.expectEqualStrings(expected, endCapture());
}

test "stacked rows join a version change with an arrow" {
    defer setCaps(null);
    var table = Table.init(&.{ .{ .title = "Package" }, .{ .title = "Current" }, .{ .title = "New", .joiner = " → " }, .{ .title = "Source" } });
    table.addText(&.{ "cpak-bin", "2.14.6-1", "2.14.18-1", "aur" });
    table.addText(&.{ "a-much-longer-package-name", "1-1", "2-1", "aur" });
    rendered(30);
    table.render();
    try std.testing.expectEqualStrings(
        \\  cpak-bin
        \\    2.14.6-1 → 2.14.18-1 · aur
        \\  a-much-longer-package-name
        \\    1-1 → 2-1 · aur
        \\
    , endCapture());
    // Wrapping between cells leaves no separator at a line end; the arrow
    // starts the line it wraps to.
    var narrow = Table.init(table.columns);
    narrow.addText(&.{ "cpak-bin", "2.14.6-1", "2.14.18-1", "aur" });
    rendered(18);
    narrow.render();
    try std.testing.expectEqualStrings(
        \\  cpak-bin
        \\    2.14.6-1
        \\    → 2.14.18-1
        \\    aur
        \\
    , endCapture());
}

test "sizes, counts and ages read naturally" {
    try std.testing.expectEqualStrings("0 B", size(0));
    try std.testing.expectEqualStrings("512 B", size(512));
    try std.testing.expectEqualStrings("1.5 KiB", size(1536));
    try std.testing.expectEqualStrings("4.3 MiB", size(4_508_877));
    try std.testing.expectEqualStrings("1.71 GiB", size(1_838_887_731));
    try std.testing.expectEqualStrings("-2.0 MiB", sizeDelta(-2 * 1024 * 1024));
    try std.testing.expectEqualStrings("+2.0 MiB", sizeDelta(2 * 1024 * 1024));
    try std.testing.expectEqualStrings("1 package", count(1, "package", "packages"));
    try std.testing.expectEqualStrings("0 packages", count(0, "package", "packages"));
    const day = 86400;
    const now = 1_800_000_000;
    try std.testing.expectEqualStrings("today", age(now - 3600, now));
    try std.testing.expectEqualStrings("today", age(now + 3600, now));
    try std.testing.expectEqualStrings("yesterday", age(now - day, now));
    try std.testing.expectEqualStrings("13 days ago", age(now - 13 * day, now));
    try std.testing.expectEqualStrings("3 weeks ago", age(now - 21 * day, now));
    try std.testing.expectEqualStrings("4 months ago", age(now - 120 * day, now));
    try std.testing.expectEqualStrings("3 years ago", age(now - 3 * 365 * day, now));
    try std.testing.expectEqualStrings("unknown", age(0, now));
    try std.testing.expectEqualStrings("2025-06-15", date(1750000000));
    try std.testing.expectEqualStrings("2025-06-15 15:06 UTC", datetime(1750000000));
    try std.testing.expectEqualStrings("unknown", date(0));
}

test "repeated progress identities support long names and distinguish phase and count" {
    const name = "日本語-terminal-with-an-extraordinarily-long-name";
    const key = progressKey("Upgrading", name, 1, 3);
    for (0..10) |_| try std.testing.expectEqual(key, progressKey("Upgrading", name, 1, 3));
    try std.testing.expect(key != progressKey("Installing", name, 1, 3));
    try std.testing.expect(key != progressKey("Upgrading", name, 2, 3));
    try std.testing.expect(key != progressKey("Upgrading", name, 1, 4));
}

test "styled entries link the name only to a plain https page" {
    defer setCaps(null);
    setCaps(.{ .width = 80, .color = true });
    beginCapture();
    entry(.{ .source = "aur", .name = "zap", .link = "https://aur.archlinux.org/packages/zap", .version = "1-1" });
    try std.testing.expect(std.mem.indexOf(u8, endCapture(), "\x1b]8;;https://aur.archlinux.org/packages/zap\x1b\\") != null);
    beginCapture();
    entry(.{ .source = "aur", .name = "zap", .link = "https://evil\x07\x1b]8;;x", .version = "1-1" });
    try std.testing.expect(std.mem.indexOf(u8, endCapture(), "\x1b]8") == null);
}
