//! AUR and local builds: dependency planning from static metadata, source
//! review, sandboxed makepkg runs, and hand-off of verified archives to the
//! transaction worker.
//!
//! Nothing from a package is executed before its sources are approved, and
//! approved sources are re-verified around every step that could change them.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const tx = @import("transaction.zig");
const cli = @import("cli.zig");
const srcinfo = @import("srcinfo.zig");
const artifacts = @import("artifacts.zig");
const sandbox = @import("sandbox.zig");
const errors = @import("errors.zig");

const srcinfo_limit = 2 * 1024 * 1024;
const source_limit = 16 * 1024 * 1024;
const aur_git = "https://aur.archlinux.org/{s}.git";
/// The commit of a package base you last approved. Diffs are shown against
/// it, so fetching for a dry run, `get` or `pkgbuild` never hides a change.
const approved_ref = "refs/zap/approved";

/// One package base to build, from the AUR or from a local snapshot.
const Node = struct {
    package: aur.Package,
    dir: []const u8,
    metadata: srcinfo.Metadata,
    /// The packages of this base that the plan installs.
    wanted: std.ArrayList([]const u8) = .empty,
    /// On the current resolution path; meeting it again is a cycle.
    visiting: bool = true,
    planned: bool = false,
    reviewed: bool = false,
    /// Fingerprint of the tracked sources the user approved.
    source_digest: ?[]const u8 = null,
    local_source: bool = false,
};

