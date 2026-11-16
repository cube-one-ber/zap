//! libalpm: the configured handle, package accessors, and the callbacks that
//! surface libalpm's questions, events and progress.
const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const config = @import("config.zig");
const signals = @import("signals.zig");
const errors = @import("errors.zig");

extern fn zap_set_log_callback(handle: *c.alpm_handle_t) c_int;

export fn zap_log_message(level: c_int, message: [*c]const u8) callconv(.c) void {
    ui.diagnostic(level & c.ALPM_LOG_ERROR != 0, u.str(message));
}

// ── Packages ────────────────────────────────────────────────────────────────

pub const Pkg = *c.alpm_pkg_t;
pub const List = [*c]c.alpm_list_t;

pub fn pkg(data: ?*anyopaque) Pkg {
    return @ptrCast(@alignCast(data.?));
}

pub fn name(p: Pkg) []const u8 {
    return u.str(c.alpm_pkg_get_name(p));
}

pub fn version(p: Pkg) []const u8 {
    return u.str(c.alpm_pkg_get_version(p));
}

pub fn description(p: Pkg) []const u8 {
    return u.str(c.alpm_pkg_get_desc(p));
}

/// The repository a sync package comes from; `local` for installed packages.
pub fn repository(p: Pkg) []const u8 {
    return u.str(c.alpm_db_get_name(c.alpm_pkg_get_db(p)));
}

pub fn explicit(p: Pkg) bool {
    return c.alpm_pkg_get_reason(p) == c.ALPM_PKG_REASON_EXPLICIT;
}

pub fn reason(p: Pkg) []const u8 {
    return if (explicit(p)) "explicit" else "dependency";
}

/// A libalpm list of C strings, borrowed.
pub fn strings(list: List) ![]const []const u8 {
    var values: std.ArrayList([]const u8) = .empty;
    var it = list;
    while (it != null) : (it = it.*.next) try values.append(u.a, u.str(@ptrCast(it.*.data)));
    return values.toOwnedSlice(u.a);
}

/// A libalpm list of dependencies, formatted as `name>=version: description`.
pub fn dependencies(list: List) ![]const []const u8 {
    var values: std.ArrayList([]const u8) = .empty;
    var it = list;
    while (it != null) : (it = it.*.next) {
        const dep: *c.alpm_depend_t = @ptrCast(@alignCast(it.*.data.?));
        const formatted = c.alpm_dep_compute_string(dep);
        defer c.free(formatted);
        try values.append(u.a, try u.a.dupe(u8, u.str(formatted)));
    }
    return values.toOwnedSlice(u.a);
}

/// Free a list whose strings libalpm allocated for the caller.
pub fn freeStrings(list: List) void {
    var it = list;
    while (it != null) : (it = it.*.next) c.free(it.*.data);
    c.alpm_list_free(list);
}

// ── Handle ──────────────────────────────────────────────────────────────────

