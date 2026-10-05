const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const tx = @import("transaction.zig");
const srcinfo = @import("srcinfo.zig");
const artifacts = @import("artifacts.zig");
const Node = struct { package: aur.Package, dir: []const u8, metadata: srcinfo.Metadata, wanted: std.ArrayList([]const u8) = .empty, visiting: bool = true, reviewed: bool = false, source_digest: ?[]const u8 = null, local_source: bool = false, planned: bool = false };
pub const Builder = struct {
    db: *alpm.Alpm,
    root: []const u8,
    lock: c_int,
    requested: []const []const u8,
    preserve_reasons: bool = false,
    scope: @import("cli.zig").Scope = .all,
    aur_targets: []const []const u8 = &.{},
    needed: bool = false,
    sandbox: bool = true,
    clean_after: bool = false,
    rebuild_tree: bool = false,
    reason: ?@import("cli.zig").Reason = null,
    nodes: std.ArrayList(Node) = .empty,
    order: std.ArrayList(usize) = .empty,
    repos: std.ArrayList(tx.Target) = .empty,
    reason_targets: std.ArrayList(tx.Target) = .empty,
    explicit_names: std.ArrayList([]const u8) = .empty,
    pub fn init(db: *alpm.Alpm, requested: []const []const u8) !Builder {
        if (c.geteuid() == 0) return error.BuildsMustNotRunAsRoot;
        const home = std.posix.getenv("HOME") orelse return error.HomeNotSet;
        const cache = std.posix.getenv("XDG_CACHE_HOME") orelse try std.fs.path.join(u.a, &.{ home, ".cache" });
        if (!std.fs.path.isAbsolute(cache)) return error.InvalidCacheDirectory;
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
    pub fn plan(self: *Builder) !void {
        const local_count = self.nodes.items.len;
        for (0..local_count) |index| if (self.nodes.items[index].local_source) {
            try self.planLocal(index, 0);
        };
        for (self.requested) |name| try self.resolve(name, true, 0);
        if (self.nodes.items.len > 0) try self.resolve("base-devel", false, 0);
        ui.title("Installation plan");
        ui.text(try std.fmt.allocPrint(u.a, "{d} repo · {d} build bases · {d} reason changes", .{ self.repos.items.len, self.order.items.len, self.reason_targets.items.len }), 2, .muted);
        for (self.repos.items) |target| ui.packageHeader(target.name, "", "repo", null, if (target.dependency) "[dependency]" else "[requested]");
        for (self.reason_targets.items) |target| ui.text(try std.fmt.allocPrint(u.a, "{s} → {s}", .{ target.name, @tagName(target.reason.?) }), 2, .muted);
        for (self.order.items) |idx| {
            const node = self.nodes.items[idx];
            ui.print("\n", .{});
            ui.package(node.package.PackageBase, node.package.Version, if (node.local_source) "local" else "aur", if (node.local_source) "" else "Last modified", node.package.LastModified, "", null);
            for (node.wanted.items) |wanted| ui.text(try std.fmt.allocPrint(u.a, "{s} · {s}", .{ wanted, if (srcinfo.contains(self.explicit_names.items, wanted)) @as([]const u8, "requested") else "dependency" }), 4, .muted);
        }
        ui.print("\n", .{});
        if (self.repos.items.len == 0 and self.order.items.len == 0 and self.reason_targets.items.len == 0) ui.note(.success, "No changes needed.");
    }
    fn skipInstalled(self: *Builder, old: alpm.Pkg) !void {
        ui.print("Already installed: {s} {s}\n", .{ ui.safe(alpm.name(old)), ui.safe(alpm.version(old)) });
        const reason = self.reason orelse .explicit;
        const desired: c.alpm_pkgreason_t = if (reason == .explicit) c.ALPM_PKG_REASON_EXPLICIT else c.ALPM_PKG_REASON_DEPEND;
        if (c.alpm_pkg_get_reason(old) != desired) try self.reason_targets.append(u.a, .{ .name = alpm.name(old), .reason = reason });
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
    fn resolve(self: *Builder, dep: []const u8, explicit: bool, depth: usize) anyerror!void {
        if (depth > 128 or self.nodes.items.len > 512) return error.DependencyGraphTooLarge;
        if (!u.validName(u.depName(dep))) return error.InvalidDependency;
        // Explicit local snapshots take precedence over installed/repository satisfiers.
        for (self.nodes.items, 0..) |node, idx| if (node.local_source) {
            for (node.metadata.names.items) |name| if (try node.metadata.matches(dep, name)) {
                try self.planLocal(idx, depth + 1);
                if (explicit) try srcinfo.appendUnique(&self.explicit_names, name);
                return;
            };
        };
        if (!explicit and try self.db.local(dep) != null) {
            // Rebuild installed foreign dependencies only; repository packages stay satisfied.
            const installed = (try self.db.local(dep)).?;
            if (!self.rebuild_tree or !try self.db.foreign(installed)) return;
        }
        if (!(explicit and (self.scope == .aur or srcinfo.contains(self.aur_targets, u.depName(dep))))) if (try self.db.repo(dep)) |p| {
            if (explicit) try srcinfo.appendUnique(&self.explicit_names, alpm.name(p));
            if (explicit and self.needed) if (c.alpm_db_get_pkg(c.alpm_get_localdb(self.db.h), c.alpm_pkg_get_name(p))) |old| {
                if (c.alpm_pkg_vercmp(c.alpm_pkg_get_version(old), c.alpm_pkg_get_version(p)) >= 0) {
                    try self.skipInstalled(old);
                    return;
                }
            };
            try self.addRepo(alpm.name(p), explicit);
            return;
        };
        if (explicit and self.scope == .repo) return error.RepositoryPackageNotFound;
        // A previously selected package/provider can satisfy the dependency.
        for (self.nodes.items, 0..) |node, idx| {
            for (node.metadata.names.items) |n| if (try node.metadata.matches(dep, n)) {
                if (node.local_source and !node.planned) try self.planLocal(idx, depth + 1);
                if (self.nodes.items[idx].visiting) return error.DependencyCycle;
                if (explicit) try srcinfo.appendUnique(&self.explicit_names, n);
                try srcinfo.appendUnique(&self.nodes.items[idx].wanted, n);
                return;
            };
        }
        const exact = try aur.info(&.{u.depName(dep)});
        var selected: ?aur.Package = null;
        if (exact.len > 0 and try srcinfo.satisfies(dep, exact[0].Name, exact[0].Version, exact[0].Provides)) selected = exact[0];
        if (selected == null) {
            const candidates = try aur.search(u.depName(dep), "provides");
            var matches: std.ArrayList(aur.Package) = .empty;
            for (candidates) |candidate| {
                const detail = try aur.info(&.{candidate.Name});
                if (detail.len > 0 and try srcinfo.satisfies(dep, detail[0].Name, detail[0].Version, detail[0].Provides)) try matches.append(u.a, detail[0]);
            }
            if (matches.items.len == 0) {
                ui.print("No provider satisfies {s}\n", .{ui.safe(dep)});
                return error.UnsatisfiedDependency;
            }
            if (matches.items.len == 1) selected = matches.items[0] else {
                ui.print("AUR providers for {s}:\n", .{ui.safe(dep)});
                for (matches.items, 0..) |p, i| ui.print("  {d}. {s} {s}\n", .{ i + 1, ui.safe(p.Name), ui.safe(p.Version) });
                const reply = try ui.answer("Select provider: ");
                const index = try std.fmt.parseInt(usize, reply, 10);
                if (index == 0 or index > matches.items.len) return error.InvalidProvider;
                selected = matches.items[index - 1];
            }
        }
        const p = selected.?;
        if (explicit and self.needed) if (c.alpm_db_get_pkg(c.alpm_get_localdb(self.db.h), (try u.z(p.Name)).ptr)) |old| {
            if (c.alpm_pkg_vercmp(c.alpm_pkg_get_version(old), (try u.z(p.Version)).ptr) >= 0) {
                try self.skipInstalled(old);
                return;
            }
        };
        if (explicit) try srcinfo.appendUnique(&self.explicit_names, p.Name);
        for (self.nodes.items, 0..) |node, idx| if (std.mem.eql(u8, node.package.PackageBase, p.PackageBase)) {
            if (node.visiting) return error.DependencyCycle;
            try srcinfo.appendUnique(&self.nodes.items[idx].wanted, p.Name);
            return;
        };
        const dir = try self.checkout(p.PackageBase);
        const meta = try srcinfo.Metadata.parse(try u.readFile(try std.fs.path.join(u.a, &.{ dir, ".SRCINFO" }), 2 * 1024 * 1024), self.db.cfg.arch);
        if (!std.mem.eql(u8, meta.base, p.PackageBase) or !srcinfo.contains(meta.names.items, p.Name)) return error.AurMetadataMismatch;
        if (!try meta.matches(dep, p.Name)) return error.AurMetadataMismatch;
        const idx = self.nodes.items.len;
        var node: Node = .{ .package = p, .dir = dir, .metadata = meta };
        try node.wanted.append(u.a, p.Name);
        try self.nodes.append(u.a, node);
        for (meta.deps.items) |dependency| {
            var sibling = false;
            for (meta.names.items) |n| if (try meta.matches(dependency, n)) {
                sibling = true;
                try srcinfo.appendUnique(&self.nodes.items[idx].wanted, n);
                break;
            };
            if (!sibling) try self.resolve(dependency, false, depth + 1);
        }
        self.nodes.items[idx].visiting = false;
        self.nodes.items[idx].planned = true;
        try self.order.append(u.a, idx);
    }
    pub fn loadLocal(self: *Builder, dirs: []const []const u8) !void {
        for (dirs) |input| {
            const source = try std.fs.cwd().realpathAlloc(u.a, input);
            const template = try u.a.dupeZ(u8, try std.fs.path.join(u.a, &.{ self.root, ".local-XXXXXX" }));
            if (c.mkdtemp(template.ptr) == null) return error.StagingFailed;
            const destination = std.mem.sliceTo(template, 0);
            errdefer std.fs.cwd().deleteTree(destination) catch {};
            try snapshotLocal(source, destination);
            const meta = try srcinfo.Metadata.parse(try u.readFile(try std.fs.path.join(u.a, &.{ destination, ".SRCINFO" }), 2 * 1024 * 1024), self.db.cfg.arch);
            for (self.nodes.items) |node| if (std.mem.eql(u8, node.metadata.base, meta.base)) return error.DuplicateTarget;
            for (meta.names.items) |name| {
                if (srcinfo.contains(self.explicit_names.items, name)) return error.DuplicateTarget;
                try srcinfo.appendUnique(&self.explicit_names, name);
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
            ui.print("Local sources copied to {s}\n", .{ui.safe(destination)});
        }
    }
    fn planLocal(self: *Builder, idx: usize, depth: usize) anyerror!void {
        if (depth > 128) return error.DependencyGraphTooLarge;
        if (self.nodes.items[idx].planned) return;
        if (self.nodes.items[idx].visiting) return error.DependencyCycle;
        self.nodes.items[idx].visiting = true;
        const meta = self.nodes.items[idx].metadata;
        for (meta.deps.items) |dep| {
            var sibling = false;
            for (meta.names.items) |name| if (try meta.matches(dep, name)) {
                sibling = true;
                break;
            };
            if (!sibling) try self.resolve(dep, false, depth + 1);
        }
        self.nodes.items[idx].visiting = false;
        self.nodes.items[idx].planned = true;
        try self.order.append(u.a, idx);
    }
    fn git(args: []const []const u8, dir: ?[]const u8) ![]u8 {
        var argv: std.ArrayList([]const u8) = .empty;
        try argv.appendSlice(u.a, &.{ "/usr/bin/git", "-c", "core.hooksPath=/dev/null", "-c", "protocol.file.allow=never", "-c", "core.fsmonitor=false" });
        try argv.appendSlice(u.a, args);
        return u.capture(argv.items, dir, 16 * 1024 * 1024);
    }
    fn checkout(self: *Builder, base: []const u8) ![]const u8 {
        const dir = try std.fs.path.join(u.a, &.{ self.root, base });
        var st: c.struct_stat = undefined;
        if (c.lstat((try u.z(dir)).ptr, &st) != 0) {
            _ = try git(&.{ "clone", "--", try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/{s}.git", .{base}), dir }, null);
            try u.secureDir(dir);
        } else {
            if (st.st_uid != c.getuid() or st.st_mode & c.S_IFMT != c.S_IFDIR) return error.UnsafeCacheDirectory;
            const status = try git(&.{ "status", "--porcelain", "--untracked-files=no" }, dir);
            if (status.len > 0) {
                ui.print("Local edits in {s}; review and commit or remove them before continuing.\n", .{ui.safe(dir)});
                return error.ModifiedBuildFiles;
            }
            _ = try git(&.{ "fetch", "--", try std.fmt.allocPrint(u.a, "https://aur.archlinux.org/{s}.git", .{base}), "master" }, dir);
            const diff = try git(&.{ "diff", "--no-ext-diff", "--no-textconv", "HEAD", "FETCH_HEAD", "--" }, dir);
            if (diff.len > 0) {
                ui.print("\nChanges for {s}:\n", .{ui.safe(base)});
                printSource(diff);
            }
            _ = try git(&.{ "merge", "--ff-only", "FETCH_HEAD" }, dir);
        }
        // Refuse symlinked metadata before any PKGBUILD code is executed.
        for ([_][]const u8{ "PKGBUILD", ".SRCINFO" }) |file| {
            const path = try std.fs.path.join(u.a, &.{ dir, file });
            if (c.lstat((try u.z(path)).ptr, &st) != 0 or st.st_mode & c.S_IFMT != c.S_IFREG or st.st_uid != c.getuid()) return error.UnsafeBuildFile;
        }
        return dir;
    }
    pub fn getSources(self: *Builder) !void {
        for (self.requested) |name| {
            const packages = try aur.info(&.{name});
            if (packages.len == 0) return error.AurPackageNotFound;
            const p = packages[0];
            ui.print("{s}: {s} · modified {s}\n", .{ ui.safe(name), ui.safe(try self.checkout(p.PackageBase)), ui.date(p.LastModified) });
        }
    }
    pub fn printPkgbuilds(self: *Builder) !void {
        for (self.requested) |name| {
            const packages = try aur.info(&.{name});
            if (packages.len == 0) return error.AurPackageNotFound;
            const dir = try self.checkout(packages[0].PackageBase);
            printSource(try u.readFile(try std.fs.path.join(u.a, &.{ dir, "PKGBUILD" }), 16 * 1024 * 1024));
        }
    }
    fn makepkgArgs(self: *Builder, dir: []const u8, args: []const []const u8) ![]const []const u8 {
        return if (self.sandbox) @import("sandbox.zig").argv(dir, self.db.cfg.db, args) else args;
    }
    pub fn execute(self: *Builder, install: bool) !void {
        if (install) try tx.trustedExecutable() else {
            if (self.nodes.items.len == 0) return error.BuildRequiresAurPackage;
            for (self.repos.items) |target| {
                if (try self.db.local(target.name) == null) {
                    ui.print("Missing installed build dependency: {s}\n", .{ui.safe(target.name)});
                    return error.MissingBuildDependencies;
                }
                if (!target.dependency) return error.BuildRequiresAurPackage;
            }
            for (self.nodes.items) |node| for (node.metadata.deps.items) |dep| {
                var sibling = false;
                for (node.metadata.names.items) |name| if (try node.metadata.matches(dep, name)) {
                    sibling = true;
                    break;
                };
                if (!sibling and try self.db.local(dep) == null) {
                    ui.print("Missing installed build dependency: {s}\n", .{ui.safe(dep)});
                    return error.MissingBuildDependencies;
                }
            };
        }
        if (self.sandbox and self.order.items.len > 0) {
            // Probe before source approval or dependency installation. Failure never falls back.
            const probe = self.makepkgArgs(self.nodes.items[self.order.items[0]].dir, &.{"/usr/bin/true"}) catch return error.BuildSandboxUnavailable;
            u.run(probe, null) catch return error.BuildSandboxUnavailable;
            ui.print("Build isolation: private home, read-only system and database; source downloads can access the network.\n", .{});
        }
        // Review every package base before executing any PKGBUILD code or changing the system.
        for (self.order.items, 1..) |idx, step| {
            const node = &self.nodes.items[idx];
            ui.title(try std.fmt.allocPrint(u.a, "Review {d}/{d} · {s}", .{ step, self.order.items.len, node.package.PackageBase }));
            if (node.local_source) ui.field("Source", "Local snapshot") else {
                ui.field("Last modified", ui.date(node.package.LastModified));
                ui.field("Maintainer", node.package.Maintainer orelse "unmaintained");
            }
            if (node.package.OutOfDate != null) ui.note(.warning, "Flagged out of date");
            node.source_digest = try sourceDigest(node.dir, true);
            const files = try git(&.{ "ls-files", "-z" }, node.dir);
            var paths = std.mem.splitScalar(u8, files, 0);
            while (paths.next()) |path| {
                if (path.len == 0) continue;
                const content = try git(&.{ "show", try std.fmt.allocPrint(u.a, "HEAD:{s}", .{path}) }, node.dir);
                // Preserve newlines for source review while filtering terminal control sequences.
                ui.title(path);
                printSource(content);
            }
            try ui.require("\nThese build files can execute arbitrary code as your user. Approve this source? [y/N] ");
            try verifySource(node.*);
            node.reviewed = true;
        }
        for (self.order.items) |idx| {
            const node = &self.nodes.items[idx];
            try verifySource(node.*);
            const generated = try u.capture(try self.makepkgArgs(node.dir, &.{ "/usr/bin/makepkg", "--printsrcinfo" }), node.dir, 2 * 1024 * 1024);
            const actual = try srcinfo.Metadata.parse(generated, self.db.cfg.arch);
            if (!node.metadata.equivalent(actual)) {
                ui.print("PKGBUILD metadata differs from .SRCINFO. Resolve the discrepancy before building.\n", .{});
                return error.StaleSrcInfo;
            }
            try verifySource(node.*);
        }
        if (install and self.reason_targets.items.len > 0) try tx.escalate(.{ .operation = .reason, .targets = self.reason_targets.items });
        if (install and self.repos.items.len > 0) {
            try tx.escalate(.{ .operation = .install, .targets = self.repos.items, .needed = self.needed });
            try self.reload();
        }
        for (self.order.items, 1..) |idx, step| {
            const node = self.nodes.items[idx];
            if (!node.reviewed) return error.SourceNotReviewed;
            try verifySource(node);
            ui.title(try std.fmt.allocPrint(u.a, "Build {d}/{d} · {s}", .{ step, self.order.items.len, node.package.PackageBase }));
            try u.run(try self.makepkgArgs(node.dir, &.{ "/usr/bin/makepkg", "--cleanbuild", "--clean", "--force", "--noconfirm" }), node.dir);
            try verifySource(node);
            const paths = try u.capture(try self.makepkgArgs(node.dir, &.{ "/usr/bin/makepkg", "--packagelist" }), node.dir, 2 * 1024 * 1024);
            const built = try artifacts.collect(self.db, .{
                .base = node.package.PackageBase,
                .dir = node.dir,
                .wanted = node.wanted.items,
                .explicit_names = self.explicit_names.items,
                .preserve_reasons = self.preserve_reasons,
            }, paths);
            defer built.deinit();
            try verifySource(node);
            for (built.targets, built.versions) |target, version| {
                ui.text(try std.fmt.allocPrint(u.a, "{s} {s}", .{ target.name, version }), 2, .bold);
                ui.field("Archive", target.archive.?);
                ui.field("SHA-256", target.sha256.?);
                ui.print("\n", .{});
            }
            for (built.targets) |*target| if (srcinfo.contains(self.explicit_names.items, target.name)) {
                target.reason = self.reason;
            };
            if (install) {
                try tx.escalate(.{ .operation = .install, .targets = built.targets, .needed = self.needed });
                try self.reload();
            }
            // makepkg may rewrite a literal pkgver after a VCS build. Only that
            // validated change is restored; other tracked edits fail verification.
            _ = try git(&.{ "restore", "--source=HEAD", "--", "PKGBUILD" }, node.dir);
            if (self.clean_after and install) _ = try git(&.{ "clean", "-fdx", "--" }, node.dir);
        }
    }
    fn sourceDigest(dir: []const u8, strict: bool) ![]const u8 {
        const status = try git(&.{ "status", "--porcelain", "--untracked-files=no" }, dir);
        if (strict and status.len > 0) return error.ModifiedBuildFiles;
        var hash = std.crypto.hash.sha2.Sha256.init(.{});
        const files = try git(&.{ "ls-files", "-z" }, dir);
        hash.update(files);
        var paths = std.mem.splitScalar(u8, files, 0);
        while (paths.next()) |path| {
            if (path.len == 0) continue;
            if (std.fs.path.isAbsolute(path) or std.mem.indexOf(u8, path, "../") != null) return error.UnsafeBuildFile;
            const file = try std.fs.path.join(u.a, &.{ dir, path });
            const digest = try tx.hashSourceFile(file);
            if (std.mem.eql(u8, path, "PKGBUILD")) hash.update(try normalizePkgver(try u.readFile(file, 16 * 1024 * 1024))) else hash.update(digest);
        }
        const digest = hash.finalResult();
        return std.fmt.allocPrint(u.a, "{s}", .{std.fmt.bytesToHex(digest, .lower)});
    }
    fn verifySource(node: Node) !void {
        if (!std.mem.eql(u8, node.source_digest orelse return error.SourceNotReviewed, try sourceDigest(node.dir, false))) return error.BuildSourceChanged;
    }
    fn reload(self: *Builder) !void {
        const refreshed = try alpm.Alpm.init();
        self.db.deinit();
        self.db.* = refreshed;
    }
    pub fn clean(self: *Builder) !void {
        if (self.requested.len > 0) {
            for (self.requested) |base| {
                if (!u.validName(base)) return error.InvalidPackageName;
                ui.print("Clear cached base {s}\n", .{ui.safe(base)});
            }
            try ui.require("Remove these cached sources and build outputs? [y/N] ");
            var dir = try std.fs.cwd().openDir(self.root, .{ .iterate = true });
            defer dir.close();
            for (self.requested) |base| try dir.deleteTree(base);
            return;
        }
        ui.print("Remove all cached AUR sources and built packages from {s}?\n", .{ui.safe(self.root)});
        try ui.require("Clear build cache? [y/N] ");
        var dir = try std.fs.cwd().openDir(self.root, .{ .iterate = true });
        defer dir.close();
        var it = dir.iterate();
        while (try it.next()) |entry| {
            if (std.mem.eql(u8, entry.name, ".lock")) continue;
            if (entry.kind == .directory) try dir.deleteTree(entry.name) else try dir.deleteFile(entry.name);
        }
        ui.note(.success, "Build cache cleared.");
    }
};

fn printSource(content: []const u8) void {
    var lines = std.mem.splitScalar(u8, content, '\n');
    while (lines.next()) |line| ui.print("{s}\n", .{ui.safe(line)});
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