pub const Builder = struct {
    db: *alpm.Alpm,
    root: []const u8,
    lock: c_int,
    requested: []const []const u8,
    preserve_reasons: bool = false,
    scope: cli.Scope = .all,
    /// Requested names that must come from the AUR even if a repository has them.
    aur_targets: []const []const u8 = &.{},
    needed: bool = false,
    sandbox: bool = true,
    clean_after: bool = false,
    rebuild_tree: bool = false,
    reason: ?cli.Reason = null,
    nodes: std.ArrayList(Node) = .empty,
    /// Indices into `nodes`, dependencies before dependants.
    order: std.ArrayList(usize) = .empty,
    repos: std.ArrayList(tx.Target) = .empty,
    reason_targets: std.ArrayList(tx.Target) = .empty,
    explicit_names: std.ArrayList([]const u8) = .empty,

    /// Open the user's build cache and take its lock, so that two builds
    /// never share a checkout.
    pub fn init(db: *alpm.Alpm, requested: []const []const u8) !Builder {
        if (c.geteuid() == 0) return error.BuildsMustNotRunAsRoot;
        const home = std.posix.getenv("HOME") orelse return error.HomeNotSet;
        const cache = std.posix.getenv("XDG_CACHE_HOME") orelse try std.fs.path.join(u.a, &.{ home, ".cache" });
        if (!std.fs.path.isAbsolute(cache)) return errors.with(error.InvalidCacheDirectory, "{s}", .{cache});
        const root = try std.fs.path.join(u.a, &.{ cache, "zap" });
        try u.secureDir(root);
        const lockpath = try std.fs.path.join(u.a, &.{ root, ".lock" });
        const lock = c.open((try u.z(lockpath)).ptr, @as(c_int, c.O_CREAT | c.O_RDWR | c.O_CLOEXEC | c.O_NOFOLLOW | c.O_NONBLOCK), @as(c_uint, 0o600));
        if (lock < 0) return error.UnsafeCacheLock;
        errdefer _ = c.close(lock);
        var st: c.struct_stat = undefined;
        if (c.fstat(lock, &st) != 0 or st.st_uid != c.getuid() or st.st_mode & c.S_IFMT != c.S_IFREG) return error.UnsafeCacheLock;
        if (c.flock(lock, c.LOCK_EX | c.LOCK_NB) != 0) return error.AnotherBuildIsRunning;
        return .{ .db = db, .root = root, .lock = lock, .requested = requested };
    }

    pub fn deinit(self: *Builder) void {
        _ = c.close(self.lock);
    }

    // ── Planning ────────────────────────────────────────────────────────────

    /// Resolve every requested package and its dependencies, then show the plan.
    pub fn plan(self: *Builder) !void {
        const local_count = self.nodes.items.len;
        for (0..local_count) |index| {
            if (self.nodes.items[index].local_source) try self.planLocal(index, 0);
        }
        for (self.requested) |name| try self.resolve(name, true, 0);
        // makepkg itself needs the base-devel group's tools.
        if (self.nodes.items.len > 0) try self.resolve("base-devel", false, 0);
        try self.showPlan();
    }

    fn showPlan(self: *Builder) !void {
        var builds: usize = 0;
        for (self.order.items) |index| builds += self.nodes.items[index].wanted.items.len;
        var parts: std.ArrayList([]const u8) = .empty;
        if (self.repos.items.len > 0) try parts.append(u.a, ui.count(self.repos.items.len, "repository package", "repository packages"));
        if (builds > 0) try parts.append(u.a, ui.count(builds, "package to build", "packages to build"));
        if (self.reason_targets.items.len > 0) try parts.append(u.a, ui.count(self.reason_targets.items.len, "reason change", "reason changes"));
        if (parts.items.len == 0) return ui.note(.success, "Nothing to do.");

        ui.heading("Plan", try std.mem.join(u.a, " · ", parts.items));
        var table = ui.Table.init(&.{
            .{ .title = "Package", .tone = .bold },
            .{ .title = "Current" },
            .{ .title = "New", .tone = .success, .joiner = " → " },
            .{ .title = "Change" },
            .{ .title = "Source" },
            .{ .title = "Reason", .tone = .muted },
        });
        var download: i64 = 0;
        var growth: i64 = 0;
        var reinstalls: std.ArrayList([]const u8) = .empty;
        for (self.repos.items) |target| {
            const p = try self.db.repo(target.name) orelse continue;
            const old = try self.db.installedPkg(target.name);
            download += c.alpm_pkg_download_size(p);
            growth += c.alpm_pkg_get_isize(p) - (if (old) |previous| c.alpm_pkg_get_isize(previous) else 0);
            if (try planRow(&table, target.name, old, alpm.version(p), alpm.repository(p), !target.dependency)) try reinstalls.append(u.a, target.name);
        }
        for (self.order.items) |index| {
            const node = self.nodes.items[index];
            const origin: []const u8 = if (node.local_source) "local" else "aur";
            for (node.wanted.items) |wanted| {
                const split = !std.mem.eql(u8, wanted, node.package.PackageBase);
                const source = if (split) try std.fmt.allocPrint(u.a, "{s} · base {s}", .{ origin, node.package.PackageBase }) else origin;
                if (try planRow(&table, wanted, try self.db.installedPkg(wanted), node.package.Version, source, u.contains(self.explicit_names.items, wanted))) try reinstalls.append(u.a, wanted);
            }
        }
        table.render();
        ui.blank();
        if (self.repos.items.len > 0) {
            ui.field("Download", if (download == 0) "nothing: already in the package cache" else ui.size(download));
            ui.field("Disk space", if (growth == 0) "no change" else try std.fmt.allocPrint(u.a, "{s} for repository packages", .{ui.sizeDelta(growth)}));
            ui.note(.info, "Repository dependencies, conflicts and replacements are settled in the final transaction, which you confirm separately.");
        }
        if (builds > 0) {
            if (self.sandbox) {
                ui.note(.info, "Packages to build are reviewed, then built in a sandbox and sized before anything is installed.");
            } else ui.note(.warning, "Sandbox disabled: approved build code will run with your user's full permissions.");
        }
        for (self.reason_targets.items) |target| {
            ui.notef(.info, "{s} is already current; it will be marked {s}.", .{ target.name, @tagName(target.reason.?) });
        }
        if (reinstalls.items.len > 0) {
            ui.notef(.warning, "{s} already installed at this version. Add --needed to skip current packages.", .{
                if (reinstalls.items.len == 1) try std.fmt.allocPrint(u.a, "{s} is", .{reinstalls.items[0]}) else ui.count(reinstalls.items.len, "package is", "packages are"),
            });
        }
        ui.blank();
    }

    /// One plan row. Returns whether it reinstalls the installed version.
    fn planRow(table: *ui.Table, name: []const u8, old: ?alpm.Pkg, version: []const u8, source: []const u8, requested: bool) !bool {
        const kind: []const u8, const tone: ui.Tone = if (old) |previous| blk: {
            const cmp = c.alpm_pkg_vercmp((try u.z(version)).ptr, c.alpm_pkg_get_version(previous));
            break :blk if (cmp > 0) .{ "upgrade", .accent } else if (cmp < 0) .{ "downgrade", .warning } else .{ "reinstall", .warning };
        } else .{ "install", .accent };
        const same = if (old) |previous| std.mem.eql(u8, alpm.version(previous), version) else false;
        table.add(&.{
            .{ .text = name },
            .{ .text = if (old) |previous| alpm.version(previous) else "—" },
            // A version that does not change is not shown as new.
            .{ .text = version, .tone = if (same) .plain else null },
            .{ .text = kind, .tone = tone },
            .{ .text = source, .tone = if (std.mem.startsWith(u8, source, "aur")) .special else .accent },
            .{ .text = if (requested) "requested" else "dependency" },
        });
        return std.mem.eql(u8, kind, "reinstall");
    }

    /// `--needed`: leave a current package alone, but still honour a
    /// requested change of installation reason.
    fn skipInstalled(self: *Builder, old: alpm.Pkg) !void {
        ui.notef(.success, "{s} {s} is already installed and current.", .{ alpm.name(old), alpm.version(old) });
        const reason = self.reason orelse .explicit;
        const desired: c.alpm_pkgreason_t = if (reason == .explicit) c.ALPM_PKG_REASON_EXPLICIT else c.ALPM_PKG_REASON_DEPEND;
        if (c.alpm_pkg_get_reason(old) != desired) try self.reason_targets.append(u.a, .{ .name = alpm.name(old), .reason = reason });
    }

    fn current(self: *Builder, name: [*c]const u8, version: [*c]const u8) ?alpm.Pkg {
        const old = c.alpm_db_get_pkg(self.db.localDb(), name) orelse return null;
        return if (c.alpm_pkg_vercmp(c.alpm_pkg_get_version(old), version) >= 0) old else null;
    }

    fn addRepo(self: *Builder, name: []const u8, explicit: bool) !void {
        for (self.repos.items) |*target| if (std.mem.eql(u8, target.name, name)) {
            if (explicit) {
                target.dependency = false;
                target.reason = self.reason;
            }
            return;
        };
        try self.repos.append(u.a, .{ .name = name, .dependency = !explicit, .reason = if (explicit) self.reason else null });
    }

    /// Decide where a dependency comes from, in order of preference: a local
    /// snapshot supplied on the command line, an installed package, a
    /// repository, a package base already in the plan, then the AUR.
    fn resolve(self: *Builder, dep: []const u8, explicit: bool, depth: usize) anyerror!void {
        if (depth > 128 or self.nodes.items.len > 512) return error.DependencyGraphTooLarge;
        if (!u.validName(u.depName(dep))) return errors.with(error.InvalidDependency, "{s}", .{dep});

        for (self.nodes.items, 0..) |node, index| if (node.local_source) {
            if (try node.metadata.provider(dep)) |name| {
                try self.planLocal(index, depth + 1);
                if (explicit) try u.appendUnique(&self.explicit_names, name);
                return;
            }
        };

        if (!explicit) if (try self.db.local(dep)) |installed| {
            // --rebuildtree rebuilds installed foreign dependencies only;
            // repository packages stay satisfied.
            if (!self.rebuild_tree or !self.db.foreign(installed)) return;
        };

        const from_aur = explicit and (self.scope == .aur or u.contains(self.aur_targets, u.depName(dep)));
        if (!from_aur) if (try self.db.repo(dep)) |p| {
            if (explicit) try u.appendUnique(&self.explicit_names, alpm.name(p));
            if (explicit and self.needed) if (self.current(c.alpm_pkg_get_name(p), c.alpm_pkg_get_version(p))) |old| {
                return self.skipInstalled(old);
            };
            return self.addRepo(alpm.name(p), explicit);
        };
        if (explicit and self.scope == .repo) return errors.with(error.RepositoryPackageNotFound, "{s}", .{dep});

        for (self.nodes.items, 0..) |node, index| {
            const name = try node.metadata.provider(dep) orelse continue;
            if (node.local_source and !node.planned) try self.planLocal(index, depth + 1);
            if (self.nodes.items[index].visiting) return errors.with(error.DependencyCycle, "{s}", .{dep});
            if (explicit) try u.appendUnique(&self.explicit_names, name);
            try u.appendUnique(&self.nodes.items[index].wanted, name);
            return;
        }

        const p = try chooseAur(dep);
        if (explicit and self.needed) if (self.current((try u.z(p.Name)).ptr, (try u.z(p.Version)).ptr)) |old| {
            return self.skipInstalled(old);
        };
        if (explicit) try u.appendUnique(&self.explicit_names, p.Name);
        for (self.nodes.items, 0..) |node, index| if (std.mem.eql(u8, node.package.PackageBase, p.PackageBase)) {
            if (node.visiting) return errors.with(error.DependencyCycle, "{s}", .{dep});
            try u.appendUnique(&self.nodes.items[index].wanted, p.Name);
            return;
        };

        const dir = try self.checkout(p.PackageBase, true);
        const meta = try srcinfo.Metadata.parse(try u.readFile(try std.fs.path.join(u.a, &.{ dir, ".SRCINFO" }), srcinfo_limit), self.db.cfg.arch);
        // The checkout must be the package the RPC described.
        if (!std.mem.eql(u8, meta.base, p.PackageBase) or !u.contains(meta.names.items, p.Name) or !try meta.matches(dep, p.Name)) {
            return errors.with(error.AurMetadataMismatch, "{s}", .{p.PackageBase});
        }
        const index = self.nodes.items.len;
        var node: Node = .{ .package = p, .dir = dir, .metadata = meta };
        try node.wanted.append(u.a, p.Name);
        try self.nodes.append(u.a, node);
        // For safety the plan covers the dependencies of every split package
        // in the base, since building the base builds them all.
        for (meta.deps.items) |dependency| {
            if (try meta.provider(dependency)) |sibling| {
                try u.appendUnique(&self.nodes.items[index].wanted, sibling);
            } else try self.resolve(dependency, false, depth + 1);
        }
        self.nodes.items[index].visiting = false;
        self.nodes.items[index].planned = true;
        try self.order.append(u.a, index);
    }

    /// The AUR package for a dependency: the package of that name if it
    /// satisfies the version, otherwise a provider, chosen by the user when
    /// several qualify.
    fn chooseAur(dep: []const u8) !aur.Package {
        const name = u.depName(dep);
        if (try aur.find(name)) |exact| {
            if (try srcinfo.satisfies(dep, exact.Name, exact.Version, exact.Provides)) return exact;
        }
        // Search results lack provides and versions: fetch full metadata for
        // every candidate in one batched request.
        const candidates = try aur.search(name, .provides);
        var names: std.ArrayList([]const u8) = .empty;
        for (candidates) |candidate| try names.append(u.a, candidate.Name);
        var providers: std.ArrayList(aur.Package) = .empty;
        for (try aur.info(names.items)) |detail| {
            if (try srcinfo.satisfies(dep, detail.Name, detail.Version, detail.Provides)) try providers.append(u.a, detail);
        }
        std.mem.sort(aur.Package, providers.items, {}, moreVotes);
        if (providers.items.len == 0) return errors.with(error.UnsatisfiedDependency, "{s}", .{dep});
        if (providers.items.len == 1) return providers.items[0];
        ui.heading("Choose a provider", dep);
        for (providers.items, 1..) |p, number| ui.entry(.{
            .index = number,
            .source = "aur",
            .name = p.Name,
            .version = p.Version,
            .description = p.Description orelse "",
            .meta = try std.fmt.allocPrint(u.a, "{s} · popularity {d:.2}", .{ ui.count(@intCast(@max(p.NumVotes, 0)), "vote", "votes"), p.Popularity }),
        });
        const reply = try ui.answer("Provider number:");
        const choice = std.fmt.parseInt(usize, reply, 10) catch return errors.with(error.InvalidProvider, "{s}", .{reply});
        if (choice == 0 or choice > providers.items.len) return errors.with(error.InvalidProvider, "{s}", .{reply});
        return providers.items[choice - 1];
    }

    fn moreVotes(_: void, a: aur.Package, b: aur.Package) bool {
        if (a.NumVotes != b.NumVotes) return a.NumVotes > b.NumVotes;
        return std.mem.lessThan(u8, a.Name, b.Name);
    }

    // ── Local sources ───────────────────────────────────────────────────────

    /// Snapshot local PKGBUILD directories into the cache as reviewable
    /// commits. The originals are never sourced or modified.
    pub fn loadLocal(self: *Builder, dirs: []const []const u8) !void {
        for (dirs) |input| {
            const source = std.fs.cwd().realpathAlloc(u.a, input) catch |err| return errors.with(err, "{s}", .{input});
            const template = try u.a.dupeZ(u8, try std.fs.path.join(u.a, &.{ self.root, ".local-XXXXXX" }));
            if (c.mkdtemp(template.ptr) == null) return error.StagingFailed;
            const destination = std.mem.sliceTo(template, 0);
            errdefer std.fs.cwd().deleteTree(destination) catch {};
            try snapshotLocal(source, destination);
            const meta = try srcinfo.Metadata.parse(try u.readFile(try std.fs.path.join(u.a, &.{ destination, ".SRCINFO" }), srcinfo_limit), self.db.cfg.arch);
            for (self.nodes.items) |node| if (std.mem.eql(u8, node.metadata.base, meta.base)) return errors.with(error.DuplicateTarget, "{s}", .{meta.base});
            for (meta.names.items) |name| {
                if (u.contains(self.explicit_names.items, name)) return errors.with(error.DuplicateTarget, "{s}", .{name});
                try u.appendUnique(&self.explicit_names, name);
            }
            _ = try git(&.{ "init", "--quiet" }, destination);
            _ = try git(&.{ "add", "--force", "--all", "--", "." }, destination);
            _ = try git(&.{ "-c", "user.name=zap local review", "-c", "user.email=local@example.invalid", "commit", "--quiet", "-m", "Local source snapshot" }, destination);
            var node: Node = .{
                .package = .{ .Name = meta.names.items[0], .PackageBase = meta.base, .Version = meta.version, .Maintainer = "local source" },
                .dir = destination,
                .metadata = meta,
                .visiting = false,
                .local_source = true,
            };
            try node.wanted.appendSlice(u.a, meta.names.items);
            try self.nodes.append(u.a, node);
            ui.notef(.info, "Snapshot of {s} taken for review.", .{source});
        }
    }

    fn planLocal(self: *Builder, index: usize, depth: usize) anyerror!void {
        if (depth > 128) return error.DependencyGraphTooLarge;
        if (self.nodes.items[index].planned) return;
        if (self.nodes.items[index].visiting) return errors.with(error.DependencyCycle, "{s}", .{self.nodes.items[index].metadata.base});
        self.nodes.items[index].visiting = true;
        const meta = self.nodes.items[index].metadata;
        for (meta.deps.items) |dep| {
            if (try meta.provider(dep) == null) try self.resolve(dep, false, depth + 1);
        }
        self.nodes.items[index].visiting = false;
        self.nodes.items[index].planned = true;
        try self.order.append(u.a, index);
    }

    // ── Sources ─────────────────────────────────────────────────────────────

    /// Run git with hooks, file transports and filesystem monitors disabled.
    fn git(args: []const []const u8, dir: ?[]const u8) ![]u8 {
        var argv: std.ArrayList([]const u8) = .empty;
        try argv.appendSlice(u.a, &.{ "/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-c", "protocol.file.allow=never", "-c", "core.fsmonitor=false" });
        try argv.appendSlice(u.a, args);
        return u.capture(argv.items, dir, source_limit);
    }

    /// Clone a package base, or fast-forward the cached clone, showing what
    /// changed since the cached copy when `announce` is set. No package code
    /// runs here.
    fn checkout(self: *Builder, base: []const u8, announce: bool) ![]const u8 {
        const dir = try std.fs.path.join(u.a, &.{ self.root, base });
        const url = try std.fmt.allocPrint(u.a, aur_git, .{base});
        var st: c.struct_stat = undefined;
        if (c.lstat((try u.z(dir)).ptr, &st) != 0) {
            _ = try git(&.{ "clone", "--", url, dir }, null);
            try u.secureDir(dir);
        } else {
            if (st.st_uid != c.getuid() or st.st_mode & c.S_IFMT != c.S_IFDIR) return errors.with(error.UnsafeCacheDirectory, "{s}", .{dir});
            const status = try git(&.{ "status", "--porcelain", "--untracked-files=no" }, dir);
            if (status.len > 0) return errors.with(error.ModifiedBuildFiles, "{s}", .{dir});
            const before = std.mem.trim(u8, try git(&.{ "rev-parse", "HEAD" }, dir), "\n");
            _ = try git(&.{ "fetch", "--", url, "master" }, dir);
            _ = try git(&.{ "merge", "--ff-only", "FETCH_HEAD" }, dir);
            const approved = std.mem.trim(u8, try git(&.{ "for-each-ref", "--format=%(objectname)", approved_ref }, dir), "\n");
            const head = std.mem.trim(u8, try git(&.{ "rev-parse", "HEAD" }, dir), "\n");
            const baseline = if (approved.len > 0) approved else before;
            if (announce and !std.mem.eql(u8, baseline, head)) {
                const diff = try git(&.{ "diff", "--no-ext-diff", "--no-textconv", baseline, head, "--" }, dir);
                if (diff.len > 0) {
                    ui.rule(try std.fmt.allocPrint(u.a, "{s} · changes since {s}", .{ base, if (approved.len > 0) "you last approved it" else "your cached copy" }));
                    printDiff(diff);
                }
            }
        }
        // Refuse linked or foreign metadata before any PKGBUILD code can run.
        for ([_][]const u8{ "PKGBUILD", ".SRCINFO" }) |file| {
            const path = try std.fs.path.join(u.a, &.{ dir, file });
            if (c.lstat((try u.z(path)).ptr, &st) != 0 or st.st_mode & c.S_IFMT != c.S_IFREG or st.st_uid != c.getuid()) {
                return errors.with(error.UnsafeBuildFile, "{s}", .{path});
            }
        }
        return dir;
    }

    fn aurBase(name: []const u8) ![]const u8 {
        const p = try aur.find(name) orelse return errors.with(error.AurPackageNotFound, "{s}", .{name});
        return p.PackageBase;
    }

    /// `get`: fetch sources into the cache without executing anything.
    pub fn getSources(self: *Builder) !void {
        for (self.requested) |name| {
            const dir = try self.checkout(try aurBase(name), true);
            ui.notef(.success, "{s} · sources are in {s}", .{ name, dir });
        }
    }

    /// `pkgbuild`: print PKGBUILDs, and nothing else, to standard output.
    pub fn printPkgbuilds(self: *Builder) !void {
        for (self.requested) |name| {
            const dir = try self.checkout(try aurBase(name), false);
            printSource(try u.readFile(try std.fs.path.join(u.a, &.{ dir, "PKGBUILD" }), source_limit));
        }
    }

    /// A fingerprint of every tracked file. `strict` also requires a clean
    /// work tree. A literal pkgver is normalized, because makepkg rewrites it
    /// in place for VCS packages.
    fn sourceDigest(dir: []const u8, strict: bool) ![]const u8 {
        const status = try git(&.{ "status", "--porcelain", "--untracked-files=no" }, dir);
        if (strict and status.len > 0) return errors.with(error.ModifiedBuildFiles, "{s}", .{dir});
        var hash = std.crypto.hash.sha2.Sha256.init(.{});
        const files = try git(&.{ "ls-files", "-z" }, dir);
        hash.update(files);
        var paths = std.mem.splitScalar(u8, files, 0);
        while (paths.next()) |path| {
            if (path.len == 0) continue;
            if (std.fs.path.isAbsolute(path) or std.mem.indexOf(u8, path, "../") != null) return errors.with(error.UnsafeBuildFile, "{s}", .{path});
            const file = try std.fs.path.join(u.a, &.{ dir, path });
            const digest = try tx.hashSourceFile(file);
            if (std.mem.eql(u8, path, "PKGBUILD")) hash.update(try normalizePkgver(try u.readFile(file, source_limit))) else hash.update(digest);
        }
        const digest = hash.finalResult();
        return std.fmt.allocPrint(u.a, "{s}", .{std.fmt.bytesToHex(digest, .lower)});
    }

    fn verifySource(node: Node) !void {
        const approved = node.source_digest orelse return errors.with(error.SourceNotReviewed, "{s}", .{node.package.PackageBase});
        if (!std.mem.eql(u8, approved, try sourceDigest(node.dir, false))) return errors.with(error.BuildSourceChanged, "{s}", .{node.package.PackageBase});
    }

    // ── Review and build ────────────────────────────────────────────────────

    fn makepkgArgs(self: *Builder, dir: []const u8, args: []const []const u8) ![]const []const u8 {
        return if (self.sandbox) sandbox.argv(dir, self.db.cfg.db, args) else args;
    }

    /// Review, build and, when `install` is set, install the plan.
    pub fn execute(self: *Builder, install: bool) !void {
        // Everything needed to install is checked before any review or build.
        // An empty plan has already said "Nothing to do." and needs none of it.
        if (install and self.repos.items.len == 0 and self.order.items.len == 0 and self.reason_targets.items.len == 0) return;
        if (install) {
            _ = try tx.preflight();
        } else try self.requireInstalledDependencies();
        if (self.sandbox and self.order.items.len > 0) {
            // Probe before any approval or installation. A sandbox that cannot
            // start is an error; it never falls back to an unsandboxed build.
            const probe = self.makepkgArgs(self.nodes.items[self.order.items[0]].dir, &.{"/usr/bin/true"}) catch return error.BuildSandboxUnavailable;
            u.run(probe, null) catch return error.BuildSandboxUnavailable;
            ui.note(.info, "Builds run in a sandbox: private home, read-only system and package database, network for downloads only.");
        } else if (self.order.items.len > 0) {
            ui.note(.warning, "Sandbox disabled: approved build code runs with your user's full permissions.");
        }

        // Every base is reviewed before any PKGBUILD code runs or the system changes.
        for (self.order.items, 1..) |index, step| try self.review(&self.nodes.items[index], step);

        // Sourcing the PKGBUILD is the first execution of package code. The
        // plan came from .SRCINFO, so the two must agree.
        for (self.order.items) |index| {
            const node = &self.nodes.items[index];
            try verifySource(node.*);
            const generated = try u.capture(try self.makepkgArgs(node.dir, &.{ "/usr/bin/makepkg", "--printsrcinfo" }), node.dir, srcinfo_limit);
            const actual = try srcinfo.Metadata.parse(generated, self.db.cfg.arch);
            if (!node.metadata.equivalent(actual)) return errors.with(error.StaleSrcInfo, "{s}", .{node.package.PackageBase});
            try verifySource(node.*);
        }

        if (install and self.reason_targets.items.len > 0) try tx.escalate(.{ .operation = .reason, .targets = self.reason_targets.items });
        if (install and self.repos.items.len > 0) {
            try tx.escalate(.{ .operation = .install, .targets = self.repos.items, .needed = self.needed });
            try self.db.reload();
        }
        // Each base is installed before its dependants are built.
        for (self.order.items, 1..) |index, step| try self.build(self.nodes.items[index], step, install);
    }

    /// `build` changes nothing on the system, so everything must be in place.
    pub fn requireInstalledDependencies(self: *Builder) !void {
        if (self.nodes.items.len == 0) return error.BuildRequiresAurPackage;
        for (self.repos.items) |target| {
            if (try self.db.local(target.name) == null) return errors.with(error.MissingBuildDependencies, "{s}", .{target.name});
            if (!target.dependency) return errors.with(error.BuildRequiresAurPackage, "{s} is a repository package", .{target.name});
        }
        for (self.nodes.items) |node| for (node.metadata.deps.items) |dep| {
            if (try node.metadata.provider(dep) != null) continue;
            if (try self.db.local(dep) == null) return errors.with(error.MissingBuildDependencies, "{s}", .{dep});
        };
    }

    fn review(self: *Builder, node: *Node, step: usize) !void {
        const p = node.package;
        ui.heading(try std.fmt.allocPrint(u.a, "Review {d}/{d} · {s}", .{ step, self.order.items.len, p.PackageBase }), p.Version);
        if (node.local_source) {
            ui.field("Source", "local snapshot");
        } else {
            ui.field("Source", try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/packages/{s}", .{p.Name}));
            ui.field("Maintainer", p.Maintainer orelse "none (orphaned)");
            ui.field("Last modified", ui.dated(p.LastModified));
            ui.field("Votes", try std.fmt.allocPrint(u.a, "{d} · popularity {d:.2}", .{ p.NumVotes, p.Popularity }));
            if (p.OutOfDate) |flagged| ui.notef(.warning, "Flagged out of date on {s}.", .{ui.date(flagged)});
            if (p.Maintainer == null) ui.note(.warning, "Orphaned: nobody maintains this package.");
        }
        node.source_digest = try sourceDigest(node.dir, true);
        const files = try git(&.{ "ls-files", "-z" }, node.dir);
        var paths = std.mem.splitScalar(u8, files, 0);
        while (paths.next()) |path| {
            if (path.len == 0) continue;
            const content = try git(&.{ "show", try std.fmt.allocPrint(u.a, "HEAD:{s}", .{path}) }, node.dir);
            ui.rule(path);
            printSource(content);
        }
        ui.blank();
        try ui.require(try std.fmt.allocPrint(u.a, "These build files can run arbitrary code as your user. Approve {s}? [y/N]", .{p.PackageBase}));
        try verifySource(node.*);
        node.reviewed = true;
        // Later diffs start from what was approved here.
        if (!node.local_source) _ = try git(&.{ "update-ref", approved_ref, "HEAD" }, node.dir);
    }

    fn build(self: *Builder, node: Node, step: usize, install: bool) !void {
        if (!node.reviewed) return errors.with(error.SourceNotReviewed, "{s}", .{node.package.PackageBase});
        try verifySource(node);
        ui.heading(try std.fmt.allocPrint(u.a, "Build {d}/{d} · {s}", .{ step, self.order.items.len, node.package.PackageBase }), node.package.Version);
        try u.run(try self.makepkgArgs(node.dir, &.{ "/usr/bin/makepkg", "--cleanbuild", "--clean", "--force", "--noconfirm" }), node.dir);
        try verifySource(node);
        const paths = try u.capture(try self.makepkgArgs(node.dir, &.{ "/usr/bin/makepkg", "--packagelist" }), node.dir, srcinfo_limit);
        const built = try artifacts.collect(self.db, .{
            .base = node.package.PackageBase,
            .dir = node.dir,
            .wanted = node.wanted.items,
            .explicit_names = self.explicit_names.items,
            .preserve_reasons = self.preserve_reasons,
        }, paths);
        try verifySource(node);
        ui.blank();
        for (built.targets, built.versions) |target, version| {
            ui.notef(.success, "Built {s} {s}", .{ target.name, version });
            ui.fieldRaw("  Archive", target.archive.?);
            ui.fieldRaw("  SHA-256", target.sha256.?);
        }
        for (built.targets) |*target| {
            if (u.contains(self.explicit_names.items, target.name)) target.reason = self.reason;
        }
        if (install) {
            try tx.escalate(.{ .operation = .install, .targets = built.targets, .needed = self.needed });
            try self.db.reload();
        }
        // makepkg may rewrite a literal pkgver after a VCS build. Only that
        // validated change is restored; other tracked edits fail verification.
        _ = try git(&.{ "restore", "--source=HEAD", "--", "PKGBUILD" }, node.dir);
        if (self.clean_after and install) _ = try git(&.{ "clean", "-fdx", "--" }, node.dir);
    }

    // ── Cache ───────────────────────────────────────────────────────────────

    /// `clean`: delete the named package bases, or the whole cache.
    pub fn clean(self: *Builder, dry: bool) !void {
        for (self.requested) |base| if (!u.validName(base)) return errors.with(error.InvalidPackageName, "{s}", .{base});
        if (self.requested.len > 0) {
            ui.heading("Clean build cache", ui.count(self.requested.len, "package base", "package bases"));
            for (self.requested) |base| ui.line(try std.fs.path.join(u.a, &.{ self.root, base }), 2, .plain);
        } else {
            ui.heading("Clean build cache", "everything");
            ui.line(self.root, 2, .plain);
        }
        ui.blank();
        if (dry) return;
        try ui.require("Delete these cached sources and build outputs? [y/N]");
        var dir = try std.fs.cwd().openDir(self.root, .{ .iterate = true });
        defer dir.close();
        if (self.requested.len > 0) {
            for (self.requested) |base| try dir.deleteTree(base);
        } else {
            var it = dir.iterate();
            while (try it.next()) |entry| {
                if (std.mem.eql(u8, entry.name, ".lock")) continue;
                if (entry.kind == .directory) try dir.deleteTree(entry.name) else try dir.deleteFile(entry.name);
            }
        }
        ui.note(.success, "Build cache cleaned.");
    }
};

/// Print file contents for review: newlines kept, everything else that could
/// control the terminal neutralized, tabs expanded.
fn printSource(content: []const u8) void {
    ui.hold();
    defer ui.release();
    var lines = std.mem.splitScalar(u8, std.mem.trimEnd(u8, content, "\n"), '\n');
    while (lines.next()) |line| ui.line(expandTabs(line), 0, .plain);
}

/// Like `printSource`, with additions and removals colored.
fn printDiff(content: []const u8) void {
    ui.hold();
    defer ui.release();
    var lines = std.mem.splitScalar(u8, std.mem.trimEnd(u8, content, "\n"), '\n');
    while (lines.next()) |line| {
        const tone: ui.Tone = if (std.mem.startsWith(u8, line, "+++") or std.mem.startsWith(u8, line, "---") or std.mem.startsWith(u8, line, "diff "))
            .bold
        else if (std.mem.startsWith(u8, line, "@@"))
            .accent
        else if (std.mem.startsWith(u8, line, "+"))
            .success
        else if (std.mem.startsWith(u8, line, "-"))
            .danger
        else
            .plain;
        ui.line(expandTabs(line), 0, tone);
    }
}

fn expandTabs(line: []const u8) []const u8 {
    if (std.mem.indexOfScalar(u8, line, '\t') == null) return line;
    return std.mem.replaceOwned(u8, u.a, line, "\t", "    ") catch line;
}

fn normalizePkgver(bytes: []const u8) ![]const u8 {
    var output: std.ArrayList(u8) = .empty;
    var lines = std.mem.splitScalar(u8, bytes, '\n');
    while (lines.next()) |line| {
        var normalized = line;
        if (std.mem.startsWith(u8, line, "pkgver=")) {
            var value = line[7..];
            if (value.len >= 2 and ((value[0] == '\'' and value[value.len - 1] == '\'') or (value[0] == '"' and value[value.len - 1] == '"'))) value = value[1 .. value.len - 1];
            var literal = value.len > 0;
            for (value) |ch| if (!std.ascii.isAlphanumeric(ch) and std.mem.indexOfScalar(u8, "._+", ch) == null) {
                literal = false;
                break;
            };
            if (literal) normalized = "pkgver=<literal>";
        }
        try output.appendSlice(u.a, normalized);
        try output.append(u.a, '\n');
    }
    return output.toOwnedSlice(u.a);
}
test "source fingerprint permits literal VCS pkgver updates and preserves executable changes" {
    const old = try normalizePkgver("pkgver=1.0\nbuild() { echo ok; }");
    const vcs = try normalizePkgver("pkgver='2.0.r4'\nbuild() { echo ok; }");
    try std.testing.expectEqualStrings(old, vcs);
    try std.testing.expect(!std.mem.eql(u8, old, try normalizePkgver("pkgver=2.0;evil\nbuild() { echo ok; }")));
    try std.testing.expect(!std.mem.eql(u8, old, try normalizePkgver("pkgver=2.0\nbuild() { echo evil; }")));
}

test "review and build a local fixture without escalation or installation" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    defer u.a.free(root);
    // Isolate makepkg's per-user configuration and package destinations.
    const old_home = if (std.posix.getenv("HOME")) |home| try u.a.dupeZ(u8, home) else null;
    defer if (old_home) |home| {
        _ = c.setenv("HOME", home.ptr, 1);
        u.a.free(home);
    } else {
        _ = c.unsetenv("HOME");
    };
    try std.testing.expectEqual(@as(c_int, 0), c.setenv("HOME", (try u.z(root)).ptr, 1));
    // Stock Arch enables these options; a text-only fixture has no debug archive.
    try tmp.dir.writeFile(.{ .sub_path = ".makepkg.conf", .data = "OPTIONS+=(strip debug)\n" });
    try tmp.dir.writeFile(.{ .sub_path = "PKGBUILD", .data = 
        \\pkgname=zap-build-fixture
        \\pkgver=1.0
        \\pkgrel=1
        \\pkgdesc='Isolated zap build test'
        \\arch=('any')
        \\license=('MIT')
        \\package() {
        \\  install -d "$pkgdir/usr/share/zap-build-fixture"
        \\  printf 'fixture\n' > "$pkgdir/usr/share/zap-build-fixture/example"
        \\}
        \\
    });
    const generated = try u.capture(&.{ "/usr/bin/makepkg", "--printsrcinfo" }, root, 1024 * 1024);
    try tmp.dir.writeFile(.{ .sub_path = ".SRCINFO", .data = generated });
    _ = try Builder.git(&.{ "init", "--quiet" }, root);
    _ = try Builder.git(&.{ "add", "--", "PKGBUILD", ".SRCINFO" }, root);
    _ = try Builder.git(&.{ "-c", "user.name=zap tests", "-c", "user.email=tests@example.invalid", "commit", "--quiet", "-m", "fixture" }, root);
    const h = try @import("native_tests.zig").handle(root);
    var db: alpm.Alpm = .{ .h = h, .cfg = .{} };
    defer db.deinit();
    var builds: Builder = .{ .db = &db, .root = root, .lock = -1, .requested = &.{"zap-build-fixture"}, .sandbox = false };
    var node: Node = .{
        .package = .{ .Name = "zap-build-fixture", .PackageBase = "zap-build-fixture", .Version = "1.0-1", .Maintainer = "fixture" },
        .dir = root,
        .metadata = try srcinfo.Metadata.parse(generated, "x86_64"),
        .visiting = false,
    };
    try node.wanted.append(u.a, "zap-build-fixture");
    try builds.nodes.append(u.a, node);
    try builds.order.append(u.a, 0);
    try builds.explicit_names.append(u.a, "zap-build-fixture");
    // Supply the single source approval to this isolated fixture only.
    const pipe = try std.posix.pipe();
    defer std.posix.close(pipe[0]);
    defer std.posix.close(pipe[1]);
    const stdin = try std.posix.dup(0);
    defer std.posix.close(stdin);
    try std.posix.dup2(pipe[0], 0);
    defer std.posix.dup2(stdin, 0) catch {};
    _ = try std.posix.write(pipe[1], "y\n");
    try builds.execute(false);
    try std.testing.expect(try db.local("zap-build-fixture") == null);
    const paths = try u.capture(&.{ "/usr/bin/makepkg", "--packagelist" }, root, 1024 * 1024);
    var listed = std.mem.tokenizeScalar(u8, paths, '\n');
    const path = listed.next() orelse return error.MissingBuildArtifacts;
    const debug_path = listed.next() orelse return error.MissingDebugPrediction;
    try std.testing.expect(std.mem.indexOf(u8, debug_path, "-debug-") != null);
    try std.testing.expectError(error.FileNotFound, std.fs.cwd().access(debug_path, .{}));
    try std.testing.expect(listed.next() == null);
    const hash = try tx.hashFile(path);
    try std.testing.expectEqual(@as(usize, 64), hash.len);
}

