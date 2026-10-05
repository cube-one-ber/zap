const std = @import("std");
const u = @import("util.zig");
const c = u.c;
const ui = @import("ui.zig");
const config = @import("config.zig");
const signals = @import("signals.zig");
extern fn zap_set_log_callback(handle: *c.alpm_handle_t) c_int;
export fn zap_log_message(level: c_int, message: [*c]const u8) callconv(.c) void {
    ui.diagnostic(if (level & c.ALPM_LOG_ERROR != 0) .danger else .warning, u.str(message));
}
pub const Pkg = *c.alpm_pkg_t;
pub fn pkg(data: ?*anyopaque) Pkg {
    return @ptrCast(@alignCast(data.?));
}
pub fn name(p: Pkg) []const u8 {
    return u.str(c.alpm_pkg_get_name(p));
}
pub fn version(p: Pkg) []const u8 {
    return u.str(c.alpm_pkg_get_version(p));
}
pub fn show(p: Pkg) void {
    showSearch(p, null, null);
}
pub fn showSearch(p: Pkg, index: ?usize, installed: ?[]const u8) void {
    const annotation = if (installed) |old| std.fmt.allocPrint(u.a, "[installed{s}{s}]", .{ if (std.mem.eql(u8, old, version(p))) @as([]const u8, "") else ": ", if (std.mem.eql(u8, old, version(p))) @as([]const u8, "") else ui.safe(old) }) catch return else "";
    ui.packageHeader(name(p), version(p), u.str(c.alpm_db_get_name(c.alpm_pkg_get_db(p))), index, annotation);
    ui.text(u.str(c.alpm_pkg_get_desc(p)), 4, .reset);
    const stamp = std.fmt.allocPrint(u.a, "{d:.1} MiB installed · built {s}", .{ @as(f64, @floatFromInt(c.alpm_pkg_get_isize(p))) / (1024 * 1024), ui.date(c.alpm_pkg_get_builddate(p)) }) catch return;
    defer u.a.free(stamp);
    ui.text(stamp, 4, .muted);
}
pub const Alpm = struct {
    h: *c.alpm_handle_t,
    cfg: config.Config,
    pub fn init() !Alpm {
        const cfg = try config.Config.load();
        var err: c.alpm_errno_t = 0;
        const h = c.alpm_initialize((try u.z(cfg.root)).ptr, (try u.z(cfg.db)).ptr, &err) orelse {
            ui.print("libalpm: {s}\n", .{u.str(c.alpm_strerror(err))});
            return error.AlpmInitializationFailed;
        };
        errdefer _ = c.alpm_release(h);
        var self: Alpm = .{ .h = h, .cfg = cfg };
        try self.check(zap_set_log_callback(h));
        for (cfg.architectures.items) |arch| try self.check(c.alpm_option_add_architecture(h, (try u.z(arch)).ptr));
        try self.check(c.alpm_option_set_logfile(h, (try u.z(cfg.log)).ptr));
        try self.check(c.alpm_option_set_gpgdir(h, (try u.z(cfg.gpg)).ptr));
        const sig = try config.sigLevel(cfg.signature, 0);
        try self.check(c.alpm_option_set_default_siglevel(h, sig));
        try self.check(c.alpm_option_set_local_file_siglevel(h, if (cfg.local_signature) |tokens| try config.sigLevel(tokens, sig) else sig));
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
        for (cfg.repos.items) |repository| {
            const db = c.alpm_register_syncdb(h, (try u.z(repository.name)).ptr, if (repository.signatures) |tokens| try config.sigLevel(tokens, sig) else sig) orelse return error.RepositoryRegistrationFailed;
            var usage: c_int = 0;
            var words = std.mem.tokenizeAny(u8, repository.usage, " \t");
            while (words.next()) |word| {
                if (std.mem.eql(u8, word, "All")) usage |= c.ALPM_DB_USAGE_ALL else if (std.mem.eql(u8, word, "Sync")) usage |= c.ALPM_DB_USAGE_SYNC else if (std.mem.eql(u8, word, "Search")) usage |= c.ALPM_DB_USAGE_SEARCH else if (std.mem.eql(u8, word, "Install")) usage |= c.ALPM_DB_USAGE_INSTALL else if (std.mem.eql(u8, word, "Upgrade")) usage |= c.ALPM_DB_USAGE_UPGRADE else return error.InvalidRepositoryUsage;
            }
            try self.check(c.alpm_db_set_usage(db, usage));
            for (repository.servers.items) |server| {
                const first = try std.mem.replaceOwned(u8, u.a, server, "$repo", repository.name);
                defer u.a.free(first);
                const expanded = try std.mem.replaceOwned(u8, u.a, first, "$arch", cfg.arch);
                defer u.a.free(expanded);
                try self.check(c.alpm_db_add_server(db, (try u.z(expanded)).ptr));
            }
        }
        try self.check(c.alpm_option_set_questioncb(h, question, null));
        try self.check(c.alpm_option_set_eventcb(h, event, h));
        try self.check(c.alpm_option_set_progresscb(h, progress, h));
        return self;
    }
    pub fn deinit(self: *Alpm) void {
        _ = c.alpm_release(self.h);
    }
    pub fn check(self: *const Alpm, result: c_int) !void {
        if (result < 0) {
            ui.print("libalpm: {s}\n", .{u.str(c.alpm_strerror(c.alpm_errno(self.h)))});
            return error.AlpmOperationFailed;
        }
    }
    pub fn local(self: *const Alpm, dep: []const u8) !?Pkg {
        return c.alpm_find_satisfier(c.alpm_db_get_pkgcache(c.alpm_get_localdb(self.h)), (try u.z(dep)).ptr);
    }
    pub fn repo(self: *const Alpm, dep: []const u8) !?Pkg {
        return c.alpm_find_dbs_satisfier(self.h, c.alpm_get_syncdbs(self.h), (try u.z(dep)).ptr);
    }
    pub fn installed(self: *const Alpm) [*c]c.alpm_list_t {
        return c.alpm_db_get_pkgcache(c.alpm_get_localdb(self.h));
    }
    pub fn foreign(self: *const Alpm, p: Pkg) !bool {
        var dbs = c.alpm_get_syncdbs(self.h);
        while (dbs != null) : (dbs = dbs.*.next) {
            const db: *c.alpm_db_t = @ptrCast(@alignCast(dbs.*.data.?));
            if (c.alpm_db_get_pkg(db, (try u.z(name(p))).ptr) != null) return false;
        }
        return true;
    }
    pub fn search(self: *const Alpm, term: []const u8) !void {
        var needles: [*c]c.alpm_list_t = null;
        needles = c.alpm_list_add(needles, @constCast((try u.z(term)).ptr));
        defer c.alpm_list_free(needles);
        var dbs = c.alpm_get_syncdbs(self.h);
        while (dbs != null) : (dbs = dbs.*.next) {
            const db: *c.alpm_db_t = @ptrCast(@alignCast(dbs.*.data.?));
            var usage: c_int = 0;
            try self.check(c.alpm_db_get_usage(db, &usage));
            if (usage & c.ALPM_DB_USAGE_SEARCH == 0) continue;
            var result: [*c]c.alpm_list_t = null;
            try self.check(c.alpm_db_search(db, needles, &result));
            defer c.alpm_list_free(result);
            var it = result;
            while (it != null) : (it = it.*.next) show(pkg(it.*.data));
        }
    }
    pub fn orphans(self: *const Alpm) !std.ArrayList([]const u8) {
        var result: std.ArrayList([]const u8) = .empty;
        var it = self.installed();
        while (it != null) : (it = it.*.next) {
            const p = pkg(it.*.data);
            if (c.alpm_pkg_get_reason(p) != c.ALPM_PKG_REASON_DEPEND) continue;
            const required = c.alpm_pkg_compute_requiredby(p);
            defer freeStrings(required);
            const optional = c.alpm_pkg_compute_optionalfor(p);
            defer freeStrings(optional);
            if (required == null and optional == null) try result.append(u.a, try u.a.dupe(u8, name(p)));
        }
        return result;
    }
    pub fn refresh(self: *const Alpm) !void {
        ui.title("Refresh repository databases");
        try self.check(c.alpm_db_update(self.h, c.alpm_get_syncdbs(self.h), 0));
    }
    pub fn errors(self: *const Alpm, data: [*c]c.alpm_list_t) void {
        const err = c.alpm_errno(self.h);
        var it = data;
        while (it != null) : (it = it.*.next) {
            switch (err) {
                c.ALPM_ERR_UNSATISFIED_DEPS => {
                    const missing: *c.alpm_depmissing_t = @ptrCast(@alignCast(it.*.data.?));
                    const dep = c.alpm_dep_compute_string(missing.depend);
                    defer c.free(dep);
                    ui.print("  {s} requires {s}\n", .{ ui.safe(u.str(missing.target)), ui.safe(u.str(dep)) });
                    c.alpm_depmissing_free(missing);
                },
                c.ALPM_ERR_CONFLICTING_DEPS => {
                    const conflict: *c.alpm_conflict_t = @ptrCast(@alignCast(it.*.data.?));
                    ui.print("  Conflict: {s} / {s}\n", .{ ui.safe(conflictName(conflict.package1)), ui.safe(conflictName(conflict.package2)) });
                    c.alpm_conflict_free(conflict);
                },
                c.ALPM_ERR_FILE_CONFLICTS => {
                    const conflict: *c.alpm_fileconflict_t = @ptrCast(@alignCast(it.*.data.?));
                    ui.print("  File conflict: {s} ({s})\n", .{ ui.safe(u.str(conflict.file)), ui.safe(u.str(conflict.target)) });
                    c.alpm_fileconflict_free(conflict);
                },
                else => {
                    ui.print("  {s}\n", .{ui.safe(u.str(@ptrCast(it.*.data.?)))});
                    c.free(it.*.data);
                },
            }
        }
        c.alpm_list_free(data);
    }
};
fn freeStrings(list: [*c]c.alpm_list_t) void {
    var it = list;
    while (it != null) : (it = it.*.next) c.free(it.*.data);
    c.alpm_list_free(list);
}
fn question(_: ?*anyopaque, q: [*c]c.alpm_question_t) callconv(.c) void {
    q.*.any.answer = 0;
    switch (q.*.type) {
        c.ALPM_QUESTION_INSTALL_IGNOREPKG => {
            ui.print("Ignored package: {s}\n", .{ui.safe(name(q.*.install_ignorepkg.pkg.?))});
            q.*.install_ignorepkg.install = @intFromBool(ui.confirm("Override IgnorePkg? [y/N] "));
        },
        c.ALPM_QUESTION_REPLACE_PKG => {
            ui.print("Replace {s} with {s}?\n", .{ ui.safe(name(q.*.replace.oldpkg.?)), ui.safe(name(q.*.replace.newpkg.?)) });
            q.*.replace.replace = @intFromBool(ui.confirm("Approve replacement? [y/N] "));
        },
        c.ALPM_QUESTION_CONFLICT_PKG => {
            ui.print("{s} conflicts with {s}.\n", .{ ui.safe(conflictName(q.*.conflict.conflict.*.package1)), ui.safe(conflictName(q.*.conflict.conflict.*.package2)) });
            q.*.conflict.remove = @intFromBool(ui.confirm("Remove the conflicting installed package? [y/N] "));
        },
        c.ALPM_QUESTION_SELECT_PROVIDER => {
            var it = q.*.select_provider.providers;
            var count: usize = 0;
            while (it != null) : (it = it.*.next) {
                count += 1;
                ui.print("  {d}. {s}\n", .{ count, ui.safe(name(pkg(it.*.data))) });
            }
            const reply = ui.answer("Select provider [1]: ") catch {
                q.*.select_provider.use_index = -1;
                return;
            };
            const index = if (reply.len == 0) 1 else std.fmt.parseInt(usize, reply, 10) catch 0;
            q.*.select_provider.use_index = if (index > 0 and index <= count) @intCast(index - 1) else -1;
        },
        c.ALPM_QUESTION_IMPORT_KEY => {
            ui.print("Import key {s} ({s})\n", .{ ui.safe(u.str(q.*.import_key.fingerprint)), ui.safe(u.str(q.*.import_key.uid)) });
            q.*.import_key.import = @intFromBool(ui.confirm("Trust this key for package verification? [y/N] "));
        },
        // Never silently skip unsatisfied targets or delete corrupted files.
        else => {},
    }
}
fn event(ctx: ?*anyopaque, e: [*c]c.alpm_event_t) callconv(.c) void {
    if (signals.cancelled.load(.monotonic)) {
        _ = c.alpm_trans_interrupt(@ptrCast(ctx));
        return;
    }
    switch (e.*.type) {
        c.ALPM_EVENT_SCRIPTLET_INFO => ui.print("  {s}\n", .{ui.safe(u.str(e.*.scriptlet_info.line))}),
        c.ALPM_EVENT_HOOK_RUN_START => ui.text(std.fmt.allocPrint(u.a, "Hook {d}/{d} · {s}", .{ e.*.hook_run.position, e.*.hook_run.total, u.str(e.*.hook_run.desc) }) catch return, 2, .muted),
        c.ALPM_EVENT_PACNEW_CREATED => ui.note(.warning, std.fmt.allocPrint(u.a, "Review {s}.pacnew", .{u.str(e.*.pacnew_created.file)}) catch return),
        c.ALPM_EVENT_PACSAVE_CREATED => ui.note(.warning, std.fmt.allocPrint(u.a, "Saved {s}.pacsave", .{u.str(e.*.pacsave_created.file)}) catch return),
        c.ALPM_EVENT_PKG_RETRIEVE_START => ui.title(std.fmt.allocPrint(u.a, "Download · {d} packages · {d:.1} MiB", .{ e.*.pkg_retrieve.num, @as(f64, @floatFromInt(e.*.pkg_retrieve.total_size)) / (1024 * 1024) }) catch return),
        else => {},
    }
}

fn conflictName(value: anytype) []const u8 {
    return switch (@typeInfo(@TypeOf(value))) {
        .optional => if (value) |p| name(p) else "unknown",
        else => u.str(value),
    };
}

fn progress(ctx: ?*anyopaque, stage: c.alpm_progress_t, package_name: [*c]const u8, percent: c_int, total: usize, current: usize) callconv(.c) void {
    if (signals.cancelled.load(.monotonic)) {
        _ = c.alpm_trans_interrupt(@ptrCast(ctx));
        return;
    }
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
