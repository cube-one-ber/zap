//! Read-only views of the repositories, the AUR and the installed system.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const cli = @import("cli.zig");
const ui = @import("ui.zig");
const errors = @import("errors.zig");

// ── Search ──────────────────────────────────────────────────────────────────

pub const Match = struct { name: []const u8, source: cli.Scope };

/// Search the repositories and the AUR as the scope allows. Results are
/// numbered repositories first; `select` shows them with number 1 last, so
/// the best matches sit next to the prompt.
pub fn search(db: *alpm.Alpm, o: cli.Options) ![]Match {
    const term = try std.mem.join(u.a, " ", o.names);
    const numbered = o.command == .select;
    const repo_packages: []alpm.Pkg = if (o.scope != .aur) try repositorySearch(db, term, false) else &.{};
    const aur_packages: []aur.Package = if (o.scope != .repo) try aur.search(term, o.search_by) else &.{};
    // The name you typed comes first, then names that start with it; the
    // sort is stable, so repositories keep their configured order after that.
    std.mem.sort(alpm.Pkg, repo_packages, term, repoBefore);
    std.mem.sort(aur.Package, aur_packages, AurOrder{ .term = term, .by = o.sort_by }, aurBefore);

    var matches: std.ArrayList(Match) = .empty;
    for (repo_packages) |p| try matches.append(u.a, .{ .name = alpm.name(p), .source = .repo });
    for (aur_packages) |p| try matches.append(u.a, .{ .name = p.Name, .source = .aur });
    if (o.quiet) {
        ui.hold();
        defer ui.release();
        for (matches.items) |match| ui.print("{s}\n", .{ui.safe(match.name)});
        return matches.toOwnedSlice(u.a);
    }

    const sections = [_]cli.Scope{ .repo, .aur };
    for (0..sections.len) |step| {
        const section = sections[if (numbered) sections.len - 1 - step else step];
        if (section == .repo and o.scope != .aur) {
            ui.heading("Repositories", ui.count(repo_packages.len, "match", "matches"));
            for (0..repo_packages.len) |i| {
                const at = if (numbered) repo_packages.len - 1 - i else i;
                ui.entry(try repoEntry(db, repo_packages[at], if (numbered) at + 1 else null));
            }
            if (repo_packages.len == 0) ui.note(.info, "Nothing in the configured repositories matches.");
        }
        if (section == .aur and o.scope != .repo) {
            const detail = try std.fmt.allocPrint(u.a, "{s} · closest names first, then by {s}", .{ ui.count(aur_packages.len, "match", "matches"), @tagName(o.sort_by) });
            ui.heading("AUR", detail);
            for (0..aur_packages.len) |i| {
                const at = if (numbered) aur_packages.len - 1 - i else i;
                ui.entry(try aurEntry(db, aur_packages[at], if (numbered) repo_packages.len + at + 1 else null));
            }
            if (aur_packages.len == 0) ui.note(.info, "Nothing in the AUR matches.");
        }
    }
    ui.blank();
    if (matches.items.len == 0) {
        ui.note(.info, "Try a shorter or broader term.");
    } else if (!numbered) {
        ui.note(.info, "zap info NAME shows details · zap select TERM installs from this list.");
    }
    return matches.toOwnedSlice(u.a);
}

/// 0 for the exact name, 1 for a name that starts with the term, 2 otherwise.
fn closeness(name: []const u8, term: []const u8) u2 {
    if (std.mem.eql(u8, name, term)) return 0;
    if (std.mem.startsWith(u8, name, term)) return 1;
    return 2;
}

fn repoBefore(term: []const u8, a: alpm.Pkg, b: alpm.Pkg) bool {
    return closeness(alpm.name(a), term) < closeness(alpm.name(b), term);
}

const AurOrder = struct { term: []const u8, by: cli.SortBy };

fn aurBefore(order: AurOrder, a: aur.Package, b: aur.Package) bool {
    const near_a = closeness(a.Name, order.term);
    const near_b = closeness(b.Name, order.term);
    if (near_a != near_b) return near_a < near_b;
    switch (order.by) {
        .votes => if (a.NumVotes != b.NumVotes) return a.NumVotes > b.NumVotes,
        .popularity => if (a.Popularity != b.Popularity) return a.Popularity > b.Popularity,
        .modified => if (a.LastModified != b.LastModified) return a.LastModified > b.LastModified,
        .name => {},
    }
    return std.mem.lessThan(u8, a.Name, b.Name);
}