// Local build files are copied to a fresh review snapshot. Git metadata and
// makepkg work trees are excluded; every other file is tracked for review.
fn snapshotLocal(source: []const u8, destination: []const u8) !void {
    var count: usize = 0;
    var total: u64 = 0;
    try copyLocalTree(source, destination, "", 0, &count, &total);
    for ([_][]const u8{ "PKGBUILD", ".SRCINFO" }) |file| _ = try tx.hashSourceFile(try std.fs.path.join(u.a, &.{ destination, file }));
}
fn copyLocalTree(source: []const u8, destination: []const u8, prefix: []const u8, depth: usize, count: *usize, total: *u64) !void {
    if (depth > 32) return error.LocalSourceTooLarge;
    var dir = try std.fs.cwd().openDir(try std.fs.path.join(u.a, &.{ source, prefix }), .{ .iterate = true, .no_follow = true });
    defer dir.close();
    var it = dir.iterate();
    while (try it.next()) |entry| {
        const relative = try std.fs.path.join(u.a, &.{ prefix, entry.name });
        if (excludedLocal(relative)) continue;
        count.* += 1;
        if (count.* > 1024) return error.LocalSourceTooLarge;
        if (!@import("query.zig").validFile(relative)) return error.UnsafeBuildFile;
        const input = try std.fs.path.join(u.a, &.{ source, relative });
        const output = try std.fs.path.join(u.a, &.{ destination, relative });
        switch (entry.kind) {
            .directory => {
                try std.fs.cwd().makePath(output);
                try copyLocalTree(source, destination, relative, depth + 1, count, total);
            },
            .file => {
                // Open without following symlinks and retain the descriptor while copying.
                const fd = c.open((try u.z(input)).ptr, @as(c_int, c.O_RDONLY | c.O_CLOEXEC | c.O_NOFOLLOW | c.O_NONBLOCK));
                if (fd < 0) return error.UnsafeBuildFile;
                const file: std.fs.File = .{ .handle = fd };
                defer file.close();
                const st = try file.stat();
                if (st.kind != .file) return error.UnsafeBuildFile;
                const bytes = try file.readToEndAlloc(u.a, 16 * 1024 * 1024);
                defer u.a.free(bytes);
                total.* += bytes.len;
                if (total.* > 64 * 1024 * 1024) return error.LocalSourceTooLarge;
                const out = try std.fs.cwd().createFile(output, .{ .exclusive = true, .mode = 0o600 | (st.mode & 0o111) });
                defer out.close();
                try out.writeAll(bytes);
            },
            else => return error.UnsafeBuildFile,
        }
    }
}
fn excludedLocal(path: []const u8) bool {
    var parts = std.mem.splitScalar(u8, path, '/');
    const first = parts.next() orelse return false;
    if (std.mem.eql(u8, first, "src") or std.mem.eql(u8, first, "pkg")) return true;
    var all = std.mem.splitScalar(u8, path, '/');
    while (all.next()) |part| if (std.mem.eql(u8, part, ".git")) return true;
    return std.mem.indexOf(u8, first, ".pkg.tar") != null;
}
test "local source snapshots keep patches and reject symlinks" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    try tmp.dir.makePath("original/src");
    try tmp.dir.makePath("original/patches");
    try tmp.dir.makeDir("copy");
    try tmp.dir.writeFile(.{ .sub_path = "original/PKGBUILD", .data = "pkgname=fixture" });
    try tmp.dir.writeFile(.{ .sub_path = "original/.SRCINFO", .data = "pkgbase = fixture" });
    try tmp.dir.writeFile(.{ .sub_path = "original/patches/fix.patch", .data = "patch" });
    try tmp.dir.writeFile(.{ .sub_path = "original/src/ignored", .data = "output" });
    const source = try std.fs.path.join(u.a, &.{ root, "original" });
    const dest = try std.fs.path.join(u.a, &.{ root, "copy" });
    try snapshotLocal(source, dest);
    try tmp.dir.access("copy/patches/fix.patch", .{});
    try std.testing.expectError(error.FileNotFound, tmp.dir.access("copy/src/ignored", .{}));
    try tmp.dir.symLink("PKGBUILD", "original/alias", .{});
    try tmp.dir.makeDir("copy2");
    try std.testing.expectError(error.UnsafeBuildFile, snapshotLocal(source, try std.fs.path.join(u.a, &.{ root, "copy2" })));
}
test "local package build runs every makepkg phase in the sandbox" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    const cache = try std.fs.path.join(u.a, &.{ root, "cache" });
    const source = try std.fs.path.join(u.a, &.{ root, "local" });
    try tmp.dir.makeDir("cache");
    try tmp.dir.makeDir("local");
    try tmp.dir.writeFile(.{ .sub_path = "private", .data = "secret" });
    const pkgbuild = try std.fmt.allocPrint(u.a, "pkgname=zap-isolated-fixture\npkgver=1\npkgrel=1\narch=('any')\nlicense=('MIT')\n" ++
        "test ! -e '{s}/private' || exit 1\nif touch .git/injected 2>/dev/null; then exit 1; fi\npackage() {{ mkdir -p \"$pkgdir/usr/share/zap-isolated-fixture\"; echo isolated > \"$pkgdir/usr/share/zap-isolated-fixture/result\"; }}\n", .{root});
    try tmp.dir.writeFile(.{ .sub_path = "local/PKGBUILD", .data = pkgbuild });
    try tmp.dir.writeFile(.{ .sub_path = "local/.SRCINFO", .data = "pkgbase = zap-isolated-fixture\npkgver = 1\npkgrel = 1\narch = any\npkgname = zap-isolated-fixture\n" });
    var db: alpm.Alpm = .{ .h = try @import("native_tests.zig").handle(root), .cfg = .{ .db = try std.fs.path.join(u.a, &.{ root, "db" }) } };
    defer db.deinit();
    var builds: Builder = .{ .db = &db, .root = cache, .lock = -1, .requested = &.{}, .sandbox = true };
    try builds.loadLocal(&.{source});
    // No host queries/installations are needed for this dependency-free fixture.
    try builds.planLocal(0, 0);
    const pipe = try std.posix.pipe();
    defer std.posix.close(pipe[0]);
    defer std.posix.close(pipe[1]);
    const stdin = try std.posix.dup(0);
    defer std.posix.close(stdin);
    try std.posix.dup2(pipe[0], 0);
    defer std.posix.dup2(stdin, 0) catch {};
    _ = try std.posix.write(pipe[1], "y\n");
    try builds.execute(false);
    try std.testing.expect(try db.local("zap-isolated-fixture") == null);
    // The original checkout was never sourced or rewritten by the build.
    try std.testing.expectEqualStrings(pkgbuild, try tmp.dir.readFileAlloc(u.a, "local/PKGBUILD", 4096));
}

