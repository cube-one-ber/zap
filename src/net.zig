//! HTTPS GET requests through libcurl: certificate validation, timeouts, a
//! response size limit, and one connection reused across requests.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const errors = @import("errors.zig");

const response_limit = 16 * 1024 * 1024;
const user_agent = std.fmt.comptimePrint("zap/{s} (Arch Linux AUR helper)", .{u.version});

var session: ?*c.CURL = null;

fn handle() !*c.CURL {
    if (session) |existing| return existing;
    if (c.curl_global_init(c.CURL_GLOBAL_DEFAULT) != c.CURLE_OK) return error.CurlInitializationFailed;
    const h = c.curl_easy_init() orelse return error.CurlInitializationFailed;
    errdefer c.curl_easy_cleanup(h);
    try option(c.curl_easy_setopt(h, c.CURLOPT_PROTOCOLS_STR, @as([*:0]const u8, "https")));
    try option(c.curl_easy_setopt(h, c.CURLOPT_REDIR_PROTOCOLS_STR, @as([*:0]const u8, "https")));
    try option(c.curl_easy_setopt(h, c.CURLOPT_USERAGENT, @as([*:0]const u8, user_agent)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_CONNECTTIMEOUT, @as(c_long, 15)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_TIMEOUT, @as(c_long, 60)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_FAILONERROR, @as(c_long, 1)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_FOLLOWLOCATION, @as(c_long, 1)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_MAXREDIRS, @as(c_long, 3)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_SSL_VERIFYPEER, @as(c_long, 1)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_SSL_VERIFYHOST, @as(c_long, 2)));
    try option(c.curl_easy_setopt(h, c.CURLOPT_WRITEFUNCTION, &receive));
    session = h;
    return h;
}

fn option(result: c.CURLcode) !void {
    if (result != c.CURLE_OK) return error.CurlOptionFailed;
}

const Buffer = struct { bytes: std.ArrayList(u8) = .empty };

fn receive(data: [*c]u8, size: usize, count: usize, ctx: ?*anyopaque) callconv(.c) usize {
    const buffer: *Buffer = @ptrCast(@alignCast(ctx.?));
    const n = std.math.mul(usize, size, count) catch return 0;
    if (n > response_limit - buffer.bytes.items.len) return 0;
    buffer.bytes.appendSlice(u.a, data[0..n]) catch return 0;
    return n;
}

pub fn get(url: []const u8) ![]const u8 {
    const h = try handle();
    var buffer: Buffer = .{};
    errdefer buffer.bytes.deinit(u.a);
    try option(c.curl_easy_setopt(h, c.CURLOPT_URL, (try u.z(url)).ptr));
    try option(c.curl_easy_setopt(h, c.CURLOPT_WRITEDATA, &buffer));
    const result = c.curl_easy_perform(h);
    if (result == c.CURLE_OK) return buffer.bytes.toOwnedSlice(u.a);
    var status: c_long = 0;
    _ = c.curl_easy_getinfo(h, c.CURLINFO_RESPONSE_CODE, &status);
    if (status == 429) return errors.with(error.RateLimited, "{s}", .{host(url)});
    if (result == c.CURLE_HTTP_RETURNED_ERROR) return errors.with(error.RequestFailed, "{s} answered HTTP {d}", .{ host(url), status });
    return errors.with(error.RequestFailed, "{s}: {s}", .{ host(url), u.str(c.curl_easy_strerror(result)) });
}

/// The host of an https URL, for error messages.
fn host(url: []const u8) []const u8 {
    const prefix = "https://";
    if (!std.mem.startsWith(u8, url, prefix)) return url;
    const rest = url[prefix.len..];
    return rest[0 .. std.mem.indexOfAny(u8, rest, "/?") orelse rest.len];
}

test "request failures name the host, never the full query" {
    try std.testing.expectEqualStrings("aur.archlinux.org", host("https://aur.archlinux.org/rpc/v5/info?arg[]=secret"));
    try std.testing.expectEqualStrings("archlinux.org", host("https://archlinux.org"));
}