/// Mark results that are installed under exactly this name. A package that
/// merely provides the name is a different package.
fn installedBadge(db: *alpm.Alpm, name: []const u8, available: []const u8) !?ui.Badge {
    const installed_package = try db.installedPkg(name) orelse return null;
    const current = alpm.version(installed_package);
    if (std.mem.eql(u8, current, available)) return .{ .text = "✓ installed", .tone = .success };
    // An older installed version means an update is waiting, not "done".
    if (c.alpm_pkg_vercmp(c.alpm_pkg_get_version(installed_package), (try u.z(available)).ptr) < 0) {
        return .{ .text = try std.fmt.allocPrint(u.a, "installed {s} · update available", .{current}), .tone = .warning };
    }
    return .{ .text = try std.fmt.allocPrint(u.a, "✓ installed {s}", .{current}), .tone = .success };
}

fn repoEntry(db: *alpm.Alpm, p: alpm.Pkg, index: ?usize) !ui.Entry {
    var badges: std.ArrayList(ui.Badge) = .empty;
    if (try installedBadge(db, alpm.name(p), alpm.version(p))) |badge| try badges.append(u.a, badge);
    return .{
        .index = index,
        .source = alpm.repository(p),
        .name = alpm.name(p),
        .link = try repoPage(p),
        .version = alpm.version(p),
        .badges = badges.items,
        .description = alpm.description(p),
        .meta = try std.fmt.allocPrint(u.a, "{s} · built {s}", .{ ui.size(c.alpm_pkg_get_isize(p)), ui.date(c.alpm_pkg_get_builddate(p)) }),
    };
}

/// The archlinux.org page of a package from an official Arch repository;
/// other repositories have no known page.
fn repoPage(p: alpm.Pkg) ![]const u8 {
    const repo = alpm.repository(p);
    for ([_][]const u8{ "core", "extra", "multilib", "core-testing", "extra-testing", "multilib-testing" }) |official| {
        if (std.mem.eql(u8, repo, official)) {
            return std.fmt.allocPrint(u.a, "https://archlinux.org/packages/{s}/{s}/{s}/", .{ repo, u.str(c.alpm_pkg_get_arch(p)), alpm.name(p) });
        }
    }
    return "";
}

fn aurEntry(db: *alpm.Alpm, p: aur.Package, index: ?usize) !ui.Entry {
    var badges: std.ArrayList(ui.Badge) = .empty;
    if (try installedBadge(db, p.Name, p.Version)) |badge| try badges.append(u.a, badge);
    if (p.OutOfDate != null) try badges.append(u.a, .{ .text = "! out of date", .tone = .warning });
    if (p.Maintainer == null) try badges.append(u.a, .{ .text = "! orphaned", .tone = .warning });
    return .{
        .index = index,
        .source = "aur",
        .name = p.Name,
        .link = try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/packages/{s}", .{p.Name}),
        .version = p.Version,
        .badges = badges.items,
        .description = p.Description orelse "",
        .meta = try std.fmt.allocPrint(u.a, "{s} · popularity {d:.2} · {s} · updated {s}", .{
            ui.count(@intCast(@max(p.NumVotes, 0)), "vote", "votes"),
            p.Popularity,
            p.Maintainer orelse "no maintainer",
            ui.date(p.LastModified),
        }),
    };
}

/// Packages whose name or description matches, from the local database or
/// from every repository that allows searching. The first repository to
/// carry a name wins, as it does for installation.
pub fn repositorySearch(db: *alpm.Alpm, term: []const u8, local: bool) ![]alpm.Pkg {
    var needles: alpm.List = null;
    needles = c.alpm_list_add(needles, @constCast((try u.z(term)).ptr));
    defer c.alpm_list_free(needles);
    var packages: std.ArrayList(alpm.Pkg) = .empty;
    var dbs = if (local) c.alpm_list_add(null, db.localDb()) else c.alpm_get_syncdbs(db.h);
    const owned = dbs;
    defer if (local) c.alpm_list_free(owned);
    var seen: std.StringHashMap(void) = .init(u.a);
    defer seen.deinit();
    while (dbs != null) : (dbs = dbs.*.next) {
        const database: *c.alpm_db_t = @ptrCast(@alignCast(dbs.*.data.?));
        if (!local) {
            var usage: c_int = 0;
            try db.check(c.alpm_db_get_usage(database, &usage));
            if (usage & c.ALPM_DB_USAGE_SEARCH == 0) continue;
        }
        var result: alpm.List = null;
        try db.check(c.alpm_db_search(database, needles, &result));
        defer c.alpm_list_free(result);
        var it = result;
        while (it != null) : (it = it.*.next) {
            const p = alpm.pkg(it.*.data);
            if (seen.contains(alpm.name(p))) continue;
            try seen.put(alpm.name(p), {});
            try packages.append(u.a, p);
        }
    }
    return packages.toOwnedSlice(u.a);
}

