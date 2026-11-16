//! The command line as data: one table of commands and one of options drive
//! parsing, validation, suggestions and every help page.
const std = @import("std");
const u = @import("util.zig");
const errors = @import("errors.zig");
const ui = @import("ui.zig");

pub const Command = enum { help, version, search, select, info, localinfo, install, install_files, build, build_local, install_local, get, pkgbuild, upgrade, updates, list, foreign, local_search, files, owns, check, remove, orphans, autoremove, clean, stats, news, reason };
pub const Scope = enum { all, aur, repo };
pub const Reason = enum { explicit, dependency };
pub const SearchBy = enum { name, @"name-desc", maintainer, depends, makedepends, checkdepends, optdepends, provides };
pub const SortBy = enum { votes, popularity, name, modified };
pub const Elevator = enum { run0, sudo };

pub const Options = struct {
    command: Command,
    /// The command a help request is about, when it names one.
    topic: ?Command = null,
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
    search_by: SearchBy = .@"name-desc",
    sort_by: SortBy = .votes,
    color: ui.ColorMode = .auto,
    elevate: ?Elevator = null,
};

// ── Commands ────────────────────────────────────────────────────────────────

pub const Group = enum { everyday, inspect, build, maintain, about };
pub const Targets = enum { none, term, required, optional, topic };

pub const Spec = struct {
    command: Command,
    name: []const u8,
    alias: []const u8 = "",
    operands: []const u8 = "",
    targets: Targets = .none,
    group: Group,
    summary: []const u8,
    about: []const u8 = "",
    examples: []const []const u8 = &.{},
};

