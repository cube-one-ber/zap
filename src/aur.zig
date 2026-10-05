const std = @import("std");
const u = @import("util.zig");
const ui = @import("ui.zig");
const c = u.c;
pub const Package = struct {
    Name: []const u8,
    PackageBase: []const u8,
    Version: []const u8,
    Description: ?[]const u8 = null,
    URL: ?[]const u8 = null,
    Maintainer: ?[]const u8 = null,
    NumVotes: i64 = 0,
    Popularity: f64 = 0,
    FirstSubmitted: i64 = 0,
    LastModified: i64 = 0,
    OutOfDate: ?i64 = null,
    Depends: []const []const u8 = &.{},
    MakeDepends: []const []const u8 = &.{},
    CheckDepends: []const []const u8 = &.{},
    OptDepends: []const []const u8 = &.{},
    Provides: []const []const u8 = &.{},
    Conflicts: []const []const u8 = &.{},
    License: []const []const u8 = &.{},
    pub fn show(self: Package) void {
        self.showSearch(null, null);
    }
    pub fn showSearch(self: Package, index: ?usize, installed: ?[]const u8) void {
        const badge = if (installed) |old| std.fmt.allocPrint(u.a, "[installed{s}{s}]", .{ if (std.mem.eql(u8, old, self.Version)) @as([]const u8, "") else ": ", if (std.mem.eql(u8, old, self.Version)) @as([]const u8, "") else old }) catch return else "";
        defer if (installed != null) u.a.free(badge);
        ui.packageHeader(self.Name, self.Version, "aur", index, badge);
        if (self.Description) |description| ui.text(description, 4, .reset);
        const meta = std.fmt.allocPrint(u.a, "{d} votes · popularity {d:.2} · {s}", .{ self.NumVotes, self.Popularity, self.Maintainer orelse "unmaintained" }) catch return;
        defer u.a.free(meta);
        ui.text(meta, 4, .muted);
        const stamp = std.fmt.allocPrint(u.a, "Modified {s}", .{ui.date(self.LastModified)}) catch return;
        defer u.a.free(stamp);
        ui.text(stamp, 4, .muted);
        if (self.Maintainer == null) ui.note(.warning, "Unmaintained package");
        if (self.OutOfDate != null) ui.note(.warning, "Flagged out of date");
    }
};
const Response = struct { version: i64, type: []const u8, resultcount: usize = 0, results: []Package = &.{}, @"error": ?[]const u8 = null };
pub fn decode(bytes: []const u8) ![]Package {
    const parsed = try std.json.parseFromSlice(Response, u.a, bytes, .{ .ignore_unknown_fields = true, .allocate = .alloc_always });
    if (parsed.value.version != 5 or std.mem.eql(u8, parsed.value.type, "error")) {
        ui.print("AUR: {s}\n", .{ui.safe(parsed.value.@"error" orelse "unsupported API response")});
        return error.AurApiError;
    }
    if (parsed.value.results.len != parsed.value.resultcount) return error.InvalidAurResponse;
    for (parsed.value.results) |p| if (!u.validName(p.Name) or !u.validName(p.PackageBase) or p.LastModified < 0) return error.InvalidAurResponse;
    return parsed.value.results;
}
pub fn encode(s: []const u8) ![]const u8 {
    var out: std.ArrayList(u8) = .empty;
    const hex = "0123456789ABCDEF";
    for (s) |ch| {
        if (std.ascii.isAlphanumeric(ch) or std.mem.indexOfScalar(u8, "-_.~", ch) != null) try out.append(u.a, ch) else try out.appendSlice(u.a, &.{ '%', hex[ch >> 4], hex[ch & 15] });
    }
    return out.toOwnedSlice(u.a);
}
pub fn search(term: []const u8, by: []const u8) ![]Package {
    const url = try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/rpc/v5/search/{s}?by={s}", .{ try encode(term), by });
    return decode(try get(url));
}
pub fn info(names: []const []const u8) ![]Package {
    var all: std.ArrayList(Package) = .empty;
    var start: usize = 0;
    while (start < names.len) {
        const end = @min(start + 100, names.len);
        var url: std.ArrayList(u8) = .empty;
        try url.appendSlice(u.a, "https://aur.archlinux.org/rpc/v5/info?");
        for (names[start..end]) |name| {
            try url.appendSlice(u.a, "arg[]=");
            try url.appendSlice(u.a, try encode(name));
            try url.append(u.a, '&');
        }
        try all.appendSlice(u.a, try decode(try get(url.items)));
        start = end;
    }
    return all.toOwnedSlice(u.a);
}
const Buffer = struct { bytes: std.ArrayList(u8) = .empty };
fn receive(data: [*c]u8, size: usize, count: usize, ctx: ?*anyopaque) callconv(.c) usize {
    const buffer: *Buffer = @ptrCast(@alignCast(ctx.?));
    const n = std.math.mul(usize, size, count) catch return 0;
    if (n > 16 * 1024 * 1024 - buffer.bytes.items.len) return 0;
    buffer.bytes.appendSlice(u.a, data[0..n]) catch return 0;
    return n;
}
pub fn get(url: []const u8) ![]const u8 {
    if (c.curl_global_init(c.CURL_GLOBAL_DEFAULT) != c.CURLE_OK) return error.CurlInitializationFailed;
    defer c.curl_global_cleanup();
    const handle = c.curl_easy_init() orelse return error.CurlInitializationFailed;
    defer c.curl_easy_cleanup(handle);
    var buffer: Buffer = .{};
    errdefer buffer.bytes.deinit(u.a);
    try option(c.curl_easy_setopt(handle, c.CURLOPT_URL, (try u.z(url)).ptr));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_PROTOCOLS_STR, @as([*:0]const u8, "https")));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_REDIR_PROTOCOLS_STR, @as([*:0]const u8, "https")));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_USERAGENT, @as([*:0]const u8, "zap/0.1 (Arch Linux AUR helper)")));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_CONNECTTIMEOUT, @as(c_long, 15)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_TIMEOUT, @as(c_long, 60)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_FAILONERROR, @as(c_long, 1)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_FOLLOWLOCATION, @as(c_long, 1)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_MAXREDIRS, @as(c_long, 3)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_SSL_VERIFYPEER, @as(c_long, 1)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_SSL_VERIFYHOST, @as(c_long, 2)));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_WRITEFUNCTION, &receive));
    try option(c.curl_easy_setopt(handle, c.CURLOPT_WRITEDATA, &buffer));
    const result = c.curl_easy_perform(handle);
    if (result != c.CURLE_OK) {
        ui.print("AUR request: {s}\n", .{u.str(c.curl_easy_strerror(result))});
        return error.AurRequestFailed;
    }
    return buffer.bytes.toOwnedSlice(u.a);
}
fn option(result: c.CURLcode) !void {
    if (result != c.CURLE_OK) return error.CurlOptionFailed;
}
test "AUR responses retain last modification, split bases, and dependencies" {
    const bytes = @embedFile("fixtures/aur.json");
    const result = try decode(bytes);
    try std.testing.expectEqual(@as(usize, 1), result.len);
    try std.testing.expectEqualStrings("sample-base", result[0].PackageBase);
    try std.testing.expectEqual(@as(i64, 1750000000), result[0].LastModified);
    try std.testing.expectEqualStrings("libfoo>=2", result[0].Depends[0]);
}
test "RPC rejects malicious package paths and API errors" {
    try std.testing.expectError(error.InvalidAurResponse, decode("{\"version\":5,\"type\":\"info\",\"resultcount\":1,\"results\":[{\"Name\":\"../bad\",\"PackageBase\":\"bad\",\"Version\":\"1\"}]}"));
    try std.testing.expectError(error.AurApiError, decode("{\"version\":5,\"type\":\"error\",\"error\":\"rate limited\"}"));
    try std.testing.expectEqualStrings("foo%2Bbar%26x", try encode("foo+bar&x"));
}