pub fn localSearch(db: *alpm.Alpm, o: cli.Options) !void {
    const packages = try repositorySearch(db, try std.mem.join(u.a, " ", o.names), true);
    if (o.quiet) {
        ui.hold();
        defer ui.release();
        for (packages) |p| ui.print("{s}\n", .{ui.safe(alpm.name(p))});
        return;
    }
    ui.heading("Installed", ui.count(packages.len, "match", "matches"));
    for (packages) |p| ui.entry(.{
        .source = "local",
        .name = alpm.name(p),
        .version = alpm.version(p),
        .description = alpm.description(p),
        .meta = try std.fmt.allocPrint(u.a, "{s} · {s} · installed {s}", .{ ui.size(c.alpm_pkg_get_isize(p)), alpm.reason(p), ui.date(c.alpm_pkg_get_installdate(p)) }),
    });
    if (packages.len == 0) ui.note(.info, "No installed package matches.");
    ui.blank();
}

/// Parse a reply such as `1 3-5, 7` into unique zero-based indices. Numbers
/// are bounded by `count` and are never used as anything but indices.
pub fn selection(reply: []const u8, count: usize) ![]usize {
    var selected: std.ArrayList(usize) = .empty;
    var words = std.mem.tokenizeAny(u8, reply, " ,\t");
    while (words.next()) |word| {
        const dash = std.mem.indexOfScalar(u8, word, '-');
        const first = std.fmt.parseInt(usize, if (dash) |d| word[0..d] else word, 10) catch return error.InvalidSelection;
        const last = if (dash) |d| std.fmt.parseInt(usize, word[d + 1 ..], 10) catch return error.InvalidSelection else first;
        if (first == 0 or first > last or last > count) return error.InvalidSelection;
        for (first - 1..last) |index| {
            if (std.mem.indexOfScalar(usize, selected.items, index) == null) try selected.append(u.a, index);
        }
    }
    if (selected.items.len == 0) return error.Cancelled;
    return selected.toOwnedSlice(u.a);
}

// ── Details ─────────────────────────────────────────────────────────────────

/// The installed package with exactly this name, or a failure naming it.
pub fn installed(db: *alpm.Alpm, name: []const u8) !alpm.Pkg {
    return try db.installedPkg(name) orelse errors.with(error.PackageNotInstalled, "{s}", .{name});
}

pub fn info(db: *alpm.Alpm, o: cli.Options) !void {
    for (o.names) |name| {
        if (o.command == .localinfo) {
            try packageDetails(db, try installed(db, name), true);
            continue;
        }
        const repo_package = if (o.scope == .aur) null else try db.repo(name);
        if (repo_package) |p| {
            try packageDetails(db, p, false);
        } else if (o.scope == .repo) {
            return errors.with(error.RepositoryPackageNotFound, "{s}", .{name});
        } else {
            const p = try aur.find(name) orelse return errors.with(if (o.scope == .aur) error.AurPackageNotFound else error.PackageNotFound, "{s}", .{name});
            try aurDetails(db, p);
        }
    }
}