pub const specs = [_]Spec{
    .{
        .command = .search,
        .name = "search",
        .alias = "-Ss",
        .operands = "TERM",
        .targets = .term,
        .group = .everyday,
        .summary = "Find packages in repositories and the AUR",
        .about = "Searches the cached repository databases and live AUR metadata. Repository results show the installed size and build date; AUR results show votes, popularity, the maintainer and the last modification. Results you already have installed are marked.",
        .examples = &.{ "zap search ripgrep", "zap search browser --aur --sortby popularity", "zap search alice --aur --searchby maintainer" },
    },
    .{
        .command = .select,
        .name = "select",
        .operands = "TERM",
        .targets = .term,
        .group = .everyday,
        .summary = "Search, choose numbered results, then install them",
        .about = "Lists results with the best match nearest the prompt, then installs the numbers you choose. Separate choices with spaces or commas and use a dash for ranges, such as 1 3-5. An empty reply cancels.",
        .examples = &.{ "zap select browser", "zap select terminal --aur --needed" },
    },
    .{
        .command = .install,
        .name = "install",
        .alias = "-S",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .everyday,
        .summary = "Resolve dependencies, review sources, build and install",
        .about = "Repository packages are installed directly. AUR packages are fetched, shown to you for review, built as your user in a sandbox, and installed through a libalpm transaction that you confirm separately. An installed package or a repository package satisfies a dependency before the AUR is consulted.",
        .examples = &.{ "zap install ripgrep", "zap install google-chrome --dry-run", "zap install some-library --asdeps --needed" },
    },
    .{
        .command = .upgrade,
        .name = "upgrade",
        .alias = "-Syu",
        .group = .everyday,
        .summary = "Refresh and upgrade repositories, then AUR packages",
        .about = "Refreshes the repository databases and runs a full repository upgrade, then rebuilds installed AUR packages that have a newer version. Existing installation reasons are preserved. With --aur the repositories are neither refreshed nor upgraded. Read zap news first: some upgrades need manual intervention.",
        .examples = &.{ "zap upgrade", "zap upgrade --devel", "zap upgrade --aur --dry-run" },
    },
    .{
        .command = .updates,
        .name = "updates",
        .alias = "-Qu",
        .group = .everyday,
        .summary = "List available repository and AUR updates",
        .about = "Compares installed versions against the cached repository databases and live AUR metadata without changing anything. The repository databases are only as fresh as the last upgrade.",
        .examples = &.{ "zap updates", "zap updates --aur --devel", "zap updates --quiet" },
    },
    .{
        .command = .remove,
        .name = "remove",
        .alias = "-R",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .everyday,
        .summary = "Remove packages with dependency checks",
        .about = "Removes installed packages through a libalpm transaction. Packages that others still require cannot be removed, and HoldPkg entries are protected. -Rns is shorthand for --recursive --nosave.",
        .examples = &.{ "zap remove some-package", "zap remove some-package --recursive", "zap remove some-package --dry-run" },
    },
    .{
        .command = .info,
        .name = "info",
        .alias = "-Si",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .inspect,
        .summary = "Show available package details",
        .about = "Shows versions, sizes, dependencies, licenses and dates for packages in the repositories, falling back to the AUR for names the repositories do not provide.",
        .examples = &.{ "zap info ripgrep", "zap info yay --aur" },
    },
    .{
        .command = .localinfo,
        .name = "localinfo",
        .alias = "-Qi",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .inspect,
        .summary = "Show installed package details",
        .about = "Shows the installed version, installation reason and date, and which installed packages require or optionally use it.",
        .examples = &.{"zap localinfo pacman"},
    },
    .{
        .command = .list,
        .name = "list",
        .alias = "-Q",
        .operands = "[PACKAGES]",
        .targets = .optional,
        .group = .inspect,
        .summary = "List installed packages",
        .about = "Lists every installed package, or only the named ones, with its version, installation reason and installation date.",
        .examples = &.{ "zap list", "zap list pacman zap", "zap list --quiet" },
    },
    .{
        .command = .local_search,
        .name = "local-search",
        .alias = "-Qs",
        .operands = "TERM",
        .targets = .term,
        .group = .inspect,
        .summary = "Search installed names and descriptions",
        .examples = &.{"zap local-search font"},
    },
    .{
        .command = .foreign,
        .name = "foreign",
        .alias = "-Qm",
        .operands = "[PACKAGES]",
        .targets = .optional,
        .group = .inspect,
        .summary = "List packages absent from the repositories",
        .about = "Lists installed packages that no configured repository provides: usually AUR packages and locally built ones.",
        .examples = &.{"zap foreign"},
    },
    .{
        .command = .files,
        .name = "files",
        .alias = "-Ql",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .inspect,
        .summary = "List the files a package installed",
        .examples = &.{ "zap files ripgrep", "zap files ripgrep --quiet" },
    },
    .{
        .command = .owns,
        .name = "owns",
        .alias = "-Qo",
        .operands = "FILES",
        .targets = .required,
        .group = .inspect,
        .summary = "Find which package owns a file",
        .about = "Directory aliases such as /bin are resolved before matching. Exits with a failure status when any file has no owner.",
        .examples = &.{"zap owns /usr/bin/rg"},
    },
    .{
        .command = .check,
        .name = "check",
        .alias = "-Qk",
        .operands = "[PACKAGES]",
        .targets = .optional,
        .group = .inspect,
        .summary = "Check for missing installed files",
        .about = "Checks that every file a package installed still exists. This detects missing files only; contents and checksums are not verified. Exits with a failure status when any file is missing.",
        .examples = &.{ "zap check", "zap check pacman" },
    },
    .{
        .command = .stats,
        .name = "stats",
        .alias = "-Ps",
        .group = .inspect,
        .summary = "Show package counts, disk usage and the largest packages",
    },
    .{
        .command = .news,
        .name = "news",
        .alias = "-Pw",
        .group = .inspect,
        .summary = "Read Arch Linux news before upgrading",
        .about = "Shows the latest Arch Linux news items. Some upgrades need manual intervention, announced there first.",
    },
    .{
        .command = .build,
        .name = "build",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .build,
        .summary = "Review and build AUR packages without installing",
        .about = "Builds without changing the system, so every dependency must already be installed. The built archives stay in the build cache.",
        .examples = &.{"zap build some-aur-package"},
    },
    .{
        .command = .build_local,
        .name = "build-local",
        .alias = "-B",
        .operands = "DIRS",
        .targets = .required,
        .group = .build,
        .summary = "Build local PKGBUILD directories",
        .about = "Copies each directory into a fresh snapshot in the build cache, then reviews and builds the snapshot; your checkout is never sourced or modified. Each directory needs a PKGBUILD and a matching .SRCINFO. Git metadata, top-level src and pkg trees and package archives are not copied; links and special files are refused.",
        .examples = &.{"zap build-local ."},
    },
    .{
        .command = .install_local,
        .name = "install-local",
        .alias = "-Bi",
        .operands = "DIRS",
        .targets = .required,
        .group = .build,
        .summary = "Build and install local PKGBUILD directories",
        .about = "Like build-local, then resolves dependencies and installs the result. A supplied directory takes precedence over an installed, repository or AUR package of the same name.",
        .examples = &.{ "zap install-local .", "zap install-local ./libfoo ./app" },
    },
    .{
        .command = .install_files,
        .name = "install-files",
        .alias = "-U",
        .operands = "FILES",
        .targets = .required,
        .group = .build,
        .summary = "Install local package archives",
        .about = "Reads each archive's metadata and SHA-256 digest without executing anything. The root worker then copies the archives to private staging, verifies the digests you approved, applies the local signature policy and installs the copies.",
        .examples = &.{"zap install-files ./some-package-1.0-1-x86_64.pkg.tar.zst"},
    },
    .{
        .command = .get,
        .name = "get",
        .alias = "-G",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .build,
        .summary = "Fetch AUR sources without executing them",
        .examples = &.{"zap get some-aur-package"},
    },
    .{
        .command = .pkgbuild,
        .name = "pkgbuild",
        .alias = "-Gp",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .build,
        .summary = "Print AUR PKGBUILDs without executing them",
        .examples = &.{"zap pkgbuild some-aur-package"},
    },
    .{
        .command = .orphans,
        .name = "orphans",
        .alias = "-Qdt",
        .group = .maintain,
        .summary = "List dependencies nothing needs any more",
        .about = "Lists packages installed as dependencies that no installed package requires or optionally uses.",
        .examples = &.{ "zap orphans", "zap orphans --quiet" },
    },
    .{
        .command = .autoremove,
        .name = "autoremove",
        .group = .maintain,
        .summary = "Review and remove orphan dependencies",
        .examples = &.{ "zap autoremove --dry-run", "zap autoremove" },
    },
    .{
        .command = .clean,
        .name = "clean",
        .alias = "-Sc",
        .operands = "[BASES]",
        .targets = .optional,
        .group = .maintain,
        .summary = "Clear cached sources and build outputs",
        .about = "Clears the whole build cache, or only the named package bases, after confirmation. The pacman package cache is not touched.",
        .examples = &.{ "zap clean", "zap clean some-package-base" },
    },
    .{
        .command = .reason,
        .name = "reason",
        .alias = "-D",
        .operands = "PACKAGES",
        .targets = .required,
        .group = .maintain,
        .summary = "Mark packages as explicit or as dependencies",
        .about = "Changes why a package counts as installed. Dependencies nothing requires are listed by zap orphans. Needs --asdeps or --asexplicit.",
        .examples = &.{ "zap reason some-library --asdeps", "zap reason some-tool --asexplicit" },
    },
    .{
        .command = .help,
        .name = "help",
        .alias = "-h",
        .operands = "[COMMAND]",
        .targets = .topic,
        .group = .about,
        .summary = "Show this overview, or one command in detail",
        .examples = &.{ "zap help install", "zap install --help" },
    },
    .{
        .command = .version,
        .name = "version",
        .alias = "--version",
        .group = .about,
        .summary = "Show the zap, libalpm and Zig versions",
    },
};