pub const Alpm = struct {
    h: *c.alpm_handle_t,
    cfg: config.Config,

    /// Open the databases configured in pacman.conf.
    pub fn init() !Alpm {
        const cfg = try config.Config.load();
        var err: c.alpm_errno_t = 0;
        const h = c.alpm_initialize((try u.z(cfg.root)).ptr, (try u.z(cfg.db)).ptr, &err) orelse {
            return errors.with(error.AlpmInitializationFailed, "{s}", .{u.str(c.alpm_strerror(err))});
        };
        errdefer _ = c.alpm_release(h);
        var self: Alpm = .{ .h = h, .cfg = cfg };
        try self.check(zap_set_log_callback(h));
        try self.configure();
        for (cfg.repos.items) |repo_config| try self.register(repo_config);
        try self.check(c.alpm_option_set_questioncb(h, question, null));
        try self.check(c.alpm_option_set_eventcb(h, event, h));
        try self.check(c.alpm_option_set_progresscb(h, progress, h));
        return self;
    }

    fn configure(self: *const Alpm) !void {
        const h = self.h;
        const cfg = self.cfg;
        for (cfg.architectures.items) |arch| try self.check(c.alpm_option_add_architecture(h, (try u.z(arch)).ptr));
        try self.check(c.alpm_option_set_logfile(h, (try u.z(cfg.log)).ptr));
        try self.check(c.alpm_option_set_gpgdir(h, (try u.z(cfg.gpg)).ptr));
        const sig = try config.sigLevel(cfg.signature, 0);
        try self.check(c.alpm_option_set_default_siglevel(h, sig));
        // Local archives inherit the global policy unless configured otherwise.
        const local_sig = if (cfg.local_signature) |tokens| try config.sigLevel(tokens, sig) else sig;
        try self.check(c.alpm_option_set_local_file_siglevel(h, local_sig));
        try self.check(c.alpm_option_set_checkspace(h, @intFromBool(cfg.check_space)));
        try self.check(c.alpm_option_set_parallel_downloads(h, cfg.parallel));
        if (@hasDecl(c, "alpm_option_set_sandboxuser")) {
            if (cfg.sandbox_user) |user| try self.check(c.alpm_option_set_sandboxuser(h, (try u.z(user)).ptr));
        }
        for (cfg.caches.items) |s| try self.check(c.alpm_option_add_cachedir(h, (try u.z(s)).ptr));
        for (cfg.hooks.items) |s| try self.check(c.alpm_option_add_hookdir(h, (try u.z(s)).ptr));
        for (cfg.ignore.items) |s| try self.check(c.alpm_option_add_ignorepkg(h, (try u.z(s)).ptr));
        for (cfg.ignore_groups.items) |s| try self.check(c.alpm_option_add_ignoregroup(h, (try u.z(s)).ptr));
        for (cfg.no_upgrade.items) |s| try self.check(c.alpm_option_add_noupgrade(h, (try u.z(s)).ptr));
        for (cfg.no_extract.items) |s| try self.check(c.alpm_option_add_noextract(h, (try u.z(s)).ptr));
    }

    fn register(self: *const Alpm, repo_config: config.Repo) !void {
        const sig = try config.sigLevel(self.cfg.signature, 0);
        const repo_sig = if (repo_config.signatures) |tokens| try config.sigLevel(tokens, sig) else sig;
        const db = c.alpm_register_syncdb(self.h, (try u.z(repo_config.name)).ptr, repo_sig) orelse {
            return errors.with(error.RepositoryRegistrationFailed, "{s}", .{repo_config.name});
        };
        try self.check(c.alpm_db_set_usage(db, try config.usage(repo_config.usage)));
        for (repo_config.servers.items) |server| {
            const with_repo = try std.mem.replaceOwned(u8, u.a, server, "$repo", repo_config.name);
            const expanded = try std.mem.replaceOwned(u8, u.a, with_repo, "$arch", self.cfg.arch);
            try self.check(c.alpm_db_add_server(db, (try u.z(expanded)).ptr));
        }
    }

    pub fn deinit(self: *Alpm) void {
        _ = c.alpm_release(self.h);
    }

    /// Close and reopen the databases after another process changed them.
    pub fn reload(self: *Alpm) !void {
        const refreshed = try Alpm.init();
        self.deinit();
        self.* = refreshed;
    }

    /// Turn a libalpm status into an error carrying libalpm's explanation.
    pub fn check(self: *const Alpm, result: c_int) !void {
        if (result < 0) return errors.with(error.AlpmOperationFailed, "{s}", .{u.str(c.alpm_strerror(c.alpm_errno(self.h)))});
    }

    pub fn localDb(self: *const Alpm) *c.alpm_db_t {
        return c.alpm_get_localdb(self.h).?;
    }

    /// The installed package with exactly this name.
    pub fn installedPkg(self: *const Alpm, package_name: []const u8) !?Pkg {
        return c.alpm_db_get_pkg(self.localDb(), (try u.z(package_name)).ptr);
    }

    /// An installed package that satisfies a dependency, including providers.
    pub fn local(self: *const Alpm, dep: []const u8) !?Pkg {
        return c.alpm_find_satisfier(c.alpm_db_get_pkgcache(self.localDb()), (try u.z(dep)).ptr);
    }

    /// A repository package that satisfies a dependency, including providers.
    pub fn repo(self: *const Alpm, dep: []const u8) !?Pkg {
        return c.alpm_find_dbs_satisfier(self.h, c.alpm_get_syncdbs(self.h), (try u.z(dep)).ptr);
    }

    pub fn installed(self: *const Alpm) List {
        return c.alpm_db_get_pkgcache(self.localDb());
    }

    /// Whether no configured repository carries a package of this name.
    pub fn foreign(self: *const Alpm, p: Pkg) bool {
        var dbs = c.alpm_get_syncdbs(self.h);
        while (dbs != null) : (dbs = dbs.*.next) {
            const db: *c.alpm_db_t = @ptrCast(@alignCast(dbs.*.data.?));
            if (c.alpm_db_get_pkg(db, c.alpm_pkg_get_name(p)) != null) return false;
        }
        return true;
    }

    /// How current the repository data is: the oldest modification time
    /// among the sync databases, which libalpm sets from the mirror's copy,
    /// or 0 when one cannot be read.
    pub fn syncedAt(self: *const Alpm) i64 {
        var oldest: i64 = 0;
        var dbs = c.alpm_get_syncdbs(self.h);
        while (dbs != null) : (dbs = dbs.*.next) {
            const db: *c.alpm_db_t = @ptrCast(@alignCast(dbs.*.data.?));
            const path = std.fmt.allocPrint(u.a, "{s}/sync/{s}.db", .{ std.mem.trimEnd(u8, self.cfg.db, "/"), u.str(c.alpm_db_get_name(db)) }) catch return 0;
            const st = std.fs.cwd().statFile(path) catch return 0;
            const seconds: i64 = @intCast(@divFloor(st.mtime, std.time.ns_per_s));
            if (oldest == 0 or seconds < oldest) oldest = seconds;
        }
        return oldest;
    }

    /// Installed dependencies that nothing requires or optionally uses.
    pub fn orphans(self: *const Alpm) ![]Pkg {
        var result: std.ArrayList(Pkg) = .empty;
        var it = self.installed();
        while (it != null) : (it = it.*.next) {
            const p = pkg(it.*.data);
            if (explicit(p)) continue;
            const required = c.alpm_pkg_compute_requiredby(p);
            defer freeStrings(required);
            const optional = c.alpm_pkg_compute_optionalfor(p);
            defer freeStrings(optional);
            if (required == null and optional == null) try result.append(u.a, p);
        }
        return result.toOwnedSlice(u.a);
    }

    pub fn refresh(self: *const Alpm) !void {
        ui.heading("Refreshing repository databases", "");
        try self.check(c.alpm_db_update(self.h, c.alpm_get_syncdbs(self.h), 0));
    }

    /// Explain why a transaction could not be prepared or committed, and
    /// release the details libalpm allocated.
    pub fn explain(self: *const Alpm, data: List) void {
        const err = c.alpm_errno(self.h);
        var it = data;
        while (it != null) : (it = it.*.next) {
            switch (err) {
                c.ALPM_ERR_UNSATISFIED_DEPS => {
                    const missing: *c.alpm_depmissing_t = @ptrCast(@alignCast(it.*.data.?));
                    const dep = c.alpm_dep_compute_string(missing.depend);
                    defer c.free(dep);
                    ui.warn("{s} requires {s}, which is not available", .{ u.str(missing.target), u.str(dep) });
                    c.alpm_depmissing_free(missing);
                },
                c.ALPM_ERR_CONFLICTING_DEPS => {
                    const conflict: *c.alpm_conflict_t = @ptrCast(@alignCast(it.*.data.?));
                    ui.warn("{s} conflicts with {s}", .{ conflictName(conflict.package1), conflictName(conflict.package2) });
                    c.alpm_conflict_free(conflict);
                },
                c.ALPM_ERR_FILE_CONFLICTS => {
                    const conflict: *c.alpm_fileconflict_t = @ptrCast(@alignCast(it.*.data.?));
                    ui.warn("{s} would overwrite {s}", .{ u.str(conflict.target), u.str(conflict.file) });
                    c.alpm_fileconflict_free(conflict);
                },
                else => {
                    ui.warn("{s}", .{u.str(@ptrCast(it.*.data.?))});
                    c.free(it.*.data);
                },
            }
        }
        c.alpm_list_free(data);
    }
};