test "local dependency planning orders supplied bases and detects cycles" {
    var tmp = std.testing.tmpDir(.{});
    defer tmp.cleanup();
    const root = try tmp.dir.realpathAlloc(u.a, ".");
    var db: alpm.Alpm = .{ .h = try @import("native_tests.zig").handle(root), .cfg = .{} };
    defer db.deinit();
    var builds: Builder = .{ .db = &db, .root = root, .lock = -1, .requested = &.{} };
    const a = try srcinfo.Metadata.parse("pkgbase = first\npkgver = 1\npkgrel = 1\npkgname = first\ndepends = second>=1\n", "any");
    const b = try srcinfo.Metadata.parse("pkgbase = second\npkgver = 1\npkgrel = 1\npkgname = second\n", "any");
    for ([_]srcinfo.Metadata{ a, b }) |meta| try builds.nodes.append(u.a, .{
        .package = .{ .Name = meta.base, .PackageBase = meta.base, .Version = meta.version },
        .dir = root,
        .metadata = meta,
        .visiting = false,
        .local_source = true,
    });
    try builds.planLocal(0, 0);
    try std.testing.expectEqualSlices(usize, &.{ 1, 0 }, builds.order.items);
    builds.order.clearRetainingCapacity();
    for (builds.nodes.items) |*node| {
        node.planned = false;
        node.visiting = false;
    }
    try builds.nodes.items[1].metadata.deps.append(u.a, "first");
    try std.testing.expectError(error.DependencyCycle, builds.planLocal(0, 0));
}
