//! Arch Linux news, from the official RSS feed.
const std = @import("std");
const u = @import("util.zig");
const ui = @import("ui.zig");
const net = @import("net.zig");

const feed = "https://archlinux.org/feeds/news/";
const shown = 10;

pub const Item = struct { title: []const u8, link: []const u8, date: []const u8 };

/// Deliberately limited to Arch's RSS format: no DTD, external entities, HTML
/// renderer, scripts, or general XML entity expansion.
pub fn decode(bytes: []const u8) ![]Item {
    if (bytes.len > 1024 * 1024) return error.InvalidNewsFeed;
    for ([_][]const u8{ "<!DOCTYPE", "<!ENTITY" }) |forbidden| if (std.mem.indexOf(u8, bytes, forbidden) != null) return error.InvalidNewsFeed;
    for ([_][]const u8{ "<rss ", "</rss>" }) |required| if (std.mem.indexOf(u8, bytes, required) == null) return error.InvalidNewsFeed;
    var items: std.ArrayList(Item) = .empty;
    var rest = bytes;
    while (std.mem.indexOf(u8, rest, "<item>")) |start| {
        rest = rest[start + "<item>".len ..];
        const stop = std.mem.indexOf(u8, rest, "</item>") orelse return error.InvalidNewsFeed;
        const item = rest[0..stop];
        const link = try text(try field(item, "link"));
        // Links are shown for the reader to open: they must stay on archlinux.org.
        if (!std.mem.startsWith(u8, link, "https://archlinux.org/news/") or std.mem.indexOfAny(u8, link, "\r\n\t <>\"\\") != null) return error.InvalidNewsFeed;
        try items.append(u.a, .{ .title = try text(try field(item, "title")), .link = link, .date = try text(try field(item, "pubDate")) });
        if (items.items.len > 100) return error.InvalidNewsFeed;
        rest = rest[stop + "</item>".len ..];
    }
    if (items.items.len == 0) return error.InvalidNewsFeed;
    return items.toOwnedSlice(u.a);
}

fn field(item: []const u8, name: []const u8) ![]const u8 {
    const open = try std.fmt.allocPrint(u.a, "<{s}>", .{name});
    const close = try std.fmt.allocPrint(u.a, "</{s}>", .{name});
    const start = (std.mem.indexOf(u8, item, open) orelse return error.InvalidNewsFeed) + open.len;
    const stop = std.mem.indexOfPos(u8, item, start, close) orelse return error.InvalidNewsFeed;
    if (stop - start > 4096) return error.InvalidNewsFeed;
    return item[start..stop];
}

/// Decode the five predefined entities and numeric references; reject markup.
fn text(input: []const u8) ![]const u8 {
    var out: std.ArrayList(u8) = .empty;
    var i: usize = 0;
    while (i < input.len) : (i += 1) {
        if (input[i] == '<') return error.InvalidNewsFeed;
        if (input[i] != '&') {
            try out.append(u.a, input[i]);
            continue;
        }
        const stop = std.mem.indexOfScalarPos(u8, input, i, ';') orelse return error.InvalidNewsFeed;
        const entity = input[i + 1 .. stop];
        const named = [_]struct { []const u8, u8 }{ .{ "amp", '&' }, .{ "lt", '<' }, .{ "gt", '>' }, .{ "quot", '"' }, .{ "apos", '\'' } };
        const replacement: ?u8 = for (named) |pair| {
            if (std.mem.eql(u8, entity, pair[0])) break pair[1];
        } else null;
        if (replacement) |ch| {
            try out.append(u.a, ch);
        } else if (std.mem.startsWith(u8, entity, "#")) {
            const hex = std.mem.startsWith(u8, entity, "#x");
            const point = std.fmt.parseInt(u21, entity[if (hex) 2 else 1..], if (hex) 16 else 10) catch return error.InvalidNewsFeed;
            var encoded: [4]u8 = undefined;
            const n = std.unicode.utf8Encode(point, &encoded) catch return error.InvalidNewsFeed;
            try out.appendSlice(u.a, encoded[0..n]);
        } else return error.InvalidNewsFeed;
        i = stop;
    }
    return out.toOwnedSlice(u.a);
}

/// The timestamp of an RFC 822 date such as `Wed, 21 Oct 2026 13:58:42 +0000`,
/// to the day. Null when the text has another shape.
fn published(date: []const u8) ?i64 {
    var words = std.mem.tokenizeAny(u8, date, " ,");
    _ = words.next() orelse return null;
    const day = std.fmt.parseInt(u8, words.next() orelse return null, 10) catch return null;
    const month_name = words.next() orelse return null;
    const year = std.fmt.parseInt(u16, words.next() orelse return null, 10) catch return null;
    const months = [_][]const u8{ "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec" };
    const month: u8 = for (months, 1..) |candidate, number| {
        if (std.mem.eql(u8, candidate, month_name)) break @intCast(number);
    } else return null;
    if (day == 0 or day > 31 or year < 1970 or year > 9999) return null;
    // Days since the epoch for a civil date (Howard Hinnant's algorithm).
    const y: i64 = @as(i64, year) - @intFromBool(month <= 2);
    const era = @divFloor(y, 400);
    const year_of_era = y - era * 400;
    const shifted_month: i64 = if (month > 2) @as(i64, month) - 3 else @as(i64, month) + 9;
    const day_of_year = @divFloor(153 * shifted_month + 2, 5) + day - 1;
    const day_of_era = year_of_era * 365 + @divFloor(year_of_era, 4) - @divFloor(year_of_era, 100) + day_of_year;
    return (era * 146097 + day_of_era - 719468) * 86400;
}