/// libalpm 15 identifies conflicting packages by name; libalpm 16 by package.
fn conflictName(value: anytype) []const u8 {
    return switch (@typeInfo(@TypeOf(value))) {
        .optional => if (value) |p| name(p) else "unknown",
        else => u.str(value),
    };
}

// ── Callbacks ───────────────────────────────────────────────────────────────

/// Every question defaults to "no". Questions without a case here, such as
/// skipping unresolvable targets or deleting corrupted files, stay "no".
fn question(_: ?*anyopaque, q: [*c]c.alpm_question_t) callconv(.c) void {
    q.*.any.answer = 0;
    switch (q.*.type) {
        c.ALPM_QUESTION_INSTALL_IGNOREPKG => {
            ui.notef(.warning, "{s} is listed in IgnorePkg.", .{name(q.*.install_ignorepkg.pkg.?)});
            q.*.install_ignorepkg.install = @intFromBool(ui.confirm("Install it anyway? [y/N]"));
        },
        c.ALPM_QUESTION_REPLACE_PKG => {
            ui.notef(.warning, "{s} replaces {s}.", .{ name(q.*.replace.newpkg.?), name(q.*.replace.oldpkg.?) });
            q.*.replace.replace = @intFromBool(ui.confirm("Approve the replacement? [y/N]"));
        },
        c.ALPM_QUESTION_CONFLICT_PKG => {
            ui.notef(.warning, "{s} conflicts with {s}.", .{ conflictName(q.*.conflict.conflict.*.package1), conflictName(q.*.conflict.conflict.*.package2) });
            q.*.conflict.remove = @intFromBool(ui.confirm("Remove the conflicting installed package? [y/N]"));
        },
        c.ALPM_QUESTION_SELECT_PROVIDER => {
            const dep = c.alpm_dep_compute_string(q.*.select_provider.depend);
            defer c.free(dep);
            ui.heading("Choose a provider", u.str(dep));
            var it = q.*.select_provider.providers;
            var count: usize = 0;
            while (it != null) : (it = it.*.next) {
                count += 1;
                const p = pkg(it.*.data);
                ui.entry(.{ .index = count, .source = repository(p), .name = name(p), .version = version(p) });
            }
            const reply = ui.answer("Provider [1]:") catch {
                q.*.select_provider.use_index = -1;
                return;
            };
            const index = if (reply.len == 0) 1 else std.fmt.parseInt(usize, reply, 10) catch 0;
            q.*.select_provider.use_index = if (index > 0 and index <= count) @intCast(index - 1) else -1;
        },
        c.ALPM_QUESTION_IMPORT_KEY => {
            ui.notef(.warning, "Signing key {s} ({s}) is not in the keyring.", .{ u.str(q.*.import_key.fingerprint), u.str(q.*.import_key.uid) });
            q.*.import_key.import = @intFromBool(ui.confirm("Import and trust this key for package verification? [y/N]"));
        },
        else => {},
    }
}