pub fn spec(command: Command) *const Spec {
    for (&specs) |*entry| if (entry.command == command) return entry;
    unreachable;
}

const Alias = struct { text: []const u8, command: Command, scope: Scope = .all, quiet: bool = false, purge: bool = false };

// Pacman-style spellings, including the combined forms people type from habit.
const aliases = [_]Alias{
    .{ .text = "--help", .command = .help },
    .{ .text = "-V", .command = .version },
    .{ .text = "-Sua", .command = .upgrade, .scope = .aur },
    .{ .text = "-Qua", .command = .updates, .scope = .aur },
    .{ .text = "-Qq", .command = .list, .quiet = true },
    .{ .text = "-Qmq", .command = .foreign, .quiet = true },
    .{ .text = "-Qdtq", .command = .orphans, .quiet = true },
    .{ .text = "-Rns", .command = .remove, .purge = true },
};

const Resolved = struct { command: Command, alias: ?Alias = null };

fn resolve(text: []const u8) ?Resolved {
    for (specs) |entry| {
        if (std.mem.eql(u8, text, entry.name) or std.mem.eql(u8, text, @tagName(entry.command))) return .{ .command = entry.command };
        if (entry.alias.len > 0 and std.mem.eql(u8, text, entry.alias)) return .{ .command = entry.command };
    }
    for (aliases) |alias| if (std.mem.eql(u8, text, alias.text)) return .{ .command = alias.command, .alias = alias };
    return null;
}