fn packageDetails(db: *alpm.Alpm, p: alpm.Pkg, local: bool) !void {
    var badges: std.ArrayList(ui.Badge) = .empty;
    if (!local) if (try installedBadge(db, alpm.name(p), alpm.version(p))) |badge| try badges.append(u.a, badge);
    ui.blank();
    ui.entry(.{
        .source = if (local) "local" else alpm.repository(p),
        .name = alpm.name(p),
        .version = alpm.version(p),
        .badges = badges.items,
        .description = alpm.description(p),
    });
    ui.blank();
    ui.fieldOptional("URL", u.str(c.alpm_pkg_get_url(p)));
    ui.fieldList("Licenses", try alpm.strings(c.alpm_pkg_get_licenses(p)));
    ui.fieldList("Groups", try alpm.strings(c.alpm_pkg_get_groups(p)));
    ui.fieldOptional("Architecture", u.str(c.alpm_pkg_get_arch(p)));
    ui.fieldOptional("Packager", u.str(c.alpm_pkg_get_packager(p)));
    ui.field("Built", ui.dated(c.alpm_pkg_get_builddate(p)));
    if (local) {
        ui.field("Installed", ui.dated(c.alpm_pkg_get_installdate(p)));
        ui.field("Reason", alpm.reason(p));
    } else ui.field("Download size", ui.size(c.alpm_pkg_get_size(p)));
    ui.field("Installed size", ui.size(c.alpm_pkg_get_isize(p)));
    ui.field("Depends on", try std.mem.join(u.a, "  ", try alpm.dependencies(c.alpm_pkg_get_depends(p))));
    ui.fieldLines("Optional", try optionals(db, try alpm.dependencies(c.alpm_pkg_get_optdepends(p))));
    ui.fieldList("Provides", try alpm.dependencies(c.alpm_pkg_get_provides(p)));
    ui.fieldList("Conflicts with", try alpm.dependencies(c.alpm_pkg_get_conflicts(p)));
    ui.fieldList("Replaces", try alpm.dependencies(c.alpm_pkg_get_replaces(p)));
    if (local) {
        const required = c.alpm_pkg_compute_requiredby(p);
        defer alpm.freeStrings(required);
        ui.field("Required by", try std.mem.join(u.a, "  ", try alpm.strings(required)));
        const optional = c.alpm_pkg_compute_optionalfor(p);
        defer alpm.freeStrings(optional);
        ui.fieldList("Optional for", try alpm.strings(optional));
    }
    ui.blank();
}

/// Optional dependencies, `name: why`, marked when one is installed.
fn optionals(db: *alpm.Alpm, deps: []const []const u8) ![]const []const u8 {
    const marked = try u.a.alloc([]const u8, deps.len);
    for (deps, marked) |dep, *line| {
        const name = dep[0 .. std.mem.indexOf(u8, dep, ": ") orelse dep.len];
        line.* = if (try db.local(name) != null) try std.fmt.allocPrint(u.a, "{s} · installed", .{dep}) else dep;
    }
    return marked;
}

fn aurDetails(db: *alpm.Alpm, p: aur.Package) !void {
    var entry = try aurEntry(db, p, null);
    entry.meta = "";
    ui.blank();
    ui.entry(entry);
    ui.blank();
    ui.fieldOptional("URL", p.URL orelse "");
    ui.field("AUR page", try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/packages/{s}", .{p.Name}));
    if (!std.mem.eql(u8, p.PackageBase, p.Name)) ui.field("Package base", p.PackageBase);
    ui.fieldList("Licenses", p.License);
    ui.field("Maintainer", p.Maintainer orelse "none (orphaned)");
    ui.field("Votes", try std.fmt.allocPrint(u.a, "{d} · popularity {d:.2}", .{ p.NumVotes, p.Popularity }));
    ui.field("First submitted", ui.dated(p.FirstSubmitted));
    ui.field("Last modified", ui.dated(p.LastModified));
    if (p.OutOfDate) |flagged| ui.field("Out of date since", ui.dated(flagged));
    ui.field("Depends on", try std.mem.join(u.a, "  ", p.Depends));
    ui.fieldList("Build needs", p.MakeDepends);
    ui.fieldList("Check needs", p.CheckDepends);
    ui.fieldLines("Optional", try optionals(db, p.OptDepends));
    ui.fieldList("Provides", p.Provides);
    ui.fieldList("Conflicts with", p.Conflicts);
    ui.blank();
}

// ── Installed packages ──────────────────────────────────────────────────────

/// `list` and `foreign`: every installed package, or only the named ones.
pub fn list(db: *alpm.Alpm, o: cli.Options) !void {
    for (o.names) |name| _ = try installed(db, name);
    ui.hold();
    defer ui.release();
    var table = ui.Table.init(&.{ .{ .title = "Package", .tone = .bold }, .{ .title = "Version", .tone = .success }, .{ .title = "Reason" }, .{ .title = "Installed", .tone = .muted } });
    var total: usize = 0;
    var explicit: usize = 0;
    var it = db.installed();
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (o.names.len > 0 and !u.contains(o.names, alpm.name(p))) continue;
        if (o.command == .foreign and !db.foreign(p)) continue;
        total += 1;
        explicit += @intFromBool(alpm.explicit(p));
        if (o.quiet) {
            ui.print("{s}\n", .{ui.safe(alpm.name(p))});
            continue;
        }
        table.add(&.{
            .{ .text = alpm.name(p) },
            .{ .text = alpm.version(p) },
            .{ .text = alpm.reason(p), .tone = if (alpm.explicit(p)) .plain else .muted },
            .{ .text = ui.date(c.alpm_pkg_get_installdate(p)) },
        });
    }
    if (o.quiet) return;
    const foreign = o.command == .foreign;
    ui.heading(if (foreign) "Foreign packages" else "Installed packages", try std.fmt.allocPrint(u.a, "{s} · {d} explicit · {d} as dependencies", .{ ui.count(total, "package", "packages"), explicit, total - explicit }));
    table.render();
    if (total == 0) ui.note(.info, if (foreign) "Every installed package comes from a configured repository." else "No packages installed.");
    ui.blank();
}

