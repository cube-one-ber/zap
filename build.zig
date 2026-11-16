const std = @import("std");

/// Base release version. Packaging passes `-Dversion=` for development builds;
/// packaging/source.sh checks that this matches packaging/PKGBUILD.in.
const default_version = "0.2.0";

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{ .default_target = .{ .os_tag = .linux, .abi = .gnu, .glibc_version = .{ .major = 2, .minor = 38, .patch = 0 } } });
    const optimize = b.standardOptimizeOption(.{});
    const version = b.option([]const u8, "version", "Version reported by zap (default: " ++ default_version ++ ")") orelse default_version;

    const options = b.addOptions();
    options.addOption([]const u8, "version", version);

    const module = b.createModule(.{
        .root_source_file = b.path("src/main.zig"),
        .target = target,
        .optimize = optimize,
        .link_libc = true,
        .stack_protector = true,
        .pic = true,
    });
    module.addOptions("build_options", options);
    module.addLibraryPath(.{ .cwd_relative = "/usr/lib" });
    module.addSystemIncludePath(.{ .cwd_relative = "/usr/include" });
    module.linkSystemLibrary("alpm", .{});
    module.linkSystemLibrary("curl", .{});
    module.addCSourceFile(.{ .file = b.path("src/alpm_log.c"), .flags = &.{ "-std=c11", "-Wall", "-Wextra" } });

    const exe = b.addExecutable(.{ .name = "zap", .root_module = module, .use_llvm = true });
    exe.pie = true;
    b.installArtifact(exe);

    const run = b.addRunArtifact(exe);
    if (b.args) |args| run.addArgs(args);
    b.step("run", "Run zap").dependOn(&run.step);

    const tests = b.addTest(.{ .root_module = module, .use_llvm = true });
    const test_run = b.addRunArtifact(tests);
    b.step("test", "Run unit tests").dependOn(&test_run.step);
}