// ── Options ─────────────────────────────────────────────────────────────────

pub const CommandSet = std.EnumSet(Command);

fn commands(list: []const Command) CommandSet {
    return CommandSet.initMany(list);
}

const every = CommandSet.initFull();
const builds = commands(&.{ .build, .install, .select, .upgrade, .build_local, .install_local });
const scoped = commands(&.{ .search, .select, .info, .install, .upgrade, .updates });
const searches = commands(&.{ .search, .select });
pub const elevating = commands(&.{ .install, .select, .upgrade, .install_local, .install_files, .remove, .reason, .autoremove });

pub const Flag = enum { dry_run, needed, asdeps, asexplicit, aur, repo, devel, recursive, nosave, sandbox, no_sandbox, rebuildtree, cleanafter, searchby, sortby, quiet, elevate, color, help };

pub const OptionSpec = struct {
    flag: Flag,
    long: []const u8,
    short: []const u8 = "",
    value: []const u8 = "",
    summary: []const u8,
    commands: CommandSet,
};

pub const options = [_]OptionSpec{
    .{ .flag = .dry_run, .long = "--dry-run", .summary = "Show the plan without building, elevating or changing the system", .commands = builds.unionWith(commands(&.{ .install_files, .remove, .autoremove, .clean, .reason })) },
    .{ .flag = .needed, .long = "--needed", .summary = "Skip targets that are already installed and current", .commands = commands(&.{ .install, .install_files, .select }) },
    .{ .flag = .asdeps, .long = "--asdeps", .summary = "Mark the requested packages as dependencies", .commands = commands(&.{ .install, .install_files, .install_local, .select, .reason }) },
    .{ .flag = .asexplicit, .long = "--asexplicit", .summary = "Mark the requested packages as explicitly installed", .commands = commands(&.{ .install, .install_files, .install_local, .select, .reason }) },
    .{ .flag = .aur, .long = "--aur", .short = "-a", .summary = "Only consider the AUR for the requested packages", .commands = scoped },
    .{ .flag = .repo, .long = "--repo", .summary = "Only consider repositories for the requested packages", .commands = scoped },
    .{ .flag = .devel, .long = "--devel", .summary = "Also rebuild installed VCS packages such as -git", .commands = commands(&.{ .updates, .upgrade }) },
    .{ .flag = .recursive, .long = "--recursive", .summary = "Also remove dependencies nothing else needs", .commands = commands(&.{.remove}) },
    .{ .flag = .nosave, .long = "--nosave", .summary = "Delete configuration files instead of keeping .pacsave copies", .commands = commands(&.{.remove}) },
    .{ .flag = .sandbox, .long = "--sandbox", .summary = "Isolate every build phase with bubblewrap (default)", .commands = builds },
    .{ .flag = .no_sandbox, .long = "--no-sandbox", .summary = "Run reviewed build code with your user's permissions", .commands = builds },
    .{ .flag = .rebuildtree, .long = "--rebuildtree", .summary = "Also rebuild installed AUR dependencies", .commands = builds },
    .{ .flag = .cleanafter, .long = "--cleanafter", .summary = "Delete untracked build files after installing", .commands = commands(&.{ .install, .select, .upgrade, .install_local }) },
    .{ .flag = .searchby, .long = "--searchby", .value = "FIELD", .summary = "AUR field to match: name, name-desc (default), maintainer, provides, depends, makedepends, checkdepends, optdepends", .commands = searches },
    .{ .flag = .sortby, .long = "--sortby", .value = "ORDER", .summary = "AUR result order: votes (default), popularity, name, modified", .commands = searches },
    .{ .flag = .quiet, .long = "--quiet", .short = "-q", .summary = "Print bare names (paths for files), one per line", .commands = commands(&.{ .search, .local_search, .updates, .list, .foreign, .orphans, .files, .owns }) },
    .{ .flag = .elevate, .long = "--elevate", .value = "TOOL", .summary = "Gain root with run0 or sudo; default $ZAP_ELEVATE, else run0 with systemd and polkit, else sudo", .commands = elevating },
    .{ .flag = .color, .long = "--color", .value = "WHEN", .summary = "Style output: auto (default), always or never", .commands = every },
    .{ .flag = .help, .long = "--help", .short = "-h", .summary = "Show help for the command", .commands = every },
};

