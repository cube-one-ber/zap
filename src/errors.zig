//! What the user sees when an operation fails: a plain-language message, the
//! subject it concerns, and how to proceed. Error values stay ordinary Zig
//! errors; this module only describes them.
const std = @import("std");
const u = @import("util.zig");
const ui = @import("ui.zig");

pub const Report = struct {
    message: []const u8,
    hint: []const u8 = "",
    /// The command line was wrong, as opposed to the operation failing.
    usage: bool = false,
    /// The command already explained the outcome; only the exit status remains.
    silent: bool = false,
};

pub const exit_failure = 1;
pub const exit_usage = 2;
pub const exit_cancelled = 130;

var subject: ?[]const u8 = null;
var advice: ?[]const u8 = null;
var subject_of: ?anyerror = null;

/// Attach the subject of a failure, such as the package name, and return the
/// error unchanged: `return errors.with(error.PackageNotInstalled, "{s}", .{name});`
pub fn with(err: anytype, comptime fmt: []const u8, args: anytype) @TypeOf(err) {
    subject = std.fmt.allocPrint(u.a, fmt, args) catch null;
    advice = null;
    subject_of = err;
    return err;
}

/// Like `with`, replacing the catalog's hint with a specific one.
pub fn withHint(err: anytype, hint: []const u8, comptime fmt: []const u8, args: anytype) @TypeOf(err) {
    const result = with(err, fmt, args);
    advice = hint;
    return result;
}

/// Print the failure and return the process exit status.
pub fn report(err: anyerror) u8 {
    if (err == error.Cancelled) {
        ui.note(.info, "Cancelled.");
        return exit_cancelled;
    }
    // The root worker already said so.
    if (err == error.WorkerCancelled) return exit_cancelled;
    const entry = describe(err);
    if (!entry.silent) {
        const attached = subject_of != null and subject_of.? == err;
        const message = if (attached and subject != null)
            std.fmt.allocPrint(u.a, "{s}: {s}", .{ entry.message, subject.? }) catch entry.message
        else
            entry.message;
        ui.problem(.failure, message, if (attached and advice != null) advice.? else entry.hint);
    }
    return if (entry.usage) exit_usage else exit_failure;
}