/// Whether a path recorded in a package can be joined to the root directory
/// without leaving it.
pub fn validFile(path: []const u8) bool {
    if (path.len == 0 or std.fs.path.isAbsolute(path)) return false;
    var parts = std.mem.splitScalar(u8, path, '/');
    while (parts.next()) |part| if (std.mem.eql(u8, part, "..") or std.mem.eql(u8, part, ".")) return false;
    return true;
}

pub fn filePath(db: *alpm.Alpm, path: []const u8) ![]const u8 {
    if (!validFile(path)) return errors.with(error.InvalidPackageFile, "{s}", .{path});
    return std.fs.path.join(u.a, &.{ db.cfg.root, path });
}

pub fn fileEntries(p: alpm.Pkg) ![]const c.alpm_file_t {
    const file_list = c.alpm_pkg_get_files(p) orelse return error.InvalidPackageFile;
    if (file_list.*.count == 0) return &.{};
    if (file_list.*.files == null) return error.InvalidPackageFile;
    return file_list.*.files[0..file_list.*.count];
}

pub fn files(db: *alpm.Alpm, names: []const []const u8, quiet: bool) !void {
    ui.hold();
    defer ui.release();
    for (names) |name| {
        const p = try installed(db, name);
        for (try fileEntries(p)) |file| {
            const path = ui.safe(try filePath(db, u.str(file.name)));
            if (quiet) ui.print("{s}\n", .{path}) else ui.print("{s}{s}{s} {s}\n", .{ ui.style(.bold), ui.safe(name), ui.reset(), path });
        }
    }
}

/// The path of an installed file relative to the root directory, as package
/// file lists record it. Directory aliases such as /bin are resolved; the
/// final component is kept, so a symlink is looked up as itself.
fn recordedPath(db: *alpm.Alpm, input: []const u8) !?[]const u8 {
    const resolved = try std.fs.path.resolve(u.a, &.{ try std.process.getCwdAlloc(u.a), input });
    const parent = std.fs.cwd().realpathAlloc(u.a, std.fs.path.dirname(resolved) orelse "/") catch |err| switch (err) {
        error.FileNotFound, error.NotDir => return null,
        else => return err,
    };
    const path = try std.fs.path.join(u.a, &.{ parent, std.fs.path.basename(resolved) });
    const root = std.mem.trimEnd(u8, db.cfg.root, "/");
    if (!std.mem.startsWith(u8, path, root) or path.len <= root.len or path[root.len] != '/') return null;
    return path[root.len + 1 ..];
}

pub fn owners(db: *alpm.Alpm, paths: []const []const u8, quiet: bool) !void {
    var unowned: usize = 0;
    var table = ui.Table.init(&.{ .{ .title = "File" }, .{ .title = "Package", .tone = .bold }, .{ .title = "Version", .tone = .success }, .{ .title = "Status" } });
    var absent: usize = 0;
    for (paths) |input| {
        var owner: ?alpm.Pkg = null;
        if (try recordedPath(db, input)) |recorded| {
            var it = db.installed();
            search: while (it != null) : (it = it.*.next) {
                const p = alpm.pkg(it.*.data);
                for (try fileEntries(p)) |file| {
                    if (!std.mem.eql(u8, std.mem.trimEnd(u8, u.str(file.name), "/"), recorded)) continue;
                    owner = p;
                    break :search;
                }
            }
        }
        if (owner) |p| {
            if (quiet) ui.print("{s}\n", .{ui.safe(alpm.name(p))});
            table.addText(&.{ input, alpm.name(p), alpm.version(p), "" });
        } else {
            unowned += 1;
            // A path that does not exist is a different problem from a stray file.
            var st: c.struct_stat = undefined;
            const exists = c.lstat((try u.z(input)).ptr, &st) == 0;
            absent += @intFromBool(!exists);
            table.add(&.{ .{ .text = input }, .{ .text = "—", .tone = .muted }, .{ .text = "—", .tone = .muted }, .{ .text = if (exists) "not owned by any package" else "does not exist", .tone = if (exists) .danger else .warning } });
        }
    }
    if (!quiet) {
        ui.heading("File owners", ui.count(paths.len, "file", "files"));
        table.render();
    }
    if (unowned > 0) {
        if (!quiet) {
            ui.blank();
            if (unowned > absent) ui.notef(.warning, "{s} not owned by any installed package.", .{ui.count(unowned - absent, "file is", "files are")});
            if (absent > 0) ui.notef(.warning, "{s} not exist.", .{ui.count(absent, "path does", "paths do")});
            ui.blank();
        }
        return error.FileNotOwned;
    }
    if (!quiet) ui.blank();
}