fn option(text: []const u8) ?*const OptionSpec {
    for (&options) |*entry| {
        if (std.mem.eql(u8, text, entry.long) or (entry.short.len > 0 and std.mem.eql(u8, text, entry.short))) return entry;
    }
    return null;
}

// ── Parsing ─────────────────────────────────────────────────────────────────

/// The --color choice, found before parsing so that a failed parse and an
/// early --help honour it too. Invalid values are left for `parse` to report.
pub fn colorPreference(args: []const []const u8) ?ui.ColorMode {
    var choice: ?ui.ColorMode = null;
    var i: usize = 0;
    while (i < args.len) : (i += 1) {
        const arg = args[i];
        if (std.mem.eql(u8, arg, "--")) break;
        if (std.mem.startsWith(u8, arg, "--color=")) {
            choice = std.meta.stringToEnum(ui.ColorMode, arg["--color=".len..]) orelse choice;
        } else if (std.mem.eql(u8, arg, "--color") and i + 1 < args.len) {
            choice = std.meta.stringToEnum(ui.ColorMode, args[i + 1]) orelse choice;
            i += 1;
        }
    }
    return choice;
}

pub fn parse(args: []const []const u8) !Options {
    if (args.len == 0) return .{ .command = .help };
    const resolved = resolve(args[0]) orelse return unknownCommand(args[0]);
    // Asking for help never fails on what else was typed.
    if (resolved.command != .help) for (args[1..]) |arg| {
        if (std.mem.eql(u8, arg, "--")) break;
        if (std.mem.eql(u8, arg, "--help") or std.mem.eql(u8, arg, "-h")) return .{ .command = .help, .topic = resolved.command };
    };
    var o: Options = .{ .command = resolved.command };
    if (resolved.alias) |alias| {
        o.scope = alias.scope;
        o.quiet = alias.quiet;
        o.recursive = alias.purge;
        o.nosave = alias.purge;
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
        // `zap help -S` names a command by its alias; it is not an option.
        const topic = o.command == .help and option(arg) == null and resolve(arg) != null;
        if (positional or topic or !std.mem.startsWith(u8, arg, "-")) {
            try names.append(u.a, arg);
            continue;
        }
        const equals = if (std.mem.startsWith(u8, arg, "--")) std.mem.indexOfScalar(u8, arg, '=') else null;
        const key = arg[0 .. equals orelse arg.len];
        const entry = option(key) orelse return unknownOption(key);
        if (entry.flag == .help) return .{ .command = .help, .topic = if (o.command == .help) null else o.command, .color = o.color };
        if (!entry.commands.contains(o.command)) {
            return errors.with(error.OptionNotValidForCommand, "{s} with {s}", .{ entry.long, spec(o.command).name });
        }
        var value: []const u8 = "";
        if (entry.value.len > 0) {
            if (equals) |at| value = arg[at + 1 ..] else {
                i += 1;
                if (i == args.len) return errors.with(error.OptionValueRequired, "{s} {s}", .{ entry.long, entry.value });
                value = args[i];
            }
        } else if (equals != null) return errors.with(error.OptionValueNotAllowed, "{s}", .{entry.long});
        try apply(&o, entry.flag, value);
    }
    o.names = try names.toOwnedSlice(u.a);
    try validate(&o);
    return o;
}

fn apply(o: *Options, flag: Flag, value: []const u8) !void {
    switch (flag) {
        .dry_run => o.dry = true,
        .needed => o.needed = true,
        .devel => o.devel = true,
        .recursive => o.recursive = true,
        .nosave => o.nosave = true,
        .quiet => o.quiet = true,
        .rebuildtree => o.rebuild_tree = true,
        .cleanafter => o.clean_after = true,
        .sandbox, .no_sandbox => {
            const enabled = flag == .sandbox;
            if (o.sandbox != null and o.sandbox.? != enabled) return errors.with(error.ConflictingOptions, "--sandbox and --no-sandbox", .{});
            o.sandbox = enabled;
        },
        .aur, .repo => {
            const scope: Scope = if (flag == .aur) .aur else .repo;
            if (o.scope != .all and o.scope != scope) return errors.with(error.ConflictingOptions, "--aur and --repo", .{});
            o.scope = scope;
        },
        .asdeps, .asexplicit => {
            const reason: Reason = if (flag == .asdeps) .dependency else .explicit;
            if (o.reason != null and o.reason.? != reason) return errors.with(error.ConflictingOptions, "--asdeps and --asexplicit", .{});
            o.reason = reason;
        },
        .searchby => o.search_by = std.meta.stringToEnum(SearchBy, value) orelse return errors.with(error.InvalidSearchField, "{s}", .{value}),
        .sortby => o.sort_by = std.meta.stringToEnum(SortBy, value) orelse return errors.with(error.InvalidSortField, "{s}", .{value}),
        .color => o.color = std.meta.stringToEnum(ui.ColorMode, value) orelse return errors.with(error.InvalidColorMode, "{s}", .{value}),
        .elevate => o.elevate = std.meta.stringToEnum(Elevator, value) orelse return errors.with(error.InvalidElevator, "{s}", .{value}),
        .help => unreachable,
    }
}

