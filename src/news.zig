const std = @import("std");
const u = @import("util.zig");
const ui = @import("ui.zig");
pub const Item = struct { title: []const u8, link: []const u8, date: []const u8 };

// Deliberately limited to Arch's RSS format: no DTD, external entities, HTML
// renderer, scripts, or general XML entity expansion.
pub fn decode(bytes: []const u8) ![]Item {
    if (bytes.len > 1024 * 1024 or std.mem.indexOf(u8, bytes, "<!DOCTYPE") != null or std.mem.indexOf(u8, bytes, "<!ENTITY") != null or
        std.mem.indexOf(u8, bytes, "<rss ") == null or std.mem.indexOf(u8, bytes, "</rss>") == null) return error.InvalidNewsFeed;
    var items: std.ArrayList(Item) = .empty;
    var rest = bytes;
    while (std.mem.indexOf(u8, rest, "<item>")) |start| {
        rest = rest[start + 6 ..];
        const end = std.mem.indexOf(u8, rest, "</item>") orelse return error.InvalidNewsFeed;
        const item = rest[0..end];
        const link = try text(try field(item, "link"));
        if (!std.mem.startsWith(u8, link, "https://archlinux.org/news/") or std.mem.indexOfAny(u8, link, "\r\n\t <>\"\\") != null) return error.InvalidNewsFeed;
        try items.append(u.a, .{ .title = try text(try field(item, "title")), .link = link, .date = try text(try field(item, "pubDate")) });
        if (items.items.len > 100) return error.InvalidNewsFeed;
        rest = rest[end + 7 ..];
    }
    if (items.items.len == 0) return error.InvalidNewsFeed;
    return items.toOwnedSlice(u.a);
}
fn field(item: []const u8, name: []const u8) ![]const u8 {
    const open = try std.fmt.allocPrint(u.a, "<{s}>", .{name});
    const close = try std.fmt.allocPrint(u.a, "</{s}>", .{name});
    const start = (std.mem.indexOf(u8, item, open) orelse return error.InvalidNewsFeed) + open.len;
    const end = std.mem.indexOfPos(u8, item, start, close) orelse return error.InvalidNewsFeed;
    if (end - start > 4096) return error.InvalidNewsFeed;
    return item[start..end];
}
fn text(input: []const u8) ![]const u8 {
    var out: std.ArrayList(u8) = .empty;
    var i: usize = 0;
    while (i < input.len) : (i += 1) {
        if (input[i] == '<') return error.InvalidNewsFeed;
        if (input[i] != '&') {
            try out.append(u.a, input[i]);
            continue;
        }
        const end = std.mem.indexOfScalarPos(u8, input, i, ';') orelse return error.InvalidNewsFeed;
        const entity = input[i + 1 .. end];
        if (std.mem.eql(u8, entity, "amp")) try out.append(u.a, '&') else if (std.mem.eql(u8, entity, "lt")) try out.append(u.a, '<') else if (std.mem.eql(u8, entity, "gt")) try out.append(u.a, '>') else if (std.mem.eql(u8, entity, "quot")) try out.append(u.a, '"') else if (std.mem.eql(u8, entity, "apos")) try out.append(u.a, '\'') else if (std.mem.startsWith(u8, entity, "#")) {
            const hex = std.mem.startsWith(u8, entity, "#x");
            const point = std.fmt.parseInt(u21, entity[if (hex) 2 else 1..], if (hex) 16 else 10) catch return error.InvalidNewsFeed;
            var encoded: [4]u8 = undefined;
            const n = std.unicode.utf8Encode(point, &encoded) catch return error.InvalidNewsFeed;
            try out.appendSlice(u.a, encoded[0..n]);
        } else return error.InvalidNewsFeed;
        i = end;
    }
    return out.toOwnedSlice(u.a);
}
pub fn show() !void {
    const items = try decode(try @import("aur.zig").get("https://archlinux.org/feeds/news/"));
    ui.title("Recent Arch Linux news · read intervention notices before upgrading");
    for (items[0..@min(10, items.len)]) |item| ui.print("{s}\n  {s}\n  {s}\n\n", .{ ui.safe(item.title), ui.safe(item.date), ui.safe(item.link) });
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
}