/// Check that installed files exist. Contents are not verified. Files in
/// directories this user cannot search are counted, not treated as missing.
pub fn check(db: *alpm.Alpm, names: []const []const u8) !void {
    for (names) |name| _ = try installed(db, name);
    const root = std.mem.trimEnd(u8, db.cfg.root, "/");
    var path: std.ArrayList(u8) = .empty;
    var packages: usize = 0;
    var total: usize = 0;
    var missing: usize = 0;
    var unreadable: usize = 0;
    ui.heading("Installed files", if (names.len == 0) "all packages" else ui.count(names.len, "package", "packages"));
    var it = db.installed();
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        if (names.len > 0 and !u.contains(names, alpm.name(p))) continue;
        const package_files = try fileEntries(p);
        var absent: std.ArrayList([]const u8) = .empty;
        var hidden: usize = 0;
        for (package_files) |file| {
            const recorded = u.str(file.name);
            if (!validFile(recorded)) return errors.with(error.InvalidPackageFile, "{s}: {s}", .{ alpm.name(p), recorded });
            path.clearRetainingCapacity();
            try path.appendSlice(u.a, root);
            try path.append(u.a, '/');
            try path.appendSlice(u.a, std.mem.trimEnd(u8, recorded, "/"));
            try path.append(u.a, 0);
            const shown = path.items[0 .. path.items.len - 1];
            var st: c.struct_stat = undefined;
            if (c.lstat(@ptrCast(path.items.ptr), &st) == 0) continue;
            switch (std.posix.errno(-1)) {
                .NOENT, .NOTDIR => try absent.append(u.a, try u.a.dupe(u8, shown)),
                .ACCES => hidden += 1,
                else => return errors.with(error.FileInspectionFailed, "{s}", .{shown}),
            }
        }
        packages += 1;
        total += package_files.len;
        missing += absent.items.len;
        unreadable += hidden;
        if (absent.items.len > 0) {
            ui.notef(.failure, "{s} · {d} of {s} missing", .{ alpm.name(p), absent.items.len, ui.count(package_files.len, "file", "files") });
            for (absent.items) |absent_path| ui.line(absent_path, 6, .muted);
        } else if (names.len > 0) {
            const present = ui.count(package_files.len - hidden, "file", "files");
            if (hidden > 0) {
                ui.notef(.warning, "{s} · {s} present, {d} not visible to your user", .{ alpm.name(p), present, hidden });
            } else ui.notef(.success, "{s} · {s} present", .{ alpm.name(p), present });
        }
    }
    if (names.len != 1) {
        const summary = try std.fmt.allocPrint(u.a, "Checked {s} · {s} · {d} missing", .{ ui.count(packages, "package", "packages"), ui.count(total, "file", "files"), missing });
        ui.note(if (missing == 0) .success else .failure, summary);
        if (unreadable > 0) ui.notef(.info, "{s} in directories your user cannot read could not be checked.", .{ui.count(unreadable, "file", "files")});
    }
    ui.blank();
    if (missing != 0) return error.MissingInstalledFiles;
}