/// Rules that involve more than one option, and the targets a command takes.
fn validate(o: *Options) !void {
    if (o.command == .reason and o.reason == null) return error.InstallationReasonRequired;
    if (o.needed and o.rebuild_tree) return errors.with(error.ConflictingOptions, "--needed and --rebuildtree", .{});
    const entry = spec(o.command);
    switch (entry.targets) {
        .none => if (o.names.len != 0) return errors.with(error.UnexpectedTargets, "{s}", .{entry.name}),
        .term => if (o.names.len == 0) return error.SearchTermRequired,
        .required => if (o.names.len == 0) {
            return errors.withHint(error.PackageNameRequired, usageHint(entry), "zap {s}", .{entry.name});
        },
        .optional => {},
        .topic => {
            if (o.names.len > 1) return errors.with(error.UnexpectedTargets, "help takes one command", .{});
            if (o.names.len == 1) o.topic = (resolve(o.names[0]) orelse return unknownCommand(o.names[0])).command;
        },
    }
}

fn usageHint(entry: *const Spec) []const u8 {
    return std.fmt.allocPrint(u.a, "Usage: zap {s} {s}", .{ entry.name, entry.operands }) catch "";
}

fn unknownCommand(text: []const u8) error{UnknownCommand} {
    if (std.mem.startsWith(u8, text, "-")) {
        return errors.withHint(error.UnknownCommand, "Options go after the command: zap COMMAND [OPTIONS].", "{s}", .{text});
    }
    var names: [specs.len][]const u8 = undefined;
    for (specs, &names) |entry, *name| name.* = entry.name;
    if (nearest(text, &names)) |match| {
        const hint = std.fmt.allocPrint(u.a, "Did you mean zap {s}?", .{match}) catch "";
        return errors.withHint(error.UnknownCommand, hint, "{s}", .{text});
    }
    return errors.with(error.UnknownCommand, "{s}", .{text});
}

fn unknownOption(text: []const u8) error{UnknownOption} {
    var names: [options.len][]const u8 = undefined;
    for (options, &names) |entry, *name| name.* = entry.long;
    if (nearest(text, &names)) |match| {
        const hint = std.fmt.allocPrint(u.a, "Did you mean {s}?", .{match}) catch "";
        return errors.withHint(error.UnknownOption, hint, "{s}", .{text});
    }
    return errors.with(error.UnknownOption, "{s}", .{text});
}

/// The candidate a typo most plausibly meant: a unique prefix, or the single
/// closest spelling within a small edit distance.
pub fn nearest(input: []const u8, candidates: []const []const u8) ?[]const u8 {
    if (input.len < 2 or input.len > 32) return null;
    var prefixed: ?[]const u8 = null;
    var prefix_count: usize = 0;
    for (candidates) |candidate| if (std.mem.startsWith(u8, candidate, input)) {
        prefixed = candidate;
        prefix_count += 1;
    };
    if (prefix_count == 1) return prefixed;
    const limit: usize = if (input.len <= 4) 1 else 2;
    var best: ?[]const u8 = null;
    var best_distance: usize = limit + 1;
    var tied = false;
    for (candidates) |candidate| {
        const d = distance(input, candidate);
        if (d < best_distance) {
            best = candidate;
            best_distance = d;
            tied = false;
        } else if (d == best_distance) tied = true;
    }
    return if (tied) null else best;
}

