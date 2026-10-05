const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const tx = @import("transaction.zig");
const builder = @import("builder.zig");
const cli = @import("cli.zig");
const query = @import("query.zig");

pub fn main() void {
    var arena = std.heap.ArenaAllocator.init(std.heap.page_allocator);
    defer arena.deinit();
    u.a = arena.allocator();
    _ = c.setlocale(c.LC_CTYPE, "");
    dispatch() catch |err| {
        if (err == error.Cancelled) {
            ui.note(.muted, "Operation cancelled.");
            std.process.exit(130);
        }
        ui.title("Unable to complete operation");
        ui.note(.danger, @errorName(err));
        switch (err) {
            error.UnknownCommand, error.UnknownOption => ui.text("Use zap help to see available commands and options.", 2, .muted),
            error.SearchTermRequired => ui.text("Add a search term, for example: zap search ripgrep", 2, .muted),
            error.PackageNameRequired => ui.text("Add a package name or target. Use zap help for examples.", 2, .muted),
            error.InvalidSelection => ui.text("Choose numbers from the results, for example: 1 3-5", 2, .muted),
            error.InteractiveTerminalRequired => ui.text("Run system changes from an interactive terminal; every transaction requires confirmation.", 2, .muted),
            error.BuildsMustNotRunAsRoot => ui.text("Run zap as your regular user. Only the internal transaction worker runs as root through run0.", 2, .muted),
            error.AlpmOperationFailed => ui.text("The transaction was not completed. Inspect the libalpm diagnostic above; never remove an active database lock.", 2, .muted),
            error.UnsupportedPacmanConfiguration => ui.text("XferCommand and AssumeInstalled are unsupported; zap refuses to silently ignore them.", 2, .muted),
            error.MissingBuildArtifacts => ui.text("Check the required package names and output paths above, then rebuild.", 2, .muted),
            error.DuplicateBuildArtifact => ui.text("Check the conflicting archive paths and the PKGBUILD's split-package declarations before rebuilding.", 2, .muted),
            error.ArchiveBaseMismatch, error.InvalidBuildArchive => ui.text("Check the archive metadata and build outputs against the reviewed PKGBUILD before retrying.", 2, .muted),
            error.UnsafeArchive => ui.text("Build archives must be readable regular files without symlinks.", 2, .muted),
            error.BuildSandboxUnavailable => ui.text("Build isolation failed. Install bubblewrap and check user namespace support. An explicitly requested --no-sandbox build runs approved code with your user's permissions.", 2, .muted),
            else => {},
        }
        std.process.exit(1);
    };
}
fn dispatch() !void {
    const args = try std.process.argsAlloc(u.a);
    if (args.len >= 2 and std.mem.eql(u8, args[1], "__transaction")) {
        if (args.len != 3) return error.InvalidWorkerArguments;
        return tx.worker(args[2]);
    }
    const o = try cli.parse(args[1..]);
    if (o.command == .help) return help();
    if (o.dry) ui.note(.warning, "Dry run · no builds or system changes will be performed.");
    if (o.command == .version) {
        ui.print("zap 0.1.0 · Zig 0.15.2 · libalpm {s}\n", .{u.str(c.alpm_version())});
        return;
    }
    if (c.geteuid() == 0) return error.BuildsMustNotRunAsRoot;
    if (o.command == .news) return @import("news.zig").show();
    switch (o.command) {
        .install, .build, .get, .pkgbuild, .info, .localinfo, .files, .reason, .remove => try requireNames(o.names),
        .list, .foreign, .check, .clean => if (o.names.len > 0) try requireNames(o.names),
        else => {},
    }
    var db = try alpm.Alpm.init();
    defer db.deinit();
    switch (o.command) {
        .search => {
            _ = try query.search(&db, o);
        },
        .select => {
            const matches = try query.search(&db, o);
            if (matches.len == 0) return error.PackageNotFound;
            ui.title("Choose packages to install");
            ui.text("Use the numbers beside the search results. Separate choices with spaces or commas; use a dash for ranges.", 2, .muted);
            const selected = try query.selection(try ui.answer("Numbers or ranges (e.g. 1 3-5); empty cancels: "), matches.len);
            var names: std.ArrayList([]const u8) = .empty;
            var aur_names: std.ArrayList([]const u8) = .empty;
            for (selected) |index| {
                for (selected) |other| if (other != index and std.mem.eql(u8, matches[index].name, matches[other].name) and matches[index].source != matches[other].source) return error.ConflictingSelection;
                try @import("srcinfo.zig").appendUnique(&names, matches[index].name);
                if (matches[index].source == .aur) try @import("srcinfo.zig").appendUnique(&aur_names, matches[index].name);
            }
            // Preserve the selected source when repo and AUR use the same name.
            var builds = try builder.Builder.init(&db, names.items);
            defer builds.deinit();
            configure(&builds, o);
            builds.aur_targets = aur_names.items;
            try builds.plan();
            if (!o.dry) try builds.execute(true);
        },
        .info, .localinfo => for (o.names) |name| {
            if (o.command == .localinfo) {
                try query.details(try query.exactLocal(&db, name), true);
                continue;
            }
            const repo = if (o.scope == .aur) null else try db.repo(name);
            if (repo) |p| try query.details(p, false) else {
                if (o.scope == .repo) return error.RepositoryPackageNotFound;
                const result = try aur.info(&.{name});
                if (result.len == 0) return error.PackageNotFound;
                const p = result[0];
                p.show();
                ui.print("\n", .{});
                ui.field("Package base", p.PackageBase);
                ui.field("URL", p.URL orelse "");
                ui.field("AUR", try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/packages/{s}", .{p.Name}));
                ui.field("First submitted", ui.date(p.FirstSubmitted));
                for ([_][]const []const u8{ p.Depends, p.MakeDepends, p.CheckDepends, p.OptDepends, p.Provides, p.Conflicts, p.License }, [_][]const u8{ "Dependencies", "Build dependencies", "Check dependencies", "Optional dependencies", "Provides", "Conflicts", "Licenses" }) |list, label| {
                    ui.field(label, try std.mem.join(u.a, "  ", list));
                }
                ui.print("\n", .{});
            }
        },
        .list, .foreign => {
            for (o.names) |name| {
                _ = try query.exactLocal(&db, name);
            }
            var it = db.installed();
            while (it != null) : (it = it.*.next) {
                const p = alpm.pkg(it.*.data);
                if (o.names.len > 0 and !@import("srcinfo.zig").contains(o.names, alpm.name(p))) continue;
                if (o.command == .foreign and !try db.foreign(p)) continue;
                if (o.quiet) ui.print("{s}\n", .{ui.safe(alpm.name(p))}) else {
                    ui.installed(alpm.name(p), alpm.version(p), c.alpm_pkg_get_installdate(p), if (c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_EXPLICIT) "explicit" else "dependency");
                }
            }
        },
        .local_search => {
            const packages = try query.repositorySearch(&db, try std.mem.join(u.a, " ", o.names), true);
            for (packages) |p| {
                if (o.quiet) ui.print("{s}\n", .{ui.safe(alpm.name(p))}) else {
                    ui.package(alpm.name(p), alpm.version(p), "installed", "Installed", c.alpm_pkg_get_installdate(p), u.str(c.alpm_pkg_get_desc(p)), null);
                    ui.print("\n", .{});
                }
            }
        },
        .files => try query.files(&db, o.names, o.quiet),
        .owns => try query.owners(&db, o.names, o.quiet),
        .check => try query.check(&db, o.names),
        .stats => try query.stats(&db),
        .updates => {
            _ = try updates(&db, o);
        },
        .upgrade => {
            if (o.scope != .aur and !o.dry) {
                try tx.escalate(.{ .operation = .upgrade });
                const refreshed = try alpm.Alpm.init();
                db.deinit();
                db = refreshed;
            } else if (o.dry and o.scope != .aur) ui.print("Dry run uses the currently cached repository databases.\n", .{});
            const targets = try updates(&db, o);
            if (targets.len > 0) {
                var builds = try builder.Builder.init(&db, targets);
                defer builds.deinit();
                configure(&builds, o);
                builds.scope = .aur;
                builds.preserve_reasons = true;
                try builds.plan();
                if (!o.dry) try builds.execute(true);
            }
        },
        .install, .build, .get, .pkgbuild => {
            var builds = try builder.Builder.init(&db, o.names);
            defer builds.deinit();
            configure(&builds, o);
            if (o.command == .get) try builds.getSources() else if (o.command == .pkgbuild) try builds.printPkgbuilds() else {
                try builds.plan();
                if (!o.dry) try builds.execute(o.command == .install);
            }
        },
        .build_local, .install_local => {
            var builds = try builder.Builder.init(&db, &.{});
            defer builds.deinit();
            configure(&builds, o);
            try builds.loadLocal(o.names);
            try builds.plan();
            if (!o.dry) try builds.execute(o.command == .install_local);
        },
        .install_files => try installFiles(&db, o),
        .remove, .reason => {
            var targets: std.ArrayList(tx.Target) = .empty;
            for (o.names) |name| {
                _ = try query.exactLocal(&db, name);
                try targets.append(u.a, .{ .name = name, .reason = o.reason });
                if (o.command == .remove) ui.print("Remove {s}\n", .{ui.safe(name)}) else ui.print("Mark {s} as {s}\n", .{ ui.safe(name), @tagName(o.reason.?) });
            }
            if (!o.dry) try tx.escalate(.{ .operation = if (o.command == .remove) .remove else .reason, .targets = targets.items, .recursive = o.recursive, .nosave = o.nosave });
        },
        .orphans, .autoremove => {
            const orphans = try db.orphans();
            if (orphans.items.len == 0) {
                if (!o.quiet) ui.print("No orphan dependencies.\n", .{});
                return;
            }
            var targets: std.ArrayList(tx.Target) = .empty;
            for (orphans.items) |name| {
                if (o.command == .orphans) ui.print("{s}\n", .{ui.safe(name)}) else ui.print("Remove orphan {s}\n", .{ui.safe(name)});
                try targets.append(u.a, .{ .name = name });
            }
            if (o.command == .autoremove and !o.dry) try tx.escalate(.{ .operation = .remove, .targets = targets.items, .recursive = true });
        },
        .clean => {
            var builds = try builder.Builder.init(&db, o.names);
            defer builds.deinit();
            if (!o.dry) try builds.clean() else if (o.names.len == 0) ui.print("Would clear {s}\n", .{ui.safe(builds.root)}) else for (o.names) |base| ui.print("Would clear {s}/{s}\n", .{ ui.safe(builds.root), ui.safe(base) });
        },
        .help, .version, .news => unreachable,
    }
}
fn configure(builds: *builder.Builder, o: cli.Options) void {
    builds.scope = o.scope;
    builds.needed = o.needed;
    builds.reason = o.reason;
    builds.sandbox = o.sandbox orelse true;
    builds.clean_after = o.clean_after;
    builds.rebuild_tree = o.rebuild_tree;
}
pub fn archiveTargets(db: *alpm.Alpm, paths: []const []const u8, reason: ?cli.Reason) ![]tx.Target {
    var targets: std.ArrayList(tx.Target) = .empty;
    for (paths) |input| {
        // Resolve lexical components without following a final archive symlink.
        const path = try std.fs.path.resolve(u.a, &.{ try std.process.getCwdAlloc(u.a), input });
        const digest = try tx.hashFile(path);
        var p: ?*c.alpm_pkg_t = null;
        try db.check(c.alpm_pkg_load(db.h, (try u.z(path)).ptr, 1, 0, &p));
        defer _ = c.alpm_pkg_free(p);
        if (!std.mem.eql(u8, digest, try tx.hashFile(path))) return error.ArchiveChanged;
        const name = try u.a.dupe(u8, alpm.name(p.?));
        try targets.append(u.a, .{ .name = name, .archive = path, .sha256 = digest, .reason = reason });
    }
    try tx.validate(.{ .operation = .install, .targets = targets.items });
    return targets.toOwnedSlice(u.a);
}
fn installFiles(db: *alpm.Alpm, o: cli.Options) !void {
    const targets = try archiveTargets(db, o.names, o.reason);
    ui.title("Local archive installation");
    for (targets) |target| {
        ui.text(target.name, 2, .bold);
        ui.field("Archive", target.archive.?);
        ui.field("SHA-256", target.sha256.?);
        ui.print("\n", .{});
    }
    if (!o.dry) try tx.escalate(.{ .operation = .install, .targets = targets, .needed = o.needed });
}
fn requireNames(names: []const []const u8) !void {
    if (names.len == 0) return error.PackageNameRequired;
    for (names, 0..) |name, i| {
        if (!u.validName(name)) return error.InvalidPackageName;
        for (names[0..i]) |previous| if (std.mem.eql(u8, previous, name)) return error.DuplicateTarget;
    }
}
fn updates(db: *alpm.Alpm, o: cli.Options) ![]const []const u8 {
    if (!o.quiet) ui.title("Available updates");
    var foreign: std.ArrayList([]const u8) = .empty;
    var it = db.installed();
    var count: usize = 0;
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (c.alpm_pkg_should_ignore(db.h, p) != 0) continue;
        if (try db.foreign(p)) {
            if (o.scope != .repo) try foreign.append(u.a, alpm.name(p));
        } else if (o.scope != .aur) if (c.alpm_sync_get_new_version(p, c.alpm_get_syncdbs(db.h))) |next| {
            if (o.quiet) ui.print("{s}\n", .{ui.safe(alpm.name(p))}) else {
                ui.change(alpm.name(p), alpm.version(p), alpm.version(next), "repo");
                ui.field("Build date", ui.date(c.alpm_pkg_get_builddate(next)));
                ui.print("\n", .{});
            }
            count += 1;
        };
    }
    var targets: std.ArrayList([]const u8) = .empty;
    const remote = try aur.info(foreign.items);
    for (remote) |p| {
        const old = try db.local(p.Name) orelse continue;
        var vcs = false;
        if (o.devel) for ([_][]const u8{ "-git", "-svn", "-hg", "-bzr", "-cvs", "-darcs", "-fossil" }) |suffix| {
            if (std.mem.endsWith(u8, p.Name, suffix)) vcs = true;
        };
        if (c.alpm_pkg_vercmp((try u.z(p.Version)).ptr, (try u.z(alpm.version(old))).ptr) > 0 or vcs) {
            if (o.quiet) ui.print("{s}\n", .{p.Name}) else {
                ui.change(p.Name, alpm.version(old), p.Version, "AUR");
                ui.field("Last modified", ui.date(p.LastModified));
                if (vcs) ui.note(.warning, "VCS rebuild requested");
                ui.print("\n", .{});
            }
            try targets.append(u.a, p.Name);
            count += 1;
        }
    }
    if (count == 0 and !o.quiet) ui.note(.success, "Everything is up to date in the cached databases.");
    for (foreign.items) |name| {
        var found = false;
        for (remote) |p| if (std.mem.eql(u8, p.Name, name)) {
            found = true;
            break;
        };
        if (!found and !o.quiet) ui.print("Foreign package {s} is absent from AUR; left unchanged.\n", .{ui.safe(name)});
    }
    return targets.toOwnedSlice(u.a);
}
fn help() void {
    ui.print("\n  {s}⚡ zap{s}  {s}0.1.0{s}\n", .{ ui.style(.accent), ui.style(.reset), ui.style(.muted), ui.style(.reset) });
    ui.text("A native Arch Linux + AUR helper", 2, .muted);
    ui.print("\n", .{});
    ui.text("zap <command> [targets] [options]", 2, .accent);

    ui.title("Start here");
    ui.command("search TERM", "-Ss", "Find packages in repositories and the AUR");
    ui.command("select TERM", "", "Choose numbered results, then review and install");
    ui.command("install PACKAGES", "-S", "Resolve dependencies, review sources and install");
    ui.command("upgrade", "-Syu", "Refresh and upgrade repositories, then the AUR");
    ui.command("updates", "-Qu", "Check cached repository and live AUR updates");
    ui.command("remove PACKAGES", "-R", "Remove packages with dependency checks");

    ui.title("Inspect your system");
    ui.command("info PACKAGES", "-Si", "Available metadata, dependencies and dates");
    ui.command("localinfo PACKAGES", "-Qi", "Installed metadata and reverse dependencies");
    ui.command("list [PACKAGES]", "-Q", "Installed versions, dates and installation reasons");
    ui.command("local-search TERM", "-Qs", "Search installed names and descriptions");
    ui.command("foreign", "-Qm", "List packages absent from configured repositories");
    ui.command("files PACKAGES", "-Ql", "List installed package files");
    ui.command("owns FILES", "-Qo", "Find installed file owners");
    ui.command("check [PACKAGES]", "-Qk", "Check for missing installed files");
    ui.command("stats", "-Ps", "Package counts, disk usage and largest packages");
    ui.command("news", "-Pw", "Read Arch intervention notices before upgrading");

    ui.title("Build and source files");
    ui.command("build PACKAGES", "", "Review and build AUR packages without installation");
    ui.command("build-local DIRS", "-B", "Build local PKGBUILDs from fresh review snapshots");
    ui.command("install-local DIRS", "-Bi", "Review, build and install local PKGBUILDs");
    ui.command("install-files FILES", "-U", "Install verified local package archives");
    ui.command("get PACKAGES", "-G", "Fetch AUR sources without executing them");
    ui.command("pkgbuild PACKAGES", "-Gp", "Print AUR PKGBUILDs without executing them");

    ui.title("Maintain");
    ui.command("orphans", "-Qdt", "List unneeded dependencies");
    ui.command("autoremove", "", "Review and remove orphan dependencies");
    ui.command("clean [BASES]", "-Sc", "Clear all or selected cached package bases");
    ui.command("reason PACKAGES", "-D", "Change reasons; requires --asdeps or --asexplicit");
    ui.command("version", "", "Show zap version and libalpm ABI");
    ui.command("help", "-h", "Show this command reference");

    ui.title("Options");
    ui.command("--aur / --repo", "", "Scope search, select, info, install, updates or upgrade");
    ui.command("--dry-run", "", "Plan without builds, escalation or system changes");
    ui.command("--needed", "", "Skip current or newer installed targets with install");
    ui.command("--asdeps / --asexplicit", "", "Set installation reasons for requested packages");
    ui.command("--sandbox", "", "Isolate every makepkg phase (default)");
    ui.command("--no-sandbox", "", "Run reviewed build code with your user's permissions");
    ui.command("--rebuildtree", "", "Rebuild installed foreign dependencies too");
    ui.command("--cleanafter", "", "Remove untracked build outputs after installation");
    ui.command("--quiet", "-q", "Print only names; print paths with files");
    ui.command("--searchby FIELD", "", "AUR: name, name-desc (default), maintainer, provides, depends, makedepends, checkdepends, optdepends");
    ui.command("--sortby FIELD", "", "AUR: votes (default), popularity, name, modified");
    ui.command("--devel", "", "Include VCS rebuilds with updates or upgrade");
    ui.command("--recursive", "", "Remove unneeded dependencies with remove");
    ui.command("--nosave", "", "Discard backup configuration during removal (-Rns)");
    ui.command("--", "", "Treat remaining arguments as targets, including paths");

    ui.title("Try it");
    ui.text("zap select browser --aur", 2, .accent);
    ui.text("zap install ripgrep --needed", 2, .accent);
    ui.text("zap upgrade --devel --dry-run", 2, .accent);
    ui.print("\n", .{});
    ui.text("Build sources run as your user after review. Sandboxed builds have a private home and network access for downloads. System changes use run0/polkit and require a separate final confirmation.", 2, .muted);
    ui.text("Dates are UTC. NO_COLOR disables styling.", 2, .muted);
    ui.print("\n", .{});
}