pub fn describe(err: anyerror) Report {
    return switch (err) {
        // Command line.
        error.UnknownCommand => .{ .message = "Unknown command", .hint = "Run zap help to list every command.", .usage = true },
        error.UnknownOption => .{ .message = "Unknown option", .hint = "Run zap help <command> to list the options a command accepts.", .usage = true },
        error.OptionNotValidForCommand => .{ .message = "Option does not apply to this command", .hint = "Run zap help <command> to list the options a command accepts.", .usage = true },
        error.ConflictingOptions => .{ .message = "Options contradict each other", .usage = true },
        error.OptionValueRequired => .{ .message = "Option needs a value", .usage = true },
        error.OptionValueNotAllowed => .{ .message = "Option does not take a value", .usage = true },
        error.InvalidSearchField => .{ .message = "Unknown search field", .hint = "Use name, name-desc, maintainer, provides, depends, makedepends, checkdepends or optdepends.", .usage = true },
        error.InvalidSortField => .{ .message = "Unknown sort order", .hint = "Use votes, popularity, name or modified.", .usage = true },
        error.InvalidColorMode => .{ .message = "Unknown color mode", .hint = "Use auto, always or never.", .usage = true },
        error.InvalidElevator => .{ .message = "Unknown elevation tool", .hint = "Use run0 or sudo.", .usage = true },
        error.InstallationReasonRequired => .{ .message = "Choose an installation reason", .hint = "Add --asdeps or --asexplicit.", .usage = true },
        error.UnexpectedTargets => .{ .message = "This command takes no targets", .usage = true },
        error.SearchTermRequired => .{ .message = "Add a search term", .hint = "For example: zap search ripgrep", .usage = true },
        error.PackageNameRequired => .{ .message = "Missing target", .hint = "Run zap help <command> for usage and examples.", .usage = true },
        error.InvalidPackageName => .{ .message = "Not a valid package name", .hint = "Names use letters, digits and @ . _ + - and cannot start with a hyphen or dot.", .usage = true },
        error.DuplicateTarget => .{ .message = "Target given more than once", .usage = true },
        error.InvalidSelection => .{ .message = "Not a valid selection", .hint = "Choose numbers from the results, for example: 1 3-5" },
        error.ConflictingSelection => .{ .message = "Selected the same package from a repository and the AUR", .hint = "Choose one source for each package name." },

        // Environment.
        error.BuildsMustNotRunAsRoot => .{ .message = "Run zap as your regular user", .hint = "Only the internal transaction worker runs as root, started through run0 or sudo after you approve a change." },
        error.InteractiveTerminalRequired => .{ .message = "An interactive terminal is required", .hint = "Every system change is confirmed at a prompt; zap has no unattended mode. Use --dry-run to preview." },
        error.ConfirmationRequired => .{ .message = "Input ended before the question was answered", .hint = "Nothing was approved. Run zap from an interactive terminal." },
        error.InputTooLong => .{ .message = "The reply was too long" },
        error.HomeNotSet => .{ .message = "HOME is not set", .hint = "zap keeps its build cache under $XDG_CACHE_HOME or $HOME/.cache." },
        error.InvalidCacheDirectory => .{ .message = "XDG_CACHE_HOME must be an absolute path" },
        error.UnsafeCacheDirectory => .{ .message = "The build cache is not a directory you own", .hint = "Inspect ~/.cache/zap; zap refuses caches owned by another user or replaced by a link." },
        error.UnsafeCacheLock => .{ .message = "The build cache lock is not a regular file you own", .hint = "Inspect ~/.cache/zap/.lock." },
        error.AnotherBuildIsRunning => .{ .message = "Another zap build is using the cache", .hint = "Wait for it to finish, then try again." },
        error.WorkerNotInstalled => .{ .message = "System changes need zap installed at /usr/bin/zap", .hint = "Install the zap package first; a build from a checkout is never elevated." },
        error.UntrustedWorkerPath => .{ .message = "Refusing to elevate through a path root does not control", .hint = "/usr/bin/zap, the elevation tool and their directories must be owned by root and not writable by group or others." },
        error.ElevatorUnavailable => .{ .message = "No way to gain root is available", .hint = "Install sudo, or run zap on a system booted with systemd and polkit for run0. --elevate chooses one explicitly." },
        error.WorkerFailed => .{ .message = "The system change did not complete", .silent = true },
        error.WorkerInterrupted => .{ .message = "The system change was interrupted", .hint = "It may be partly applied. Check /var/log/pacman.log, then run zap again; never delete an active database lock." },
        error.RootWorkerRequired => .{ .message = "The transaction worker must be started by zap through run0 or sudo" },
        error.InvalidWorkerArguments => .{ .message = "The transaction worker was started incorrectly" },

        // Lookups.
        error.PackageNotFound => .{ .message = "No such package in the repositories or the AUR", .hint = "Run zap search <term> to look for similar names." },
        error.RepositoryPackageNotFound => .{ .message = "No such package in the configured repositories", .hint = "Run zap search <term>, or drop --repo to include the AUR." },
        error.AurPackageNotFound => .{ .message = "No such package in the AUR", .hint = "Run zap search <term> --aur to look for similar names." },
        error.PackageNotInstalled => .{ .message = "Package is not installed", .hint = "Run zap list to see installed packages." },
        error.FileNotOwned => .{ .message = "No installed package owns a requested file", .silent = true },
        error.MissingInstalledFiles => .{ .message = "Installed files are missing", .silent = true },
        error.FileInspectionFailed => .{ .message = "Could not inspect an installed file" },
        error.InvalidPackageFile => .{ .message = "The local database lists an unsafe file path" },

        // Network.
        error.RequestFailed => .{ .message = "Network request failed", .hint = "Check your connection and try again." },
        error.RateLimited => .{ .message = "The server is rate limiting requests", .hint = "Wait a while before trying again." },
        error.AurApiError => .{ .message = "The AUR rejected the request" },
        error.InvalidAurResponse => .{ .message = "The AUR returned a response zap does not accept" },
        error.InvalidNewsFeed => .{ .message = "The Arch news feed was not in the expected format", .hint = "Read https://archlinux.org/news/ directly." },
        error.CurlInitializationFailed, error.CurlOptionFailed => .{ .message = "Could not initialise libcurl" },

        // pacman configuration and libalpm.
        error.UnsupportedPacmanConfiguration => .{ .message = "pacman.conf uses a setting zap does not support", .hint = "XferCommand and AssumeInstalled are refused rather than silently ignored." },
        error.InvalidSignaturePolicy => .{ .message = "pacman.conf has an unknown SigLevel value" },
        error.InvalidParallelDownloads => .{ .message = "pacman.conf ParallelDownloads must be between 1 and 128" },
        error.InvalidConfigurationPath => .{ .message = "pacman.conf paths must be absolute" },
        error.InvalidRepositoryName => .{ .message = "pacman.conf has an invalid repository name" },
        error.InvalidRepositoryUsage => .{ .message = "pacman.conf has an unknown Usage value" },
        error.ConfigurationIncludeCycle => .{ .message = "pacman.conf Include files are nested too deeply" },
        error.ConfigurationIncludeFailed => .{ .message = "A pacman.conf Include matched no readable file" },
        error.RepositoryRegistrationFailed => .{ .message = "libalpm could not register a repository" },
        error.AlpmInitializationFailed => .{ .message = "libalpm could not open the package database" },
        error.AlpmOperationFailed => .{ .message = "libalpm could not complete the operation", .hint = "See the diagnostic above. Never remove an active database lock." },
        error.RequiredByOthers => .{ .message = "Other installed packages still need this, directly or through what it provides", .hint = "Remove the packages that need it in the same command, or keep it." },
        error.ProtectedPackage => .{ .message = "Refusing to remove a package protected by HoldPkg", .hint = "Edit HoldPkg in /etc/pacman.conf if this removal is intended." },

        // Dependency planning.
        error.InvalidDependency => .{ .message = "Not a valid dependency" },
        error.DependencyCycle => .{ .message = "Build dependencies form a cycle", .hint = "zap does not bootstrap cycles; build one of the packages by hand first." },
        error.DependencyGraphTooLarge => .{ .message = "The dependency graph is too large to plan" },
        error.UnsatisfiedDependency => .{ .message = "Nothing provides a dependency", .hint = "It is in neither the configured repositories nor the AUR." },
        error.InvalidProvider => .{ .message = "Not one of the listed providers" },
        error.AurMetadataMismatch => .{ .message = "AUR sources do not match the AUR's own metadata", .hint = "The .SRCINFO may be stale; report it on the package's AUR page." },
        error.InvalidSrcInfo => .{ .message = "The .SRCINFO is malformed" },
        error.StaleSrcInfo => .{ .message = "The PKGBUILD and .SRCINFO disagree", .hint = "zap plans from .SRCINFO, so it must describe the PKGBUILD. Regenerate it with makepkg --printsrcinfo." },
        error.BuildRequiresAurPackage => .{ .message = "Nothing to build", .hint = "build only builds AUR packages; use zap install for repository packages." },
        error.MissingBuildDependencies => .{ .message = "A build dependency is not installed", .hint = "build never installs anything; use zap install to resolve dependencies." },

        // Sources and builds.
        error.ModifiedBuildFiles => .{ .message = "Cached build files have local edits", .hint = "Commit or discard the edits, or run zap clean <base> to fetch a fresh copy." },
        error.UnsafeBuildFile => .{ .message = "Build sources contain an unsafe file", .hint = "Tracked sources must be regular files you own: no links, devices or paths that leave the directory." },
        error.LocalSourceTooLarge => .{ .message = "The local source directory is too large to snapshot", .hint = "Limits: 1024 files, 64 MiB in total, 16 MiB per file, 32 levels deep." },
        error.SourceNotReviewed => .{ .message = "Sources were not reviewed" },
        error.BuildSourceChanged => .{ .message = "Build files changed after you reviewed them", .hint = "Nothing further was built or installed. Review the package again." },
        error.BuildSandboxUnavailable => .{ .message = "The build sandbox could not start", .hint = "Install bubblewrap and check that unprivileged user namespaces are enabled. --no-sandbox runs approved build code with your user's permissions instead." },
        error.UnsafeSandboxDirectory => .{ .message = "Refusing to sandbox this directory", .hint = "The build directory must be a real path outside system directories and the package database." },
        error.StagingFailed => .{ .message = "Could not create a private staging directory" },
        error.CommandFailed => .{ .message = "Command failed", .hint = "See its output above." },
        error.MissingBuildArtifacts => .{ .message = "The build did not produce every required package", .hint = "Compare the names with the pkgname entries in the PKGBUILD." },
        error.DuplicateBuildArtifact => .{ .message = "Two build archives claim the same package", .hint = "Check the PKGBUILD's split-package declarations." },
        error.ArchiveBaseMismatch => .{ .message = "A built archive belongs to a different package base" },
        error.InvalidBuildArchive => .{ .message = "Not a valid package archive" },

        // Archives and transactions.
        error.UnsafeArchive => .{ .message = "Not a readable regular package archive", .hint = "Archives must be non-empty regular files; symbolic links are refused." },
        error.ArchiveTooLarge => .{ .message = "Archive exceeds the 4 GiB limit" },
        error.ArchiveChanged => .{ .message = "Archive changed while it was being read" },
        error.ArchiveDigestMismatch => .{ .message = "Archive no longer matches the digest you approved", .hint = "Nothing was installed." },
        error.ArchiveNameMismatch => .{ .message = "Archive contains a different package than requested" },
        error.InvalidTransactionRequest, error.InvalidArchive, error.MissingDigest, error.InvalidDigest, error.InvalidTransactionFlags, error.NoTargets => .{
            .message = "The transaction request was malformed",
            .hint = "If zap was just upgraded, run the command again so both halves are the same version.",
        },
        error.TransactionTooLarge => .{ .message = "Too many targets for one transaction", .hint = "Split the operation into smaller steps." },

        error.FileNotFound => .{ .message = "No such file or directory" },
        error.AccessDenied => .{ .message = "Permission denied" },
        error.OutOfMemory => .{ .message = "Out of memory" },
        else => .{ .message = humanize(@errorName(err)) },
    };
}