/// Levenshtein distance, for the short strings of a command line.
fn distance(a: []const u8, b: []const u8) usize {
    var previous: [65]usize = undefined;
    var current: [65]usize = undefined;
    if (b.len >= previous.len) return std.math.maxInt(usize);
    for (0..b.len + 1) |j| previous[j] = j;
    for (a, 1..) |ca, i| {
        current[0] = i;
        for (b, 1..) |cb, j| {
            const substitution = previous[j - 1] + @intFromBool(ca != cb);
            current[j] = @min(substitution, @min(previous[j], current[j - 1]) + 1);
        }
        @memcpy(previous[0 .. b.len + 1], current[0 .. b.len + 1]);
    }
    return previous[b.len];
}

// ── Tests ───────────────────────────────────────────────────────────────────

test "CLI scopes, aliases and contradictory or ignored flags" {
    const scoped_upgrade = try parse(&.{ "-Sua", "--devel", "--dry-run" });
    try std.testing.expectEqual(Scope.aur, scoped_upgrade.scope);
    try std.testing.expect(scoped_upgrade.dry and scoped_upgrade.devel);
    try std.testing.expect((try parse(&.{"-Qmq"})).quiet);
    try std.testing.expectError(error.ConflictingOptions, parse(&.{ "install", "foo", "--asdeps", "--asexplicit" }));
    try std.testing.expectError(error.ConflictingOptions, parse(&.{ "-Sua", "--repo" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "get", "foo", "--dry-run" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "upgrade", "--asdeps" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "search", "foo", "--sandbox" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "info", "foo", "--sortby", "votes" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "build", "foo", "--cleanafter" }));
    try std.testing.expectError(error.OptionNotValidForCommand, parse(&.{ "install-local", ".", "--needed" }));
    try std.testing.expectError(error.ConflictingOptions, parse(&.{ "install", "foo", "--needed", "--rebuildtree" }));
    try std.testing.expectEqual(Command.install_local, (try parse(&.{ "-Bi", ".", "--sandbox" })).command);
    try std.testing.expectError(error.OptionValueRequired, parse(&.{ "search", "foo", "--sortby" }));
    try std.testing.expectError(error.UnknownOption, parse(&.{ "install", "foo", "--noconfirm" }));
    try std.testing.expectError(error.InvalidSearchField, parse(&.{ "search", "foo", "--searchby", "name&arg=evil" }));
    try std.testing.expectEqualStrings("-archive.pkg.tar", (try parse(&.{ "-U", "--", "-archive.pkg.tar" })).names[0]);
}

test "every command resolves by name and alias, and takes the targets it documents" {
    for (specs) |entry| {
        try std.testing.expectEqual(entry.command, resolve(entry.name).?.command);
        try std.testing.expectEqual(entry.command, resolve(@tagName(entry.command)).?.command);
        if (entry.alias.len > 0) try std.testing.expectEqual(entry.command, resolve(entry.alias).?.command);
        try std.testing.expect(entry.summary.len > 0 and entry.summary.len <= 60);
        // Operands shown in help agree with what the parser enforces.
        try std.testing.expectEqual(entry.targets == .none, entry.operands.len == 0);
        try std.testing.expectEqual(entry.targets == .optional or entry.targets == .topic, std.mem.startsWith(u8, entry.operands, "["));
    }
    // Every command is documented exactly once.
    inline for (std.meta.fields(Command)) |field| {
        var seen: usize = 0;
        for (specs) |entry| if (entry.command == @as(Command, @enumFromInt(field.value))) {
            seen += 1;
        };
        try std.testing.expectEqual(@as(usize, 1), seen);
    }
    const purge = try parse(&.{ "-Rns", "foo" });
    try std.testing.expect(purge.recursive and purge.nosave);
    try std.testing.expectError(error.UnexpectedTargets, parse(&.{ "upgrade", "foo" }));
    try std.testing.expectError(error.SearchTermRequired, parse(&.{"search"}));
    try std.testing.expectError(error.PackageNameRequired, parse(&.{"install"}));
    try std.testing.expectError(error.InstallationReasonRequired, parse(&.{ "reason", "foo" }));
    try std.testing.expectEqual(@as(usize, 0), (try parse(&.{"list"})).names.len);
}