fn interrupted(ctx: ?*anyopaque) bool {
    if (!signals.cancelled.load(.monotonic)) return false;
    _ = c.alpm_trans_interrupt(@ptrCast(ctx));
    return true;
}

fn event(ctx: ?*anyopaque, e: [*c]c.alpm_event_t) callconv(.c) void {
    if (interrupted(ctx)) return;
    switch (e.*.type) {
        c.ALPM_EVENT_SCRIPTLET_INFO => ui.line(std.mem.trimEnd(u8, u.str(e.*.scriptlet_info.line), " \r\n"), 4, .plain),
        c.ALPM_EVENT_HOOK_RUN_START => ui.notef(.info, "Hook {d}/{d} · {s}", .{ e.*.hook_run.position, e.*.hook_run.total, u.str(e.*.hook_run.desc) }),
        c.ALPM_EVENT_PACNEW_CREATED => ui.notef(.warning, "New configuration saved as {s}.pacnew; merge it when convenient.", .{u.str(e.*.pacnew_created.file)}),
        c.ALPM_EVENT_PACSAVE_CREATED => ui.notef(.warning, "Previous configuration kept as {s}.pacsave.", .{u.str(e.*.pacsave_created.file)}),
        c.ALPM_EVENT_PKG_RETRIEVE_START => ui.heading("Downloading", std.fmt.allocPrint(u.a, "{s} · {s}", .{
            ui.count(e.*.pkg_retrieve.num, "package", "packages"),
            ui.size(@intCast(e.*.pkg_retrieve.total_size)),
        }) catch ""),
        else => {},
    }
}

fn progress(ctx: ?*anyopaque, stage: c.alpm_progress_t, package_name: [*c]const u8, percent: c_int, total: usize, current: usize) callconv(.c) void {
    if (interrupted(ctx)) return;
    const label: []const u8 = switch (stage) {
        c.ALPM_PROGRESS_ADD_START => "Installing",
        c.ALPM_PROGRESS_UPGRADE_START => "Upgrading",
        c.ALPM_PROGRESS_DOWNGRADE_START => "Downgrading",
        c.ALPM_PROGRESS_REINSTALL_START => "Reinstalling",
        c.ALPM_PROGRESS_REMOVE_START => "Removing",
        c.ALPM_PROGRESS_CONFLICTS_START => "Checking conflicts",
        c.ALPM_PROGRESS_DISKSPACE_START => "Checking disk space",
        c.ALPM_PROGRESS_INTEGRITY_START => "Verifying packages",
        c.ALPM_PROGRESS_LOAD_START => "Loading packages",
        c.ALPM_PROGRESS_KEYRING_START => "Checking keyring",
        else => return,
    };
    ui.progress(label, u.str(package_name), @intCast(@max(percent, 0)), current, total);
}