const Urgency = enum { none, security, intervention };

/// Whether a title announces that an upgrade needs action from the reader,
/// or a security problem that an AUR helper's user should know about.
fn urgency(title: []const u8) Urgency {
    for ([_][]const u8{ "intervention", "action required", "manual", "breaking", "renamed" }) |phrase| {
        if (std.ascii.indexOfIgnoreCase(title, phrase) != null) return .intervention;
    }
    for ([_][]const u8{ "security", "vulnerab", "remote code execution", "man-in-the-middle", "malicious", "compromise" }) |phrase| {
        if (std.ascii.indexOfIgnoreCase(title, phrase) != null) return .security;
    }
    return .none;
}

pub fn show() !void {
    const items = try decode(try net.get(feed));
    const latest = items[0..@min(shown, items.len)];
    var counts = std.EnumArray(Urgency, usize).initFill(0);
    for (latest) |item| counts.set(urgency(item.title), counts.get(urgency(item.title)) + 1);
    ui.heading("Arch Linux news", "latest first");
    // The warnings come first: they are what must not be scrolled past.
    if (counts.get(.intervention) > 0) {
        ui.notef(.warning, "{s} marked ! need manual intervention. Read them before running zap upgrade.", .{ui.count(counts.get(.intervention), "item", "items")});
    }
    if (counts.get(.security) > 0) {
        const message = try std.fmt.allocPrint(u.a, "{s} marked ◆ concern security. Check whether they affect you.", .{ui.count(counts.get(.security), "item", "items")});
        ui.marked("◆", .danger, message, .plain);
    }
    if (counts.get(.intervention) == 0 and counts.get(.security) == 0) ui.note(.info, "Nothing here asks for manual intervention.");
    for (latest) |item| {
        ui.blank();
        // Every title has a mark, so the list keeps one left edge.
        switch (urgency(item.title)) {
            .intervention => ui.marked("!", .warning, item.title, .bold),
            .security => ui.marked("◆", .danger, item.title, .bold),
            .none => ui.marked("·", .muted, item.title, .bold),
        }
        ui.text(if (published(item.date)) |timestamp| ui.dated(timestamp) else item.date, 4, .muted);
        ui.line(item.link, 4, .accent);
    }
    ui.blank();
}

test "news entities are decoded without external expansion and links stay on Arch" {
    const good = "<rss version=\"2\"><item><title>Action &amp; update &gt; &#x32;</title><link>https://archlinux.org/news/action/</link><pubDate>today</pubDate></item></rss>";
    const items = try decode(good);
    try std.testing.expectEqualStrings("Action & update > 2", items[0].title);
    try std.testing.expectError(error.InvalidNewsFeed, decode("<!DOCTYPE rss><rss version=\"2\"></rss>"));
    const bad = try std.mem.replaceOwned(u8, u.a, good, "https://archlinux.org/news/action/", "https://evil.example/news/");
    try std.testing.expectError(error.InvalidNewsFeed, decode(bad));
    const missing = try std.mem.replaceOwned(u8, u.a, good, "</item>", "");
    try std.testing.expectError(error.InvalidNewsFeed, decode(missing));
    const unknown_entity = try std.mem.replaceOwned(u8, u.a, good, "&amp;", "&nbsp;");
    try std.testing.expectError(error.InvalidNewsFeed, decode(unknown_entity));
}

test "titles that ask for manual intervention or concern security are flagged" {
    try std.testing.expectEqual(Urgency.intervention, urgency("Mkinitcpio >=42 requires manual intervention for TPM2-based unlocking"));
    try std.testing.expectEqual(Urgency.intervention, urgency("Action required: the foo package moved"));
    try std.testing.expectEqual(Urgency.intervention, urgency("Breaking changes for all users of `varnish`, which is renamed to `vinyl-cache`"));
    try std.testing.expectEqual(Urgency.security, urgency("Update to php >= 8.5.12 to prevent remote code execution"));
    try std.testing.expectEqual(Urgency.security, urgency("Active AUR malicious packages incident"));
    try std.testing.expectEqual(Urgency.none, urgency("Arch Linux leader election 2026"));
}

test "publication dates are read to the day and anything else is shown verbatim" {
    try std.testing.expectEqual(@as(?i64, 0), published("Thu, 01 Jan 1970 00:00:00 +0000"));
    try std.testing.expectEqualStrings("2026-10-21", ui.date(published("Wed, 21 Oct 2026 13:58:42 +0000").?));
    try std.testing.expectEqualStrings("2024-02-29", ui.date(published("Thu, 29 Feb 2024 08:00:00 +0000").?));
    try std.testing.expectEqualStrings("2000-03-01", ui.date(published("Wed, 01 Mar 2000 00:00:00 +0000").?));
    for ([_][]const u8{ "today", "", "Wed, 21 Smarch 2026", "Wed, 99 Oct 2026", "21 Oct" }) |text_date| {
        try std.testing.expect(published(text_date) == null);
    }
}
