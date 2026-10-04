const std = @import("std");
const u = @import("util.zig");

pub const Command = enum { help, version, search, select, info, localinfo, install, install_files, build, build_local, install_local, get, pkgbuild, upgrade, updates, list, foreign, local_search, files, owns, check, remove, orphans, autoremove, clean, stats, news, reason };
pub const Scope = enum { all, aur, repo };
pub const Reason = enum { explicit, dependency };
pub const Options = struct {
    command: Command,
    names: []const []const u8 = &.{},
    scope: Scope = .all,
    dry: bool = false,
    devel: bool = false,
    recursive: bool = false,
    nosave: bool = false,
    quiet: bool = false,
    needed: bool = false,
    sandbox: ?bool = null,
    clean_after: bool = false,
    rebuild_tree: bool = false,
    reason: ?Reason = null,
    search_by: []const u8 = "name-desc",
    sort_by: []const u8 = "votes",
};

pub fn parse(args: []const []const u8) !Options {
    if (args.len == 0) return .{ .command = .help };
    var o: Options = .{ .command = try command(args[0]) };
    if (std.mem.eql(u8, args[0], "-Sua") or std.mem.eql(u8, args[0], "-Qua")) o.scope = .aur;
    if (std.mem.eql(u8, args[0], "-Qq") or std.mem.eql(u8, args[0], "-Qmq") or std.mem.eql(u8, args[0], "-Qdtq")) o.quiet = true;
    if (std.mem.eql(u8, args[0], "-Rns")) {
        o.recursive = true;
        o.nosave = true;
    }
    var names: std.ArrayList([]const u8) = .empty;
    var positional = false;
    var i: usize = 1;
    while (i < args.len) : (i += 1) {
        const arg = args[i];
        if (!positional and std.mem.eql(u8, arg, "--")) {
            positional = true;
            continue;
        }
        if (positional or !std.mem.startsWith(u8, arg, "-")) {
            try names.append(u.a, arg);
            continue;
        }
        if (std.mem.eql(u8, arg, "--dry-run")) o.dry = true else if (std.mem.eql(u8, arg, "--devel")) o.devel = true else if (std.mem.eql(u8, arg, "--recursive")) o.recursive = true else if (std.mem.eql(u8, arg, "--nosave")) o.nosave = true else if (std.mem.eql(u8, arg, "--quiet") or std.mem.eql(u8, arg, "-q")) o.quiet = true else if (std.mem.eql(u8, arg, "--needed")) o.needed = true else if (std.mem.eql(u8, arg, "--rebuildtree")) o.rebuild_tree = true else if (std.mem.eql(u8, arg, "--sandbox") or std.mem.eql(u8, arg, "--no-sandbox")) {
            const enabled = std.mem.eql(u8, arg, "--sandbox");
            if (o.sandbox != null and o.sandbox.? != enabled) return error.ConflictingOptions;
            o.sandbox = enabled;
        } else if (std.mem.eql(u8, arg, "--cleanafter")) o.clean_after = true else if (std.mem.eql(u8, arg, "--aur") or std.mem.eql(u8, arg, "-a")) {
            if (o.scope == .repo) return error.ConflictingOptions;
            o.scope = .aur;
        } else if (std.mem.eql(u8, arg, "--repo")) {
            if (o.scope == .aur) return error.ConflictingOptions;
            o.scope = .repo;
        } else if (std.mem.eql(u8, arg, "--asdeps") or std.mem.eql(u8, arg, "--asexplicit")) {
            const reason: Reason = if (std.mem.eql(u8, arg, "--asdeps")) .dependency else .explicit;
            if (o.reason != null and o.reason.? != reason) return error.ConflictingOptions;
            o.reason = reason;
        } else if (std.mem.eql(u8, arg, "--searchby") or std.mem.eql(u8, arg, "--sortby")) {
            if (o.command != .search and o.command != .select) return error.OptionNotValidForCommand;
            i += 1;
            if (i == args.len) return error.OptionValueRequired;
            if (std.mem.eql(u8, arg, "--searchby")) {
                if (!oneOf(args[i], &.{ "name", "name-desc", "maintainer", "depends", "makedepends", "checkdepends", "optdepends", "provides" })) return error.InvalidSearchField;
                o.search_by = args[i];
            } else {
                if (!oneOf(args[i], &.{ "votes", "popularity", "name", "modified" })) return error.InvalidSortField;
                o.sort_by = args[i];
            }
        } else return error.UnknownOption;
    }
    o.names = try names.toOwnedSlice(u.a);
    try validate(o);
    return o;
}
fn oneOf(value: []const u8, values: []const []const u8) bool {
    for (values) |v| if (std.mem.eql(u8, value, v)) return true;
    return false;
}
fn command(value: []const u8) !Command {
    inline for (std.meta.fields(Command)) |field| {
        if (std.mem.eql(u8, value, field.name)) return @enumFromInt(field.value);
    }
    const aliases = .{
        .{ "-B", Command.build_local },              .{ "-Bi", Command.install_local }, .{ "build-local", Command.build_local }, .{ "install-local", Command.install_local },
        .{ "-h", Command.help },                     .{ "--help", Command.help },       .{ "--version", Command.version },       .{ "-Ss", Command.search },
        .{ "-Si", Command.info },                    .{ "-Qi", Command.localinfo },     .{ "-S", Command.install },              .{ "-U", Command.install_files },
        .{ "install-files", Command.install_files }, .{ "-G", Command.get },            .{ "-Gp", Command.pkgbuild },            .{ "-Syu", Command.upgrade },
        .{ "-Sua", Command.upgrade },                .{ "-Qu", Command.updates },       .{ "-Qua", Command.updates },            .{ "-Q", Command.list },
        .{ "-Qq", Command.list },                    .{ "-Qm", Command.foreign },       .{ "-Qmq", Command.foreign },            .{ "-Qs", Command.local_search },
        .{ "local-search", Command.local_search },   .{ "-Ql", Command.files },         .{ "-Qo", Command.owns },                .{ "-Qk", Command.check },
        .{ "-R", Command.remove },                   .{ "-Rns", Command.remove },       .{ "-Qdt", Command.orphans },            .{ "-Qdtq", Command.orphans },
        .{ "-Sc", Command.clean },                   .{ "-Ps", Command.stats },         .{ "-Pw", Command.news },                .{ "-D", Command.reason },
    };
    inline for (aliases) |pair| if (std.mem.eql(u8, value, pair[0])) return pair[1];
    return error.UnknownCommand;
}
pub fn validate(o: Options) !void {
    const query = o.command == .search or o.command == .select;
    const install = o.command == .install or o.command == .install_files or o.command == .install_local;
    const build = o.command == .build or o.command == .install or o.command == .select or o.command == .upgrade or o.command == .build_local or o.command == .install_local;
    if (o.scope != .all and !query and o.command != .info and o.command != .install and o.command != .upgrade and o.command != .updates) return error.OptionNotValidForCommand;
    if (o.devel and o.command != .updates and o.command != .upgrade) return error.OptionNotValidForCommand;
    if ((o.recursive or o.nosave) and o.command != .remove) return error.OptionNotValidForCommand;
    if (o.reason != null and !install and o.command != .reason and o.command != .select) return error.OptionNotValidForCommand;
    if (o.command == .reason and o.reason == null) return error.InstallationReasonRequired;
    if (o.rebuild_tree and !build) return error.OptionNotValidForCommand;
    if (o.needed and o.rebuild_tree) return error.ConflictingOptions;
    if (o.needed and o.command == .install_local) return error.OptionNotValidForCommand;
    if (o.clean_after and (o.command == .build or o.command == .build_local)) return error.OptionNotValidForCommand;
    if (o.needed and !install and o.command != .select) return error.OptionNotValidForCommand;
    if ((o.sandbox != null or o.clean_after) and !build) return error.OptionNotValidForCommand;
    if (o.quiet and o.command != .search and o.command != .local_search and o.command != .updates and o.command != .list and o.command != .foreign and o.command != .orphans and o.command != .files and o.command != .owns) return error.OptionNotValidForCommand;
    if ((!std.mem.eql(u8, o.search_by, "name-desc") or !std.mem.eql(u8, o.sort_by, "votes")) and !query) return error.OptionNotValidForCommand;
    if (o.dry and !build and o.command != .install_files and o.command != .remove and o.command != .autoremove and o.command != .clean and o.command != .reason) return error.OptionNotValidForCommand;
    switch (o.command) {
        .help, .version, .upgrade, .updates, .orphans, .autoremove, .stats, .news => if (o.names.len != 0) return error.UnexpectedTargets,
        .search, .select, .local_search => if (o.names.len == 0) return error.SearchTermRequired,
        .list, .foreign, .check, .clean => {},
        else => if (o.names.len == 0) return error.PackageNameRequired,
    }
}
test "CLI scopes, aliases and contradictory or ignored flags" {
    const scoped = try parse(&.{ "-Sua", "--devel", "--dry-run" });
    try std.testing.expectEqual(Scope.aur, scoped.scope);
    try std.testing.expect(scoped.dry and scoped.devel);
    try std.testing.expect((try parse(&.{"-Qmq"})).quiet);
    try std.testing.expectError(error.ConflictingOptions, parse(&.{ "install", "foo", "--asdeps", "--asexplicit" }));
    try std.testing.expectError(error.ConflictingOptions, parse(&.{ "-Sua", "--repo" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "get", "foo", "--dry-run" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "upgrade", "--asdeps" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "search", "foo", "--sandbox" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "info", "foo", "--sortby", "votes" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "build", "foo", "--cleanafter" }));
    try std.testing.expectError(error.ConflictingOptions, parse(&.{ "install", "foo", "--needed", "--rebuildtree" }));
    try std.testing.expectEqual(Command.install_local, (try parse(&.{ "-Bi", ".", "--sandbox" })).command);
    try std.testing.expectError(error.OptionValueRequired, parse(&.{ "search", "foo", "--sortby" }));
    try std.testing.expectError(error.UnknownOption, parse(&.{ "install", "foo", "--noconfirm" }));
    try std.testing.expectError(error.InvalidSearchField, parse(&.{ "search", "foo", "--searchby", "name&arg=evil" }));
    try std.testing.expectEqualStrings("-archive.pkg.tar", (try parse(&.{ "-U", "--", "-archive.pkg.tar" })).names[0]);
}