test "option values, color modes and help requests" {
    const sorted = try parse(&.{ "search", "foo", "--sortby=popularity", "--searchby", "maintainer", "--color=never" });
    try std.testing.expectEqual(SortBy.popularity, sorted.sort_by);
    try std.testing.expectEqual(SearchBy.maintainer, sorted.search_by);
    try std.testing.expectEqual(ui.ColorMode.never, sorted.color);
    try std.testing.expectError(error.InvalidColorMode, parse(&.{ "list", "--color", "sometimes" }));
    try std.testing.expectError(error.InvalidSortField, parse(&.{ "search", "foo", "--sortby=" }));
    try std.testing.expectError(error.OptionValueNotAllowed, parse(&.{ "install", "foo", "--dry-run=yes" }));
    // A target containing '=' is not mistaken for an option value.
    try std.testing.expectEqualStrings("a=b", (try parse(&.{ "owns", "a=b" })).names[0]);

    try std.testing.expectEqual(@as(?Command, null), (try parse(&.{})).topic);
    try std.testing.expectEqual(@as(?Command, null), (try parse(&.{"--help"})).topic);
    try std.testing.expectEqual(@as(?Command, .install), (try parse(&.{ "help", "install" })).topic);
    try std.testing.expectEqual(@as(?Command, .install), (try parse(&.{ "help", "-S" })).topic);
    // Asking for help never fails on what else was typed.
    const asked = try parse(&.{ "remove", "--help" });
    try std.testing.expectEqual(Command.help, asked.command);
    try std.testing.expectEqual(@as(?Command, .remove), asked.topic);
    try std.testing.expectEqual(@as(?Command, .install_files), (try parse(&.{ "-U", "-h" })).topic);
    try std.testing.expectEqual(@as(?Command, .install), (try parse(&.{ "install", "--bogus", "--sortby", "--help" })).topic);
    try std.testing.expectEqualStrings("--help", (try parse(&.{ "owns", "--", "--help" })).names[0]);
    try std.testing.expectError(error.UnknownCommand, parse(&.{ "help", "nonsense" }));
    try std.testing.expectError(error.UnexpectedTargets, parse(&.{ "help", "install", "remove" }));
}

test "a color preference is honoured even when the rest of the line fails or asks for help" {
    try std.testing.expectEqual(@as(?ui.ColorMode, .never), colorPreference(&.{ "list", "--color=never", "--bogus" }));
    try std.testing.expectEqual(@as(?ui.ColorMode, .always), colorPreference(&.{ "install", "--help", "--color", "always" }));
    try std.testing.expectEqual(@as(?ui.ColorMode, null), colorPreference(&.{ "owns", "--", "--color=never" }));
    try std.testing.expectEqual(@as(?ui.ColorMode, null), colorPreference(&.{ "list", "--color=sometimes" }));
}

test "typos suggest the nearest command or option, and only when unambiguous" {
    var names: [specs.len][]const u8 = undefined;
    for (specs, &names) |entry, *name| name.* = entry.name;
    try std.testing.expectEqualStrings("install", nearest("instal", &names).?);
    try std.testing.expectEqualStrings("search", nearest("serach", &names).?);
    try std.testing.expectEqualStrings("autoremove", nearest("auto", &names).?);
    try std.testing.expectEqualStrings("upgrade", nearest("upgrad", &names).?);
    try std.testing.expect(nearest("up", &names) == null);
    try std.testing.expect(nearest("xyzzy", &names) == null);
    try std.testing.expect(nearest("i", &names) == null);
    try std.testing.expectEqual(@as(usize, 1), distance("--sorby", "--sortby"));
    try std.testing.expectEqual(@as(usize, 3), distance("kitten", "sitting"));
    try std.testing.expectError(error.UnknownCommand, parse(&.{"instal"}));
}

test "each option is accepted exactly where its table says" {
    for (options) |entry| {
        if (entry.flag == .help) continue;
        for (specs) |command| {
            if (command.command == .help) continue;
            var args: std.ArrayList([]const u8) = .empty;
            try args.append(u.a, command.name);
            if (command.targets == .term or command.targets == .required) try args.append(u.a, "target");
            // reason needs a reason regardless of the option under test.
            if (command.command == .reason and entry.flag != .asdeps and entry.flag != .asexplicit) try args.append(u.a, "--asdeps");
            try args.append(u.a, entry.long);
            if (entry.value.len > 0) try args.append(u.a, switch (entry.flag) {
                .searchby => "name",
                .sortby => "name",
                .elevate => "sudo",
                else => "never",
            });
            const result = parse(args.items);
            if (entry.commands.contains(command.command)) {
                _ = try result;
            } else try std.testing.expectError(error.OptionNotValidForCommand, result);
        }
    }
}