pub fn stats(db: *alpm.Alpm) !void {
    var total: usize = 0;
    var explicit: usize = 0;
    var foreign: usize = 0;
    var size: i64 = 0;
    var packages: std.ArrayList(alpm.Pkg) = .empty;
    var it = db.installed();
    while (it != null) : (it = it.*.next) {
        const p = alpm.pkg(it.*.data);
        total += 1;
        explicit += @intFromBool(alpm.explicit(p));
        foreign += @intFromBool(db.foreign(p));
        size += c.alpm_pkg_get_isize(p);
        try packages.append(u.a, p);
    }
    const orphans = try db.orphans();
    ui.heading("Installed packages", "");
    inline for (.{ "Packages", "Explicit", "Dependencies", "Foreign", "Orphans" }, .{ total, explicit, total - explicit, foreign, orphans.len }) |label, value| {
        ui.field(label, try std.fmt.allocPrint(u.a, "{d}", .{value}));
    }
    ui.field("Installed size", ui.size(size));

    std.mem.sort(alpm.Pkg, packages.items, {}, larger);
    const shown = packages.items[0..@min(10, packages.items.len)];
    if (shown.len == 0) return ui.blank();
    ui.heading("Largest packages", "");
    const largest = c.alpm_pkg_get_isize(shown[0]);
    var table = ui.Table.init(&.{ .{ .tone = .muted, .alignment = .right }, .{ .tone = .bold }, .{ .alignment = .right }, .{ .tone = .accent } });
    for (shown, 1..) |p, rank| {
        const package_size = c.alpm_pkg_get_isize(p);
        table.addText(&.{ try std.fmt.allocPrint(u.a, "{d}", .{rank}), alpm.name(p), ui.size(package_size), ui.bar(package_size, largest, 24) });
    }
    table.render();
    ui.blank();
}

fn larger(_: void, a: alpm.Pkg, b: alpm.Pkg) bool {
    return c.alpm_pkg_get_isize(a) > c.alpm_pkg_get_isize(b);
}

// ── Tests ───────────────────────────────────────────────────────────────────

test "interactive selections reject invalid ranges and deduplicate" {
    try std.testing.expectEqualSlices(usize, &.{ 0, 1, 2, 4 }, try selection("1-3, 2 5", 5));
    for ([_][]const u8{ "0", "6", "3-1", "1-9999999999", "-1", "all", "1;id" }) |input| try std.testing.expectError(error.InvalidSelection, selection(input, 5));
    try std.testing.expectError(error.Cancelled, selection("", 5));
}

test "installed file paths cannot escape the configured root" {
    for ([_][]const u8{ "", "/etc/passwd", "../etc/passwd", "usr/../../etc", "usr/./bin" }) |path| try std.testing.expect(!validFile(path));
    try std.testing.expect(validFile("usr/bin/tool"));
    try std.testing.expect(validFile("usr/share/"));
}

test "AUR sort is deterministic with name tie breaks" {
    var packages = [_]aur.Package{
        .{ .Name = "z", .PackageBase = "z", .Version = "1", .NumVotes = 2 },
        .{ .Name = "a", .PackageBase = "a", .Version = "1", .NumVotes = 2 },
        .{ .Name = "b", .PackageBase = "b", .Version = "1", .NumVotes = 9 },
    };
    std.mem.sort(aur.Package, &packages, AurOrder{ .term = "", .by = .votes }, aurBefore);
    try std.testing.expectEqualStrings("b", packages[0].Name);
    try std.testing.expectEqualStrings("a", packages[1].Name);
    std.mem.sort(aur.Package, &packages, AurOrder{ .term = "", .by = .name }, aurBefore);
    try std.testing.expectEqualStrings("a", packages[0].Name);
    try std.testing.expectEqualStrings("z", packages[2].Name);
    // The exact name outranks votes, then names that start with the term.
    std.mem.sort(aur.Package, &packages, AurOrder{ .term = "z", .by = .votes }, aurBefore);
    try std.testing.expectEqualStrings("z", packages[0].Name);
    try std.testing.expectEqual(@as(u2, 1), closeness("ripgrep-all", "ripgrep"));
    try std.testing.expectEqual(@as(u2, 2), closeness("repgrep", "ripgrep"));
}

test "installed search badges require an exact identity, not a virtual provider" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    try native.installedFixture(tmp.dir, "replacement", "");
    const desc_path = "db/local/replacement-1.0-1/desc";
    const desc = try tmp.dir.readFileAlloc(u.a, desc_path, 4096);
    try tmp.dir.writeFile(.{ .sub_path = desc_path, .data = try std.fmt.allocPrint(u.a, "{s}%PROVIDES%\noriginal\n\n", .{desc}) });
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    try std.testing.expect(try db.local("original") != null);
    try std.testing.expect(try installedBadge(&db, "original", "1.0-1") == null);
    try std.testing.expectEqualStrings("✓ installed", (try installedBadge(&db, "replacement", "1.0-1")).?.text);
    try std.testing.expectEqualStrings("installed 1.0-1 · update available", (try installedBadge(&db, "replacement", "2.0-1")).?.text);
    try std.testing.expectEqual(ui.Tone.warning, (try installedBadge(&db, "replacement", "2.0-1")).?.tone);
    try std.testing.expectEqualStrings("✓ installed 1.0-1", (try installedBadge(&db, "replacement", "0.9-1")).?.text);
}