test {
    _ = @import("util.zig");
    _ = @import("ui.zig");
    _ = @import("config.zig");
    _ = @import("aur.zig");
    _ = @import("srcinfo.zig");
    _ = @import("transaction.zig");
    _ = @import("native_tests.zig");
    _ = @import("builder.zig");
    _ = @import("artifacts.zig");
    _ = @import("cli.zig");
    _ = @import("query.zig");
    _ = @import("news.zig");
    _ = @import("sandbox.zig");
}

test "local archives use metadata identities and reject duplicates and symlinks" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    const archive = try native.fixture(tmp.dir, root, "zap-archive-fixture", "");
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    try tmp.dir.copyFile(std.fs.path.basename(archive), tmp.dir, "arbitrary name.pkg.tar", .{});
    const arbitrary = try std.fs.path.join(u.a, &.{ root, "arbitrary name.pkg.tar" });
    const targets = try archiveTargets(&db, &.{arbitrary}, .dependency);
    try std.testing.expectEqualStrings("zap-archive-fixture", targets[0].name);
    try std.testing.expectEqual(cli.Reason.dependency, targets[0].reason.?);
    try std.testing.expectError(error.DuplicateTarget, archiveTargets(&db, &.{ archive, arbitrary }, null));
    try tmp.dir.symLink(std.fs.path.basename(archive), "alias.pkg.tar", .{});
    try std.testing.expectError(error.UnsafeArchive, archiveTargets(&db, &.{try std.fs.path.join(u.a, &.{ root, "alias.pkg.tar" })}, null));
}
