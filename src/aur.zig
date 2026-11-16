//! The AUR RPC interface, version 5.
const std = @import("std");
const u = @import("util.zig");
const cli = @import("cli.zig");
const net = @import("net.zig");
const errors = @import("errors.zig");

const endpoint = "https://aur.archlinux.org/rpc/v5";
/// The RPC caps an info request's URL length; batches stay well below it.
const info_batch = 100;

/// Field names follow the RPC's JSON.
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
};

const Response = struct {
    version: i64,
    type: []const u8,
    resultcount: usize = 0,
    results: []Package = &.{},
    @"error": ?[]const u8 = null,
};

/// Decode a response, rejecting API errors and names that could not be used
/// safely as paths or arguments.
pub fn decode(bytes: []const u8) ![]Package {
    const parsed = std.json.parseFromSlice(Response, u.a, bytes, .{ .ignore_unknown_fields = true, .allocate = .alloc_always }) catch |err| {
        return if (err == error.OutOfMemory) err else error.InvalidAurResponse;
    };
    const response = parsed.value;
    if (response.version != 5 or std.mem.eql(u8, response.type, "error")) {
        return errors.with(error.AurApiError, "{s}", .{response.@"error" orelse "unsupported API response"});
    }
    if (response.results.len != response.resultcount) return error.InvalidAurResponse;
    for (response.results) |p| {
        if (!u.validName(p.Name) or !u.validName(p.PackageBase) or p.LastModified < 0) return error.InvalidAurResponse;
    }
    return response.results;
}

/// Percent-encode everything outside the URL-safe unreserved set.
pub fn encode(s: []const u8) ![]const u8 {
    var out: std.ArrayList(u8) = .empty;
    const hex = "0123456789ABCDEF";
    for (s) |ch| {
        if (std.ascii.isAlphanumeric(ch) or std.mem.indexOfScalar(u8, "-_.~", ch) != null) {
            try out.append(u.a, ch);
        } else try out.appendSlice(u.a, &.{ '%', hex[ch >> 4], hex[ch & 15] });
    }
    return out.toOwnedSlice(u.a);
}

pub fn search(term: []const u8, by: cli.SearchBy) ![]Package {
    const url = try std.fmt.allocPrint(u.a, "{s}/search/{s}?by={s}", .{ endpoint, try encode(term), @tagName(by) });
    return decode(try net.get(url));
}

/// Full metadata for the named packages. Unknown names are simply absent
/// from the result, which is in no particular order.
pub fn info(names: []const []const u8) ![]Package {
    var all: std.ArrayList(Package) = .empty;
    var start: usize = 0;
    while (start < names.len) {
        const stop = @min(start + info_batch, names.len);
        var url: std.ArrayList(u8) = .empty;
        try url.appendSlice(u.a, endpoint ++ "/info?");
        for (names[start..stop]) |name| {
            try url.appendSlice(u.a, "arg[]=");
            try url.appendSlice(u.a, try encode(name));
            try url.append(u.a, '&');
        }
        try all.appendSlice(u.a, try decode(try net.get(url.items)));
        start = stop;
    }
    return all.toOwnedSlice(u.a);
}

/// The one package with exactly this name, if the AUR has it.
pub fn find(name: []const u8) !?Package {
    for (try info(&.{name})) |p| if (std.mem.eql(u8, p.Name, name)) return p;
    return null;
}

test "AUR responses retain last modification, split bases, and dependencies" {
    const result = try decode(@embedFile("fixtures/aur.json"));
    try std.testing.expectEqual(@as(usize, 1), result.len);
    try std.testing.expectEqualStrings("sample-base", result[0].PackageBase);
    try std.testing.expectEqual(@as(i64, 1750000000), result[0].LastModified);
    try std.testing.expectEqualStrings("libfoo>=2", result[0].Depends[0]);
}

test "RPC rejects malicious package paths, API errors and malformed bodies" {
    try std.testing.expectError(error.InvalidAurResponse, decode("{\"version\":5,\"type\":\"info\",\"resultcount\":1,\"results\":[{\"Name\":\"../bad\",\"PackageBase\":\"bad\",\"Version\":\"1\"}]}"));
    try std.testing.expectError(error.AurApiError, decode("{\"version\":5,\"type\":\"error\",\"error\":\"rate limited\"}"));
    try std.testing.expectError(error.InvalidAurResponse, decode("<html>gateway timeout</html>"));
    try std.testing.expectError(error.InvalidAurResponse, decode("{\"version\":5,\"type\":\"info\",\"resultcount\":2,\"results\":[]}"));
    try std.testing.expectEqualStrings("foo%2Bbar%26x", try encode("foo+bar&x"));
}
