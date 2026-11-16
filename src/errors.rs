//! What the user sees when an operation fails: a plain-language message, the
//! subject it concerns, and how to proceed. Errors are values of `Error`: a
//! `Kind` plus an optional subject and hint; this module also describes them.
use crate::ui;
use std::fmt;

pub const EXIT_FAILURE: u8 = 1;
pub const EXIT_USAGE: u8 = 2;
pub const EXIT_CANCELLED: u8 = 130;

pub struct Report {
    pub message: &'static str,
    pub hint: &'static str,
    /// The command line was wrong, as opposed to the operation failing.
    pub usage: bool,
    /// The command already explained the outcome; only the exit status remains.
    pub silent: bool,
}

macro_rules! kinds {
    ($($name:ident => ($message:expr, $hint:expr, $usage:expr, $silent:expr)),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Kind { $($name,)* }
        impl Kind {
            pub fn describe(self) -> Report {
                match self { $(Kind::$name => Report { message: $message, hint: $hint, usage: $usage, silent: $silent },)* }
            }
        }
    };
}

kinds! {
    // Command line.
    UnknownCommand => ("Unknown command", "Run zap help to list every command.", true, false),
    UnknownOption => ("Unknown option", "Run zap help <command> to list the options a command accepts.", true, false),
    OptionNotValidForCommand => ("Option does not apply to this command", "Run zap help <command> to list the options a command accepts.", true, false),
    ConflictingOptions => ("Options contradict each other", "", true, false),
    OptionValueRequired => ("Option needs a value", "", true, false),
    OptionValueNotAllowed => ("Option does not take a value", "", true, false),
    InvalidSearchField => ("Unknown search field", "Use name, name-desc, maintainer, provides, depends, makedepends, checkdepends or optdepends.", true, false),
    InvalidSortField => ("Unknown sort order", "Use votes, popularity, name or modified.", true, false),
    InvalidColorMode => ("Unknown color mode", "Use auto, always or never.", true, false),
    InvalidElevator => ("Unknown elevation tool", "Use run0 or sudo.", true, false),
    InstallationReasonRequired => ("Choose an installation reason", "Add --asdeps or --asexplicit.", true, false),
    UnexpectedTargets => ("This command takes no targets", "", true, false),
    SearchTermRequired => ("Add a search term", "For example: zap search ripgrep", true, false),
    PackageNameRequired => ("Missing target", "Run zap help <command> for usage and examples.", true, false),
    InvalidPackageName => ("Not a valid package name", "Names use letters, digits and @ . _ + - and cannot start with a hyphen or dot.", true, false),
    DuplicateTarget => ("Target given more than once", "", true, false),
    InvalidSelection => ("Not a valid selection", "Choose numbers from the results, for example: 1 3-5", false, false),
    ConflictingSelection => ("Selected the same package from a repository and the AUR", "Choose one source for each package name.", false, false),

    // Environment.
    BuildsMustNotRunAsRoot => ("Run zap as your regular user", "Only the internal transaction worker runs as root, started through run0 or sudo after you approve a change.", false, false),
    InteractiveTerminalRequired => ("An interactive terminal is required", "Every system change is confirmed at a prompt. Run zap from a terminal, or pass --noconfirm to install or upgrade unattended.", false, false),
    ConfirmationRequired => ("Input ended before the question was answered", "Nothing was approved. Run zap from an interactive terminal.", false, false),
    InputTooLong => ("The reply was too long", "", false, false),
    HomeNotSet => ("HOME is not set", "zap keeps its build cache under $XDG_CACHE_HOME or $HOME/.cache.", false, false),
    InvalidCacheDirectory => ("XDG_CACHE_HOME must be an absolute path", "", false, false),
    UnsafeCacheDirectory => ("The build cache is not a directory you own", "Inspect ~/.cache/zap; zap refuses caches owned by another user or replaced by a link.", false, false),
    UnsafeCacheLock => ("The build cache lock is not a regular file you own", "Inspect ~/.cache/zap/.lock.", false, false),
    AnotherBuildIsRunning => ("Another zap build is using the cache", "Wait for it to finish, then try again.", false, false),
    WorkerNotInstalled => ("System changes need zap installed at /usr/bin/zap", "Install the zap package first; a build from a checkout is never elevated.", false, false),
    UntrustedWorkerPath => ("Refusing to elevate through a path root does not control", "/usr/bin/zap, the elevation tool and their directories must be owned by root and not writable by group or others.", false, false),
    ElevatorUnavailable => ("No way to gain root is available", "Install sudo, or run zap on a system booted with systemd and polkit for run0. --elevate chooses one explicitly.", false, false),
    WorkerFailed => ("The system change did not complete", "", false, true),
    WorkerInterrupted => ("The system change was interrupted", "It may be partly applied. Check /var/log/pacman.log, then run zap again; never delete an active database lock.", false, false),
    RootWorkerRequired => ("The transaction worker must be started by zap through run0 or sudo", "", false, false),
    InvalidWorkerArguments => ("The transaction worker was started incorrectly", "", false, false),

    // Lookups.
    PackageNotFound => ("No such package in the repositories or the AUR", "Run zap search <term> to look for similar names.", false, false),
    RepositoryPackageNotFound => ("No such package in the configured repositories", "Run zap search <term>, or drop --repo to include the AUR.", false, false),
    AurPackageNotFound => ("No such package in the AUR", "Run zap search <term> --aur to look for similar names.", false, false),
    PackageNotInstalled => ("Package is not installed", "Run zap list to see installed packages.", false, false),
    FileNotOwned => ("No installed package owns a requested file", "", false, true),
    MissingInstalledFiles => ("Installed files are missing", "", false, true),
    FileInspectionFailed => ("Could not inspect an installed file", "", false, false),
    InvalidPackageFile => ("The local database lists an unsafe file path", "", false, false),

    // Network.
    RequestFailed => ("Network request failed", "Check your connection and try again.", false, false),
    RateLimited => ("The server is rate limiting requests", "Wait a while before trying again.", false, false),
    AurApiError => ("The AUR rejected the request", "", false, false),
    InvalidAurResponse => ("The AUR returned a response zap does not accept", "", false, false),
    InvalidNewsFeed => ("The Arch news feed was not in the expected format", "Read https://archlinux.org/news/ directly.", false, false),

    // pacman configuration and the package database.
    UnsupportedPacmanConfiguration => ("pacman.conf uses a setting zap does not support", "XferCommand and AssumeInstalled are refused rather than silently ignored.", false, false),
    InvalidSignaturePolicy => ("pacman.conf has an unknown SigLevel value", "", false, false),
    InvalidParallelDownloads => ("pacman.conf ParallelDownloads must be between 1 and 128", "", false, false),
    InvalidConfigurationPath => ("pacman.conf paths must be absolute", "", false, false),
    InvalidRepositoryName => ("pacman.conf has an invalid repository name", "", false, false),
    InvalidRepositoryUsage => ("pacman.conf has an unknown Usage value", "", false, false),
    ConfigurationIncludeCycle => ("pacman.conf Include files are nested too deeply", "", false, false),
    ConfigurationIncludeFailed => ("A pacman.conf Include matched no readable file", "", false, false),
    RepositoryRegistrationFailed => ("libalpm could not register a repository", "", false, false),
    AlpmInitializationFailed => ("libalpm could not open the package database", "", false, false),
    AlpmOperationFailed => ("libalpm could not complete the operation", "See the diagnostic above. Never remove an active database lock.", false, false),
    RequiredByOthers => ("Other installed packages still need this, directly or through what it provides", "Remove the packages that need it in the same command, or keep it.", false, false),
    ProtectedPackage => ("Refusing to remove a package protected by HoldPkg", "Edit HoldPkg in /etc/pacman.conf if this removal is intended.", false, false),

    // Dependency planning.
    InvalidDependency => ("Not a valid dependency", "", false, false),
    DependencyCycle => ("Build dependencies form a cycle", "zap does not bootstrap cycles; build one of the packages by hand first.", false, false),
    DependencyGraphTooLarge => ("The dependency graph is too large to plan", "", false, false),
    UnsatisfiedDependency => ("Nothing provides a dependency", "It is in neither the configured repositories nor the AUR.", false, false),
    InvalidProvider => ("Not one of the listed providers", "", false, false),
    AurMetadataMismatch => ("AUR sources do not match the AUR's own metadata", "The .SRCINFO may be stale; report it on the package's AUR page.", false, false),
    InvalidSrcInfo => ("The .SRCINFO is malformed", "", false, false),
    StaleSrcInfo => ("The PKGBUILD and .SRCINFO disagree", "zap plans from .SRCINFO, so it must describe the PKGBUILD. Regenerate it with makepkg --printsrcinfo.", false, false),
    BuildRequiresAurPackage => ("Nothing to build", "build only builds AUR packages; use zap install for repository packages.", false, false),
    MissingBuildDependencies => ("A build dependency is not installed", "build never installs anything; use zap install to resolve dependencies.", false, false),

    // Sources and builds.
    ModifiedBuildFiles => ("Cached build files have local edits", "Commit or discard the edits, or run zap clean <base> to fetch a fresh copy.", false, false),
    UnsafeBuildFile => ("Build sources contain an unsafe file", "Tracked sources must be regular files you own: no links, devices or paths that leave the directory.", false, false),
    LocalSourceTooLarge => ("The local source directory is too large to snapshot", "Limits: 1024 files, 64 MiB in total, 16 MiB per file, 32 levels deep.", false, false),
    SourceNotReviewed => ("Sources were not reviewed", "", false, false),
    BuildSourceChanged => ("Build files changed after you reviewed them", "Nothing further was built or installed. Review the package again.", false, false),
    BuildSandboxUnavailable => ("The build sandbox could not start", "Install bubblewrap and check that unprivileged user namespaces are enabled. --no-sandbox runs approved build code with your user's permissions instead.", false, false),
    UnsafeSandboxDirectory => ("Refusing to sandbox this directory", "The build directory must be a real path outside system directories and the package database.", false, false),
    StagingFailed => ("Could not create a private staging directory", "", false, false),
    CommandFailed => ("Command failed", "See its output above.", false, false),
    MissingBuildArtifacts => ("The build did not produce every required package", "Compare the names with the pkgname entries in the PKGBUILD.", false, false),
    DuplicateBuildArtifact => ("Two build archives claim the same package", "Check the PKGBUILD's split-package declarations.", false, false),
    ArchiveBaseMismatch => ("A built archive belongs to a different package base", "", false, false),
    InvalidBuildArchive => ("Not a valid package archive", "", false, false),

    // Archives and transactions.
    UnsafeArchive => ("Not a readable regular package archive", "Archives must be non-empty regular files; symbolic links are refused.", false, false),
    ArchiveTooLarge => ("Archive exceeds the 4 GiB limit", "", false, false),
    ArchiveChanged => ("Archive changed while it was being read", "", false, false),
    ArchiveDigestMismatch => ("Archive no longer matches the digest you approved", "Nothing was installed.", false, false),
    ArchiveNameMismatch => ("Archive contains a different package than requested", "", false, false),
    InvalidTransactionRequest => (MALFORMED, MALFORMED_HINT, false, false),
    InvalidArchive => (MALFORMED, MALFORMED_HINT, false, false),
    MissingDigest => (MALFORMED, MALFORMED_HINT, false, false),
    InvalidDigest => (MALFORMED, MALFORMED_HINT, false, false),
    InvalidTransactionFlags => (MALFORMED, MALFORMED_HINT, false, false),
    NoTargets => (MALFORMED, MALFORMED_HINT, false, false),
    TransactionTooLarge => ("Too many targets for one transaction", "Split the operation into smaller steps.", false, false),

    // Control flow.
    Cancelled => ("Cancelled", "", false, false),
    WorkerCancelled => ("Cancelled", "", false, true),

    // Operating system.
    FileNotFound => ("No such file or directory", "", false, false),
    AccessDenied => ("Permission denied", "", false, false),
    OutOfMemory => ("Out of memory", "", false, false),
    Io => ("Input/output error", "", false, false),
    FileTooBig => ("File too big", "", false, false),
    StdoutStreamTooLong => ("Stdout stream too long", "", false, false),
}