/// `PackageNotInstalled` becomes `Package not installed`.
fn humanize(name: []const u8) []const u8 {
    var out: std.ArrayList(u8) = .empty;
    for (name, 0..) |ch, i| {
        if (std.ascii.isUpper(ch) and i > 0) {
            out.append(u.a, ' ') catch return name;
            out.append(u.a, std.ascii.toLower(ch)) catch return name;
        } else out.append(u.a, ch) catch return name;
    }
    return out.items;
}

test "every failure has plain wording and usage errors have their own exit status" {
    try std.testing.expectEqualStrings("Package is not installed", describe(error.PackageNotInstalled).message);
    try std.testing.expectEqualStrings("Some new failure", describe(error.SomeNewFailure).message);
    try std.testing.expect(describe(error.UnknownOption).usage);
    try std.testing.expect(!describe(error.AlpmOperationFailed).usage);
    try std.testing.expect(describe(error.FileNotOwned).silent);
}

test "a subject is reported only with the failure it was attached to" {
    defer ui.setCaps(null);
    ui.setCaps(.{ .width = 60 });
    ui.beginCapture();
    const attached: anyerror = with(error.PackageNotInstalled, "{s}", .{"ghost\x1b[2J"});
    try std.testing.expectEqual(@as(u8, exit_failure), report(attached));
    try std.testing.expectEqual(@as(u8, exit_usage), report(error.UnknownCommand));
    const hinted: anyerror = withHint(error.UnknownCommand, "Did you mean install?", "{s}", .{"instal"});
    try std.testing.expectEqual(@as(u8, exit_usage), report(hinted));
    try std.testing.expectEqual(@as(u8, exit_cancelled), report(error.Cancelled));
    try std.testing.expectEqual(@as(u8, exit_cancelled), report(error.WorkerCancelled));
    try std.testing.expectEqual(@as(u8, exit_failure), report(error.WorkerFailed));
    try std.testing.expectEqual(@as(u8, exit_failure), report(error.FileNotOwned));
    try std.testing.expectEqualStrings(
        \\  × Package is not installed: ghost [2J
        \\    Run zap list to see installed packages.
        \\  × Unknown command
        \\    Run zap help to list every command.
        \\  × Unknown command: instal
        \\    Did you mean install?
        \\  · Cancelled.
        \\
    , ui.endCapture());
}
