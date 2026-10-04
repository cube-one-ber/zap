const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const alpm = @import("alpm.zig");
const aur = @import("aur.zig");
const tx = @import("transaction.zig");
const srcinfo = @import("srcinfo.zig");
const Node = struct { package: aur.Package, dir: []const u8, metadata: srcinfo.Metadata, wanted: std.ArrayList([]const u8) = .empty, visiting: bool = true, reviewed: bool = false, source_digest: ?[]const u8 = null };
pub const Builder = struct {
    db: *alpm.Alpm,
    root: []const u8,
    lock: c_int,
    requested: []const []const u8,
    preserve_reasons: bool = false,
    nodes: std.ArrayList(Node) = .empty,
    order: std.ArrayList(usize) = .empty,
    repos: std.ArrayList(tx.Target) = .empty,
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
        for (self.requested) |name| try self.resolve(name, true, 0);
        if (self.nodes.items.len > 0) try self.resolve("base-devel", false, 0);
        ui.title("Installation plan");
        for (self.repos.items) |target| ui.print("  repo  {s}{s}\n", .{ ui.safe(target.name), if (target.dependency) " (dependency)" else "" });
        for (self.order.items) |idx| {
            const node = self.nodes.items[idx];
            ui.print("  AUR   {s} {s} · modified {s}\n", .{ ui.safe(node.package.PackageBase), ui.safe(node.package.Version), ui.date(node.package.LastModified) });
            for (node.wanted.items) |wanted| ui.print("        {s}{s}\n", .{ ui.safe(wanted), if (srcinfo.contains(self.explicit_names.items, wanted)) " (requested)" else " (dependency)" });
        }
    }
    fn addRepo(self: *Builder, name: []const u8, explicit: bool) !void {
        for (self.repos.items) |*target| if (std.mem.eql(u8, target.name, name)) {
            if (explicit) target.dependency = false;
            return;
        };
        try self.repos.append(u.a, .{ .name = name, .dependency = !explicit });
    }
    fn resolve(self: *Builder, dep: []const u8, explicit: bool, depth: usize) anyerror!void {
        if (depth > 128 or self.nodes.items.len > 512) return error.DependencyGraphTooLarge;
        if (!u.validName(u.depName(dep))) return error.InvalidDependency;
        if (!explicit and try self.db.local(dep) != null) return;
        if (try self.db.repo(dep)) |p| {
            if (explicit) try srcinfo.appendUnique(&self.explicit_names, alpm.name(p));
            try self.addRepo(alpm.name(p), explicit);
            return;
        }
        // A previously selected package/provider can satisfy the dependency.
        for (self.nodes.items, 0..) |node, idx| {
            for (node.metadata.names.items) |n| if (try node.metadata.matches(dep, n)) {
                if (node.visiting) return error.DependencyCycle;
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
        // Review every package base before executing any PKGBUILD code or changing the system.
        for (self.order.items) |idx| {
            const node = &self.nodes.items[idx];
            ui.title(node.package.PackageBase);
            ui.print("AUR modified {s} · maintainer {s}{s}\n", .{ ui.date(node.package.LastModified), ui.safe(node.package.Maintainer orelse "orphaned"), if (node.package.OutOfDate != null) " · flagged out of date" else "" });
            node.source_digest = try sourceDigest(node.dir, true);
            const files = try git(&.{ "ls-files", "-z" }, node.dir);
            var paths = std.mem.splitScalar(u8, files, 0);
            while (paths.next()) |path| {
                if (path.len == 0) continue;
                const content = try git(&.{ "show", try std.fmt.allocPrint(u.a, "HEAD:{s}", .{path}) }, node.dir);
                // Preserve newlines for source review while filtering terminal control sequences.
                ui.print("\n── {s} ──\n", .{ui.safe(path)});
                printSource(content);
            }
            try ui.require("\nThese build files can execute arbitrary code as your user. Approve this source? [y/N] ");
            try verifySource(node.*);
            node.reviewed = true;
        }
        for (self.order.items) |idx| {
            const node = &self.nodes.items[idx];
            try verifySource(node.*);
            const generated = try u.capture(&.{ "/usr/bin/makepkg", "--printsrcinfo" }, node.dir, 2 * 1024 * 1024);
            const actual = try srcinfo.Metadata.parse(generated, self.db.cfg.arch);
            if (!node.metadata.equivalent(actual)) {
                ui.print("PKGBUILD metadata differs from .SRCINFO. Resolve the discrepancy before building.\n", .{});
                return error.StaleSrcInfo;
            }
            try verifySource(node.*);
        }
        if (install and self.repos.items.len > 0) {
            try tx.escalate(.{ .operation = .install, .targets = self.repos.items });
            try self.reload();
        }
        for (self.order.items) |idx| {
            const node = self.nodes.items[idx];
            if (!node.reviewed) return error.SourceNotReviewed;
            try verifySource(node);
            ui.title(try std.fmt.allocPrint(u.a, "Build {s}", .{node.package.PackageBase}));
            try u.run(&.{ "/usr/bin/makepkg", "--cleanbuild", "--clean", "--force", "--noconfirm" }, node.dir);
            try verifySource(node);
            const paths = try u.capture(&.{ "/usr/bin/makepkg", "--packagelist" }, node.dir, 2 * 1024 * 1024);
            var artifacts: std.ArrayList(tx.Target) = .empty;
            var lines = std.mem.splitScalar(u8, paths, '\n');
            while (lines.next()) |line| {
                if (line.len == 0) continue;
                const path = if (std.fs.path.isAbsolute(line)) line else try std.fs.path.join(u.a, &.{ node.dir, line });
                const digest = try tx.hashFile(path);
                var p: ?*c.alpm_pkg_t = null;
                try self.db.check(c.alpm_pkg_load(self.db.h, (try u.z(path)).ptr, 1, 0, &p));
                defer _ = c.alpm_pkg_free(p);
                const name = alpm.name(p.?);
                if (!srcinfo.contains(node.wanted.items, name)) continue;
                try artifacts.append(u.a, .{ .name = try u.a.dupe(u8, name), .archive = path, .sha256 = digest, .dependency = if (self.preserve_reasons) (if (try self.db.local(name)) |old| c.alpm_pkg_get_reason(old) == c.ALPM_PKG_REASON_DEPEND else !srcinfo.contains(self.explicit_names.items, name)) else !srcinfo.contains(self.explicit_names.items, name) });
                ui.print("  {s} {s}\n  Archive {s}\n  SHA-256 {s}\n", .{ ui.safe(name), ui.safe(alpm.version(p.?)), ui.safe(path), digest });
            }
            try verifySource(node);
            if (artifacts.items.len != node.wanted.items.len) return error.MissingBuildArtifacts;
            if (install) {
                try tx.escalate(.{ .operation = .install, .targets = artifacts.items });
                try self.reload();
            }
            // makepkg may rewrite a literal pkgver after a VCS build. Only that
            // validated change is restored; other tracked edits fail verification.
            _ = try git(&.{ "restore", "--source=HEAD", "--", "PKGBUILD" }, node.dir);
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
        ui.print("Remove all cached AUR sources and built packages from {s}?\n", .{ui.safe(self.root)});
        try ui.require("Clear build cache? [y/N] ");
        var dir = try std.fs.cwd().openDir(self.root, .{ .iterate = true });
        defer dir.close();
        var it = dir.iterate();
        while (try it.next()) |entry| {
            if (std.mem.eql(u8, entry.name, ".lock")) continue;
            if (entry.kind == .directory) try dir.deleteTree(entry.name) else try dir.deleteFile(entry.name);
        }
        ui.print("Build cache cleared.\n", .{});
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
    var builds: Builder = .{ .db = &db, .root = root, .lock = -1, .requested = &.{"zap-build-fixture"} };
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
    const path = std.mem.trim(u8, paths, "\n\r");
    const hash = try tx.hashFile(path);
    try std.testing.expectEqual(@as(usize, 64), hash.len);
}