test "file queries tolerate empty package lists and resolve directory aliases" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    try native.installedFixture(tmp.dir, "empty", "");
    try native.installedFixture(tmp.dir, "owner", "usr/bin/tool");
    try tmp.dir.makePath("usr/bin");
    try tmp.dir.writeFile(.{ .sub_path = "usr/bin/tool", .data = "tool" });
    try tmp.dir.symLink("usr/bin", "bin", .{});
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{ .root = root } };
    defer db.deinit();
    try std.testing.expectEqual(@as(usize, 0), (try fileEntries(try installed(&db, "empty"))).len);
    try std.testing.expectEqualStrings("usr/bin/tool", (try recordedPath(&db, try std.fs.path.join(u.a, &.{ root, "bin/tool" }))).?);
    try std.testing.expect(try recordedPath(&db, "/outside-the-root") == null);
    try std.testing.expect(try recordedPath(&db, try std.fs.path.join(u.a, &.{ root, "no-such-directory/file" })) == null);
    try owners(&db, &.{try std.fs.path.join(u.a, &.{ root, "bin/tool" })}, true);
    try check(&db, &.{ "empty", "owner" });
    try tmp.dir.deleteFile("usr/bin/tool");
    try std.testing.expectError(error.MissingInstalledFiles, check(&db, &.{"owner"}));
    try std.testing.expectError(error.FileNotOwned, owners(&db, &.{try std.fs.path.join(u.a, &.{ root, "usr/bin/missing" })}, true));
    try std.testing.expectError(error.FileNotOwned, owners(&db, &.{try std.fs.path.join(u.a, &.{ root, "no-such-directory/file" })}, true));
    try std.testing.expectError(error.PackageNotInstalled, check(&db, &.{"absent"}));
}

test "installed listings are a table for people and bare names for scripts" {
    _ = c.setlocale(c.LC_CTYPE, "C.UTF-8");
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const native = @import("native_tests.zig");
    try native.installedFixture(tmp.dir, "alpha", "");
    try native.installedFixture(tmp.dir, "beta", "");
    var db: alpm.Alpm = .{ .h = try native.handle(root), .cfg = .{} };
    defer db.deinit();
    defer ui.setCaps(null);
    ui.setCaps(.{ .width = 80 });

    ui.beginCapture();
    try list(&db, .{ .command = .list });
    try std.testing.expectEqualStrings(
        \\
        \\▌ Installed packages  2 packages · 0 explicit · 2 as dependencies
        \\  Package  Version  Reason      Installed
        \\  alpha    1.0-1    dependency  unknown
        \\  beta     1.0-1    dependency  unknown
        \\
        \\
    , ui.endCapture());

    ui.beginCapture();
    try list(&db, .{ .command = .list, .quiet = true, .names = &.{"beta"} });
    try std.testing.expectEqualStrings("beta\n", ui.endCapture());

    // No repository is configured for the fixture, so everything is foreign.
    ui.beginCapture();
    try list(&db, .{ .command = .foreign, .quiet = true });
    try std.testing.expectEqualStrings("alpha\nbeta\n", ui.endCapture());
    try std.testing.expectError(error.PackageNotInstalled, list(&db, .{ .command = .list, .names = &.{"gamma"} }));

    ui.beginCapture();
    try info(&db, .{ .command = .localinfo, .names = &.{"alpha"} });
    const details = ui.endCapture();
    for ([_][]const u8{ "local/alpha 1.0-1", "Installed fixture", "Reason              dependency", "Depends on          none", "Required by         none" }) |expected| {
        try std.testing.expect(std.mem.indexOf(u8, details, expected) != null);
    }
    // Fields with nothing to say are left out.
    for ([_][]const u8{ "Provides", "Conflicts with", "Groups", "Optional" }) |absent| {
        try std.testing.expect(std.mem.indexOf(u8, details, absent) == null);
    }
}