const MALFORMED: &str = "The transaction request was malformed";
const MALFORMED_HINT: &str = "If zap was just upgraded, run the command again so both halves are the same version.";

/// A failure: its kind, plus the subject it concerns and a specific hint.
#[derive(Clone, Debug)]
pub struct Error {
    pub kind: Kind,
    pub subject: Option<String>,
    pub hint: Option<String>,
    /// For `Kind::Io`: the operating system's wording.
    pub detail: Option<String>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn kind(&self) -> Kind {
        self.kind
    }
}

impl From<Kind> for Error {
    fn from(kind: Kind) -> Error {
        Error { kind, subject: None, hint: None, detail: None }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        use std::io::ErrorKind as K;
        let kind = match err.kind() {
            K::NotFound => Kind::FileNotFound,
            K::PermissionDenied => Kind::AccessDenied,
            K::OutOfMemory => Kind::OutOfMemory,
            _ => Kind::Io,
        };
        let mut e = Error::from(kind);
        if kind == Kind::Io {
            e.detail = Some(io_message(&err));
        }
        e
    }
}

/// The OS wording without Rust's "(os error N)" suffix, capitalised like
/// zap's other messages.
fn io_message(err: &std::io::Error) -> String {
    let text = err.to_string();
    let text = match text.find(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    };
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Input/output error".into(),
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl Error {
    /// The message with its subject, as reported.
    pub fn message(&self) -> String {
        let base = self.detail.clone().unwrap_or_else(|| self.kind.describe().message.to_string());
        match &self.subject {
            Some(subject) => format!("{base}: {subject}"),
            None => base,
        }
    }
}

/// Attach the subject of a failure, such as the package name:
/// `return Err(with(Kind::PackageNotInstalled, name))`.
pub fn with(kind: impl Into<Error>, subject: impl Into<String>) -> Error {
    let mut e = kind.into();
    e.subject = Some(subject.into());
    e.hint = None;
    e
}

/// Like `with`, replacing the catalog's hint with a specific one.
pub fn with_hint(kind: impl Into<Error>, hint: impl Into<String>, subject: impl Into<String>) -> Error {
    let mut e = with(kind, subject);
    e.hint = Some(hint.into());
    e
}

/// `Err(kind)` shorthand.
pub fn fail<T>(kind: Kind) -> Result<T> {
    Err(kind.into())
}

/// Print the failure and return the process exit status.
pub fn report(err: &Error) -> u8 {
    if err.kind == Kind::Cancelled {
        ui::note(ui::NoteKind::Info, "Cancelled.");
        return EXIT_CANCELLED;
    }
    // The root worker already said so.
    if err.kind == Kind::WorkerCancelled {
        return EXIT_CANCELLED;
    }
    let entry = err.kind.describe();
    if !entry.silent {
        let hint = err.hint.as_deref().unwrap_or(entry.hint);
        ui::problem(ui::NoteKind::Failure, &err.message(), hint);
    }
    if entry.usage {
        EXIT_USAGE
    } else {
        EXIT_FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_has_plain_wording_and_usage_errors_have_their_own_exit_status() {
        assert_eq!("Package is not installed", Kind::PackageNotInstalled.describe().message);
        assert!(Kind::UnknownOption.describe().usage);
        assert!(!Kind::AlpmOperationFailed.describe().usage);
        assert!(Kind::FileNotOwned.describe().silent);
        let io: Error = std::io::Error::from_raw_os_error(libc::EISDIR).into();
        assert_eq!("Is a directory", io.message());
    }

    #[test]
    fn a_subject_is_reported_only_with_the_failure_it_was_attached_to() {
        let _guard = ui::test_lock();
        ui::set_caps(Some(ui::Caps { width: Some(60), ..ui::Caps::default() }));
        ui::begin_capture();
        assert_eq!(EXIT_FAILURE, report(&with(Kind::PackageNotInstalled, "ghost\x1b[2J")));
        assert_eq!(EXIT_USAGE, report(&Kind::UnknownCommand.into()));
        assert_eq!(EXIT_USAGE, report(&with_hint(Kind::UnknownCommand, "Did you mean install?", "instal")));
        assert_eq!(EXIT_CANCELLED, report(&Kind::Cancelled.into()));
        assert_eq!(EXIT_CANCELLED, report(&Kind::WorkerCancelled.into()));
        assert_eq!(EXIT_FAILURE, report(&Kind::WorkerFailed.into()));
        assert_eq!(EXIT_FAILURE, report(&Kind::FileNotOwned.into()));
        let out = ui::end_capture();
        ui::set_caps(None);
        assert_eq!(
            "  × Package is not installed: ghost [2J\n    Run zap list to see installed packages.\n  × Unknown command\n    Run zap help to list every command.\n  × Unknown command: instal\n    Did you mean install?\n  · Cancelled.\n",
            out
        );
    }
}
