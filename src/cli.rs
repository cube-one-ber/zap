//! The command line as data: one table of commands and one of options drive
//! parsing, validation, suggestions and every help page.
use crate::errors::{self, Kind, Result};
use crate::ui::ColorMode;

macro_rules! commands {
    ($($variant:ident = $tag:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Command { $($variant,)* }
        impl Command {
            #[cfg(test)]
            pub const ALL: &'static [Command] = &[$(Command::$variant,)*];
            /// The snake_case identifier (`install_files`), also accepted as a spelling.
            pub fn tag(self) -> &'static str {
                match self { $(Command::$variant => $tag,)* }
            }
        }
    };
}

commands! {
    Help = "help", Version = "version", Search = "search", Select = "select", Info = "info",
    Localinfo = "localinfo", Install = "install", InstallFiles = "install_files", Build = "build",
    BuildLocal = "build_local", InstallLocal = "install_local", Get = "get", Pkgbuild = "pkgbuild",
    Upgrade = "upgrade", Updates = "updates", List = "list", Foreign = "foreign",
    LocalSearch = "local_search", Files = "files", Owns = "owns", Check = "check", Remove = "remove",
    Orphans = "orphans", Autoremove = "autoremove", Clean = "clean", Stats = "stats", News = "news",
    Reason = "reason",
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    All,
    Aur,
    Repo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reason {
    Explicit,
    Dependency,
}

macro_rules! named {
    ($name:ident { $($variant:ident = $text:literal),* $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum $name { $($variant,)* }
        impl $name {
            pub fn parse(s: &str) -> Option<$name> {
                match s { $($text => Some($name::$variant),)* _ => None }
            }
            pub fn name(self) -> &'static str {
                match self { $($name::$variant => $text,)* }
            }
        }
    };
}

named!(SearchBy {
    Name = "name", NameDesc = "name-desc", Maintainer = "maintainer", Depends = "depends",
    Makedepends = "makedepends", Checkdepends = "checkdepends", Optdepends = "optdepends", Provides = "provides",
});
named!(SortBy { Votes = "votes", Popularity = "popularity", Name = "name", Modified = "modified" });
named!(Elevator { Run0 = "run0", Sudo = "sudo" });

#[derive(Clone, Debug)]
pub struct Options {
    pub command: Command,
    /// The command a help request is about, when it names one.
    pub topic: Option<Command>,
    pub names: Vec<String>,
    pub scope: Scope,
    pub dry: bool,
    pub devel: bool,
    pub recursive: bool,
    pub nosave: bool,
    pub quiet: bool,
    pub needed: bool,
    pub sandbox: Option<bool>,
    pub clean_after: bool,
    pub rebuild_tree: bool,
    pub reason: Option<Reason>,
    pub search_by: SearchBy,
    pub sort_by: SortBy,
    pub color: ColorMode,
    pub elevate: Option<Elevator>,
    /// Unattended: approve every prompt the way --noconfirm documents.
    pub noconfirm: bool,
}

impl Options {
    pub fn new(command: Command) -> Options {
        Options {
            command,
            topic: None,
            names: Vec::new(),
            scope: Scope::All,
            dry: false,
            devel: false,
            recursive: false,
            nosave: false,
            quiet: false,
            needed: false,
            sandbox: None,
            clean_after: false,
            rebuild_tree: false,
            reason: None,
            search_by: SearchBy::NameDesc,
            sort_by: SortBy::Votes,
            color: ColorMode::Auto,
            elevate: None,
            noconfirm: false,
        }
    }
}

// ── Commands ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Everyday,
    Inspect,
    Build,
    Maintain,
    About,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Targets {
    None,
    Term,
    Required,
    Optional,
    Topic,
}

pub struct Spec {
    pub command: Command,
    pub name: &'static str,
    pub alias: &'static str,
    pub operands: &'static str,
    pub targets: Targets,
    pub group: Group,
    pub summary: &'static str,
    pub about: &'static str,
    pub examples: &'static [&'static str],
}

const fn s(command: Command, name: &'static str, group: Group, summary: &'static str) -> Spec {
    Spec { command, name, alias: "", operands: "", targets: Targets::None, group, summary, about: "", examples: &[] }
}

impl Spec {
    const fn alias(mut self, alias: &'static str) -> Spec {
        self.alias = alias;
        self
    }
    const fn takes(mut self, operands: &'static str, targets: Targets) -> Spec {
        self.operands = operands;
        self.targets = targets;
        self
    }
    const fn about(mut self, about: &'static str) -> Spec {
        self.about = about;
        self
    }
    const fn examples(mut self, examples: &'static [&'static str]) -> Spec {
        self.examples = examples;
        self
    }
}

use Command as C;
use Group as G;
use Targets as T;

pub static SPECS: [Spec; 28] = [
    s(C::Search, "search", G::Everyday, "Find packages in repositories and the AUR")
        .alias("-Ss")
        .takes("TERM", T::Term)
        .about("Searches the cached repository databases and live AUR metadata. Repository results show the installed size and build date; AUR results show votes, popularity, the maintainer and the last modification. Results you already have installed are marked.")
        .examples(&["zap search ripgrep", "zap search browser --aur --sortby popularity", "zap search alice --aur --searchby maintainer"]),
    s(C::Select, "select", G::Everyday, "Search, choose numbered results, then install them")
        .takes("TERM", T::Term)
        .about("Lists results with the best match nearest the prompt, then installs the numbers you choose. Separate choices with spaces or commas and use a dash for ranges, such as 1 3-5. An empty reply cancels.")
        .examples(&["zap select browser", "zap select terminal --aur --needed"]),
    s(C::Install, "install", G::Everyday, "Resolve dependencies, review sources, build and install")
        .alias("-S")
        .takes("PACKAGES", T::Required)
        .about("Repository packages are installed directly. AUR packages are fetched, shown to you for review, built as your user in a sandbox, and installed through a libalpm transaction that you confirm separately. An installed package or a repository package satisfies a dependency before the AUR is consulted.")
        .examples(&["zap install ripgrep", "zap install google-chrome --dry-run", "zap install some-library --asdeps --needed"]),
    s(C::Upgrade, "upgrade", G::Everyday, "Refresh and upgrade repositories, then AUR packages")
        .alias("-Syu")
        .about("Refreshes the repository databases and runs a full repository upgrade, then rebuilds installed AUR packages that have a newer version. Existing installation reasons are preserved. With --aur the repositories are neither refreshed nor upgraded. Read zap news first: some upgrades need manual intervention.")
        .examples(&["zap upgrade", "zap upgrade --devel", "zap upgrade --aur --dry-run"]),
    s(C::Updates, "updates", G::Everyday, "List available repository and AUR updates")
        .alias("-Qu")
        .about("Compares installed versions against the cached repository databases and live AUR metadata without changing anything. The repository databases are only as fresh as the last upgrade.")
        .examples(&["zap updates", "zap updates --aur --devel", "zap updates --quiet"]),
    s(C::Remove, "remove", G::Everyday, "Remove packages with dependency checks")
        .alias("-R")
        .takes("PACKAGES", T::Required)
        .about("Removes installed packages through a libalpm transaction. Packages that others still require cannot be removed, and HoldPkg entries are protected. -Rns is shorthand for --recursive --nosave.")
        .examples(&["zap remove some-package", "zap remove some-package --recursive", "zap remove some-package --dry-run"]),
    s(C::Info, "info", G::Inspect, "Show available package details")
        .alias("-Si")
        .takes("PACKAGES", T::Required)
        .about("Shows versions, sizes, dependencies, licenses and dates for packages in the repositories, falling back to the AUR for names the repositories do not provide.")
        .examples(&["zap info ripgrep", "zap info yay --aur"]),
    s(C::Localinfo, "localinfo", G::Inspect, "Show installed package details")
        .alias("-Qi")
        .takes("PACKAGES", T::Required)
        .about("Shows the installed version, installation reason and date, and which installed packages require or optionally use it.")
        .examples(&["zap localinfo pacman"]),
    s(C::List, "list", G::Inspect, "List installed packages")
        .alias("-Q")
        .takes("[PACKAGES]", T::Optional)
        .about("Lists every installed package, or only the named ones, with its version, installation reason and installation date.")
        .examples(&["zap list", "zap list pacman zap", "zap list --quiet"]),
    s(C::LocalSearch, "local-search", G::Inspect, "Search installed names and descriptions")
        .alias("-Qs")
        .takes("TERM", T::Term)
        .examples(&["zap local-search font"]),
    s(C::Foreign, "foreign", G::Inspect, "List packages absent from the repositories")
        .alias("-Qm")
        .takes("[PACKAGES]", T::Optional)
        .about("Lists installed packages that no configured repository provides: usually AUR packages and locally built ones.")
        .examples(&["zap foreign"]),
    s(C::Files, "files", G::Inspect, "List the files a package installed")
        .alias("-Ql")
        .takes("PACKAGES", T::Required)
        .examples(&["zap files ripgrep", "zap files ripgrep --quiet"]),
    s(C::Owns, "owns", G::Inspect, "Find which package owns a file")
        .alias("-Qo")
        .takes("FILES", T::Required)
        .about("Directory aliases such as /bin are resolved before matching. Exits with a failure status when any file has no owner.")
        .examples(&["zap owns /usr/bin/rg"]),
    s(C::Check, "check", G::Inspect, "Check for missing installed files")
        .alias("-Qk")
        .takes("[PACKAGES]", T::Optional)
        .about("Checks that every file a package installed still exists. This detects missing files only; contents and checksums are not verified. Exits with a failure status when any file is missing.")
        .examples(&["zap check", "zap check pacman"]),
    s(C::Stats, "stats", G::Inspect, "Show package counts, disk usage and the largest packages").alias("-Ps"),
    s(C::News, "news", G::Inspect, "Read Arch Linux news before upgrading")
        .alias("-Pw")
        .about("Shows the latest Arch Linux news items. Some upgrades need manual intervention, announced there first."),
    s(C::Build, "build", G::Build, "Review and build AUR packages without installing")
        .takes("PACKAGES", T::Required)
        .about("Builds without changing the system, so every dependency must already be installed. The built archives stay in the build cache.")
        .examples(&["zap build some-aur-package"]),
    s(C::BuildLocal, "build-local", G::Build, "Build local PKGBUILD directories")
        .alias("-B")
        .takes("DIRS", T::Required)
        .about("Copies each directory into a fresh snapshot in the build cache, then reviews and builds the snapshot; your checkout is never sourced or modified. Each directory needs a PKGBUILD and a matching .SRCINFO. Git metadata, top-level src and pkg trees and package archives are not copied; links and special files are refused.")
        .examples(&["zap build-local ."]),
    s(C::InstallLocal, "install-local", G::Build, "Build and install local PKGBUILD directories")
        .alias("-Bi")
        .takes("DIRS", T::Required)
        .about("Like build-local, then resolves dependencies and installs the result. A supplied directory takes precedence over an installed, repository or AUR package of the same name.")
        .examples(&["zap install-local .", "zap install-local ./libfoo ./app"]),
    s(C::InstallFiles, "install-files", G::Build, "Install local package archives")
        .alias("-U")
        .takes("FILES", T::Required)
        .about("Reads each archive's metadata and SHA-256 digest without executing anything. The root worker then copies the archives to private staging, verifies the digests you approved, applies the local signature policy and installs the copies.")
        .examples(&["zap install-files ./some-package-1.0-1-x86_64.pkg.tar.zst"]),
    s(C::Get, "get", G::Build, "Fetch AUR sources without executing them")
        .alias("-G")
        .takes("PACKAGES", T::Required)
        .examples(&["zap get some-aur-package"]),
    s(C::Pkgbuild, "pkgbuild", G::Build, "Print AUR PKGBUILDs without executing them")
        .alias("-Gp")
        .takes("PACKAGES", T::Required)
        .examples(&["zap pkgbuild some-aur-package"]),
    s(C::Orphans, "orphans", G::Maintain, "List dependencies nothing needs any more")
        .alias("-Qdt")
        .about("Lists packages installed as dependencies that no installed package requires or optionally uses.")
        .examples(&["zap orphans", "zap orphans --quiet"]),
    s(C::Autoremove, "autoremove", G::Maintain, "Review and remove orphan dependencies")
        .examples(&["zap autoremove --dry-run", "zap autoremove"]),
    s(C::Clean, "clean", G::Maintain, "Clear cached sources and build outputs")
        .alias("-Sc")
        .takes("[BASES]", T::Optional)
        .about("Clears the whole build cache, or only the named package bases, after confirmation. The pacman package cache is not touched.")
        .examples(&["zap clean", "zap clean some-package-base"]),
    s(C::Reason, "reason", G::Maintain, "Mark packages as explicit or as dependencies")
        .alias("-D")
        .takes("PACKAGES", T::Required)
        .about("Changes why a package counts as installed. Dependencies nothing requires are listed by zap orphans. Needs --asdeps or --asexplicit.")
        .examples(&["zap reason some-library --asdeps", "zap reason some-tool --asexplicit"]),
    s(C::Help, "help", G::About, "Show this overview, or one command in detail")
        .alias("-h")
        .takes("[COMMAND]", T::Topic)
        .examples(&["zap help install", "zap install --help"]),
    s(C::Version, "version", G::About, "Show the zap, libalpm and Rust versions").alias("--version"),
];

pub fn spec(command: Command) -> &'static Spec {
    SPECS.iter().find(|entry| entry.command == command).expect("every command has a spec")
}

#[derive(Clone, Copy)]
struct Alias {
    text: &'static str,
    command: Command,
    scope: Scope,
    quiet: bool,
    purge: bool,
}

const fn alias(text: &'static str, command: Command) -> Alias {
    Alias { text, command, scope: Scope::All, quiet: false, purge: false }
}

// Pacman-style spellings, including the combined forms people type from habit.
const ALIASES: [Alias; 8] = [
    alias("--help", C::Help),
    alias("-V", C::Version),
    Alias { scope: Scope::Aur, ..alias("-Sua", C::Upgrade) },
    Alias { scope: Scope::Aur, ..alias("-Qua", C::Updates) },
    Alias { quiet: true, ..alias("-Qq", C::List) },
    Alias { quiet: true, ..alias("-Qmq", C::Foreign) },
    Alias { quiet: true, ..alias("-Qdtq", C::Orphans) },
    Alias { purge: true, ..alias("-Rns", C::Remove) },
];

fn resolve(text: &str) -> Option<(Command, Option<Alias>)> {
    for entry in &SPECS {
        if text == entry.name || text == entry.command.tag() || (!entry.alias.is_empty() && text == entry.alias) {
            return Some((entry.command, None));
        }
    }
    ALIASES.iter().find(|a| a.text == text).map(|a| (a.command, Some(*a)))
}

// ── Options ─────────────────────────────────────────────────────────────────

/// A set of commands, one bit each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandSet(u32);

impl CommandSet {
    pub const fn of(list: &[Command]) -> CommandSet {
        let mut bits = 0;
        let mut i = 0;
        while i < list.len() {
            bits |= 1 << list[i] as u32;
            i += 1;
        }
        CommandSet(bits)
    }
    pub const fn union(self, other: CommandSet) -> CommandSet {
        CommandSet(self.0 | other.0)
    }
    pub const fn contains(self, command: Command) -> bool {
        self.0 & (1 << command as u32) != 0
    }
}

const EVERY: CommandSet = CommandSet(u32::MAX);
const BUILDS: CommandSet = CommandSet::of(&[C::Build, C::Install, C::Select, C::Upgrade, C::BuildLocal, C::InstallLocal]);
const SCOPED: CommandSet = CommandSet::of(&[C::Search, C::Select, C::Info, C::Install, C::Upgrade, C::Updates]);
const SEARCHES: CommandSet = CommandSet::of(&[C::Search, C::Select]);
pub const ELEVATING: CommandSet = CommandSet::of(&[
    C::Install,
    C::Select,
    C::Upgrade,
    C::InstallLocal,
    C::InstallFiles,
    C::Remove,
    C::Reason,
    C::Autoremove,
]);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    DryRun,
    Needed,
    Asdeps,
    Asexplicit,
    Aur,
    Repo,
    Devel,
    Recursive,
    Nosave,
    Sandbox,
    NoSandbox,
    Rebuildtree,
    Cleanafter,
    Searchby,
    Sortby,
    Quiet,
    Elevate,
    Noconfirm,
    Color,
    Help,
}

pub struct OptionSpec {
    pub flag: Flag,
    pub long: &'static str,
    pub short: &'static str,
    pub value: &'static str,
    pub summary: &'static str,
    pub commands: CommandSet,
}

const fn o(flag: Flag, long: &'static str, summary: &'static str, commands: CommandSet) -> OptionSpec {
    OptionSpec { flag, long, short: "", value: "", summary, commands }
}

pub static OPTIONS: [OptionSpec; 20] = [
    o(Flag::DryRun, "--dry-run", "Show the plan without building, elevating or changing the system", BUILDS.union(CommandSet::of(&[C::InstallFiles, C::Remove, C::Autoremove, C::Clean, C::Reason]))),
    o(Flag::Needed, "--needed", "Skip targets that are already installed and current", CommandSet::of(&[C::Install, C::InstallFiles, C::Select])),
    o(Flag::Asdeps, "--asdeps", "Mark the requested packages as dependencies", CommandSet::of(&[C::Install, C::InstallFiles, C::InstallLocal, C::Select, C::Reason])),
    o(Flag::Asexplicit, "--asexplicit", "Mark the requested packages as explicitly installed", CommandSet::of(&[C::Install, C::InstallFiles, C::InstallLocal, C::Select, C::Reason])),
    OptionSpec { short: "-a", ..o(Flag::Aur, "--aur", "Only consider the AUR for the requested packages", SCOPED) },
    o(Flag::Repo, "--repo", "Only consider repositories for the requested packages", SCOPED),
    o(Flag::Devel, "--devel", "Also rebuild installed VCS packages such as -git", CommandSet::of(&[C::Updates, C::Upgrade])),
    o(Flag::Recursive, "--recursive", "Also remove dependencies nothing else needs", CommandSet::of(&[C::Remove])),
    o(Flag::Nosave, "--nosave", "Delete configuration files instead of keeping .pacsave copies", CommandSet::of(&[C::Remove])),
    o(Flag::Sandbox, "--sandbox", "Isolate every build phase with bubblewrap (default)", BUILDS),
    o(Flag::NoSandbox, "--no-sandbox", "Run reviewed build code with your user's permissions", BUILDS),
    o(Flag::Rebuildtree, "--rebuildtree", "Also rebuild installed AUR dependencies", BUILDS),
    o(Flag::Cleanafter, "--cleanafter", "Delete untracked build files after installing", CommandSet::of(&[C::Install, C::Select, C::Upgrade, C::InstallLocal])),
    OptionSpec { value: "FIELD", ..o(Flag::Searchby, "--searchby", "AUR field to match: name, name-desc (default), maintainer, provides, depends, makedepends, checkdepends, optdepends", SEARCHES) },
    OptionSpec { value: "ORDER", ..o(Flag::Sortby, "--sortby", "AUR result order: votes (default), popularity, name, modified", SEARCHES) },
    OptionSpec { short: "-q", ..o(Flag::Quiet, "--quiet", "Print bare names (paths for files), one per line", CommandSet::of(&[C::Search, C::LocalSearch, C::Updates, C::List, C::Foreign, C::Orphans, C::Files, C::Owns])) },
    OptionSpec { value: "TOOL", ..o(Flag::Elevate, "--elevate", "Gain root with run0 or sudo; default $ZAP_ELEVATE, else run0 with systemd and polkit, else sudo", ELEVATING) },
    o(Flag::Noconfirm, "--noconfirm", "Run unattended: approve build files and the transaction without asking; root still needs run0 or sudo to authorize", CommandSet::of(&[C::Install, C::Upgrade, C::InstallLocal, C::InstallFiles])),
    OptionSpec { value: "WHEN", ..o(Flag::Color, "--color", "Style output: auto (default), always or never", EVERY) },
    OptionSpec { short: "-h", ..o(Flag::Help, "--help", "Show help for the command", EVERY) },
];

fn option(text: &str) -> Option<&'static OptionSpec> {
    OPTIONS.iter().find(|e| text == e.long || (!e.short.is_empty() && text == e.short))
}

// ── Parsing ─────────────────────────────────────────────────────────────────

/// The --color choice, found before parsing so that a failed parse and an
/// early --help honour it too. Invalid values are left for `parse` to report.
pub fn color_preference<S: AsRef<str>>(args: &[S]) -> Option<ColorMode> {
    let mut choice = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_ref();
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--color=") {
            choice = ColorMode::parse(value).or(choice);
        } else if arg == "--color" && i + 1 < args.len() {
            choice = ColorMode::parse(args[i + 1].as_ref()).or(choice);
            i += 1;
        }
        i += 1;
    }
    choice
}

pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Options> {
    let args: Vec<&str> = args.iter().map(|a| a.as_ref()).collect();
    if args.is_empty() {
        return Ok(Options::new(C::Help));
    }
    let Some((command, alias)) = resolve(args[0]) else { return Err(unknown_command(args[0])) };
    // Asking for help never fails on what else was typed.
    if command != C::Help {
        for arg in &args[1..] {
            if *arg == "--" {
                break;
            }
            if *arg == "--help" || *arg == "-h" {
                return Ok(Options { topic: Some(command), ..Options::new(C::Help) });
            }
        }
    }
    let mut o = Options::new(command);
    if let Some(alias) = alias {
        o.scope = alias.scope;
        o.quiet = alias.quiet;
        o.recursive = alias.purge;
        o.nosave = alias.purge;
    }
    let mut positional = false;
    let mut i = 1;
    while i < args.len() {
        let arg = args[i];
        i += 1;
        if !positional && arg == "--" {
            positional = true;
            continue;
        }
        // `zap help -S` names a command by its alias; it is not an option.
        let topic = o.command == C::Help && option(arg).is_none() && resolve(arg).is_some();
        if positional || topic || !arg.starts_with('-') {
            o.names.push(arg.to_string());
            continue;
        }
        let equals = if arg.starts_with("--") { arg.find('=') } else { None };
        let key = &arg[..equals.unwrap_or(arg.len())];
        let Some(entry) = option(key) else { return Err(unknown_option(key)) };
        if entry.flag == Flag::Help {
            let topic = if o.command == C::Help { None } else { Some(o.command) };
            return Ok(Options { topic, color: o.color, ..Options::new(C::Help) });
        }
        if !entry.commands.contains(o.command) {
            return Err(errors::with(Kind::OptionNotValidForCommand, format!("{} with {}", entry.long, spec(o.command).name)));
        }
        let mut value = "";
        if !entry.value.is_empty() {
            if let Some(at) = equals {
                value = &arg[at + 1..];
            } else {
                if i == args.len() {
                    return Err(errors::with(Kind::OptionValueRequired, format!("{} {}", entry.long, entry.value)));
                }
                value = args[i];
                i += 1;
            }
        } else if equals.is_some() {
            return Err(errors::with(Kind::OptionValueNotAllowed, entry.long));
        }
        apply(&mut o, entry.flag, value)?;
    }
    validate(&mut o)?;
    Ok(o)
}

fn conflicting(what: &str) -> errors::Error {
    errors::with(Kind::ConflictingOptions, what)
}

fn apply(o: &mut Options, flag: Flag, value: &str) -> Result<()> {
    match flag {
        Flag::DryRun => o.dry = true,
        Flag::Needed => o.needed = true,
        Flag::Devel => o.devel = true,
        Flag::Recursive => o.recursive = true,
        Flag::Nosave => o.nosave = true,
        Flag::Quiet => o.quiet = true,
        Flag::Noconfirm => o.noconfirm = true,
        Flag::Rebuildtree => o.rebuild_tree = true,
        Flag::Cleanafter => o.clean_after = true,
        Flag::Sandbox | Flag::NoSandbox => {
            let enabled = flag == Flag::Sandbox;
            if o.sandbox.is_some_and(|s| s != enabled) {
                return Err(conflicting("--sandbox and --no-sandbox"));
            }
            o.sandbox = Some(enabled);
        }
        Flag::Aur | Flag::Repo => {
            let scope = if flag == Flag::Aur { Scope::Aur } else { Scope::Repo };
            if o.scope != Scope::All && o.scope != scope {
                return Err(conflicting("--aur and --repo"));
            }
            o.scope = scope;
        }
        Flag::Asdeps | Flag::Asexplicit => {
            let reason = if flag == Flag::Asdeps { Reason::Dependency } else { Reason::Explicit };
            if o.reason.is_some_and(|r| r != reason) {
                return Err(conflicting("--asdeps and --asexplicit"));
            }
            o.reason = Some(reason);
        }
        Flag::Searchby => o.search_by = SearchBy::parse(value).ok_or_else(|| errors::with(Kind::InvalidSearchField, value))?,
        Flag::Sortby => o.sort_by = SortBy::parse(value).ok_or_else(|| errors::with(Kind::InvalidSortField, value))?,
        Flag::Color => o.color = ColorMode::parse(value).ok_or_else(|| errors::with(Kind::InvalidColorMode, value))?,
        Flag::Elevate => o.elevate = Some(Elevator::parse(value).ok_or_else(|| errors::with(Kind::InvalidElevator, value))?),
        Flag::Help => unreachable!(),
    }
    Ok(())
}

/// Rules that involve more than one option, and the targets a command takes.
fn validate(o: &mut Options) -> Result<()> {
    if o.command == C::Reason && o.reason.is_none() {
        return errors::fail(Kind::InstallationReasonRequired);
    }
    if o.needed && o.rebuild_tree {
        return Err(conflicting("--needed and --rebuildtree"));
    }
    let entry = spec(o.command);
    match entry.targets {
        T::None if !o.names.is_empty() => return Err(errors::with(Kind::UnexpectedTargets, entry.name)),
        T::Term if o.names.is_empty() => return errors::fail(Kind::SearchTermRequired),
        T::Required if o.names.is_empty() => {
            let hint = format!("Usage: zap {} {}", entry.name, entry.operands);
            return Err(errors::with_hint(Kind::PackageNameRequired, hint, format!("zap {}", entry.name)));
        }
        T::Topic => {
            if o.names.len() > 1 {
                return Err(errors::with(Kind::UnexpectedTargets, "help takes one command"));
            }
            if let Some(name) = o.names.first() {
                o.topic = Some(resolve(name).ok_or_else(|| unknown_command(name))?.0);
            }
        }
        _ => {}
    }
    Ok(())
}

fn unknown_command(text: &str) -> errors::Error {
    if text.starts_with('-') {
        return errors::with_hint(Kind::UnknownCommand, "Options go after the command: zap COMMAND [OPTIONS].", text);
    }
    let names: Vec<&str> = SPECS.iter().map(|e| e.name).collect();
    match nearest(text, &names) {
        Some(m) => errors::with_hint(Kind::UnknownCommand, format!("Did you mean zap {m}?"), text),
        None => errors::with(Kind::UnknownCommand, text),
    }
}

fn unknown_option(text: &str) -> errors::Error {
    let names: Vec<&str> = OPTIONS.iter().map(|e| e.long).collect();
    match nearest(text, &names) {
        Some(m) => errors::with_hint(Kind::UnknownOption, format!("Did you mean {m}?"), text),
        None => errors::with(Kind::UnknownOption, text),
    }
}

/// The candidate a typo most plausibly meant: a unique prefix, or the single
/// closest spelling within a small edit distance.
pub fn nearest<'a>(input: &str, candidates: &[&'a str]) -> Option<&'a str> {
    if input.len() < 2 || input.len() > 32 {
        return None;
    }
    let prefixed: Vec<&str> = candidates.iter().copied().filter(|c| c.starts_with(input)).collect();
    if prefixed.len() == 1 {
        return Some(prefixed[0]);
    }
    let limit = if input.len() <= 4 { 1 } else { 2 };
    let mut best = None;
    let mut best_distance = limit + 1;
    let mut tied = false;
    for candidate in candidates {
        let d = distance(input.as_bytes(), candidate.as_bytes());
        if d < best_distance {
            best = Some(*candidate);
            best_distance = d;
            tied = false;
        } else if d == best_distance {
            tied = true;
        }
    }
    if tied {
        None
    } else {
        best
    }
}

/// Levenshtein distance, for the short strings of a command line.
fn distance(a: &[u8], b: &[u8]) -> usize {
    if b.len() >= 65 {
        return usize::MAX;
    }
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(ca != cb);
            current[j + 1] = substitution.min(previous[j + 1].min(current[j]) + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(result: Result<Options>) -> Kind {
        result.expect_err("parse should fail").kind
    }

    #[test]
    fn cli_scopes_aliases_and_contradictory_or_ignored_flags() {
        let scoped_upgrade = parse(&["-Sua", "--devel", "--dry-run"]).unwrap();
        assert_eq!(Scope::Aur, scoped_upgrade.scope);
        assert!(scoped_upgrade.dry && scoped_upgrade.devel);
        assert!(parse(&["-Qmq"]).unwrap().quiet);
        assert_eq!(Kind::ConflictingOptions, kind(parse(&["install", "foo", "--asdeps", "--asexplicit"])));
        assert_eq!(Kind::ConflictingOptions, kind(parse(&["-Sua", "--repo"])));
        for args in [
            &["get", "foo", "--dry-run"][..],
            &["upgrade", "--asdeps"],
            &["search", "foo", "--sandbox"],
            &["info", "foo", "--sortby", "votes"],
            &["build", "foo", "--cleanafter"],
            &["install-local", ".", "--needed"],
        ] {
            assert_eq!(Kind::OptionNotValidForCommand, kind(parse(args)), "{args:?}");
        }
        assert_eq!(Kind::ConflictingOptions, kind(parse(&["install", "foo", "--needed", "--rebuildtree"])));
        assert_eq!(C::InstallLocal, parse(&["-Bi", ".", "--sandbox"]).unwrap().command);
        assert_eq!(Kind::OptionValueRequired, kind(parse(&["search", "foo", "--sortby"])));
        assert!(parse(&["upgrade", "--noconfirm"]).unwrap().noconfirm);
        assert_eq!(Kind::OptionNotValidForCommand, kind(parse(&["remove", "foo", "--noconfirm"])));
        assert_eq!(Kind::OptionNotValidForCommand, kind(parse(&["select", "foo", "--noconfirm"])));
        assert_eq!(Kind::InvalidSearchField, kind(parse(&["search", "foo", "--searchby", "name&arg=evil"])));
        assert_eq!("-archive.pkg.tar", parse(&["-U", "--", "-archive.pkg.tar"]).unwrap().names[0]);
    }

    #[test]
    fn every_command_resolves_by_name_and_alias_and_takes_the_targets_it_documents() {
        for entry in &SPECS {
            assert_eq!(entry.command, resolve(entry.name).unwrap().0);
            assert_eq!(entry.command, resolve(entry.command.tag()).unwrap().0);
            if !entry.alias.is_empty() {
                assert_eq!(entry.command, resolve(entry.alias).unwrap().0);
            }
            assert!(!entry.summary.is_empty() && entry.summary.len() <= 60);
            // Operands shown in help agree with what the parser enforces.
            assert_eq!(entry.targets == T::None, entry.operands.is_empty());
            assert_eq!(matches!(entry.targets, T::Optional | T::Topic), entry.operands.starts_with('['));
        }
        // Every command is documented exactly once.
        for command in Command::ALL {
            assert_eq!(1, SPECS.iter().filter(|e| e.command == *command).count());
        }
        let purge = parse(&["-Rns", "foo"]).unwrap();
        assert!(purge.recursive && purge.nosave);
        assert_eq!(Kind::UnexpectedTargets, kind(parse(&["upgrade", "foo"])));
        assert_eq!(Kind::SearchTermRequired, kind(parse(&["search"])));
        assert_eq!(Kind::PackageNameRequired, kind(parse(&["install"])));
        assert_eq!(Kind::InstallationReasonRequired, kind(parse(&["reason", "foo"])));
        assert!(parse(&["list"]).unwrap().names.is_empty());
    }

    #[test]
    fn option_values_color_modes_and_help_requests() {
        let sorted =
            parse(&["search", "foo", "--sortby=popularity", "--searchby", "maintainer", "--color=never"]).unwrap();
        assert_eq!(SortBy::Popularity, sorted.sort_by);
        assert_eq!(SearchBy::Maintainer, sorted.search_by);
        assert_eq!(ColorMode::Never, sorted.color);
        assert_eq!(Kind::InvalidColorMode, kind(parse(&["list", "--color", "sometimes"])));
        assert_eq!(Kind::InvalidSortField, kind(parse(&["search", "foo", "--sortby="])));
        assert_eq!(Kind::OptionValueNotAllowed, kind(parse(&["install", "foo", "--dry-run=yes"])));
        // A target containing '=' is not mistaken for an option value.
        assert_eq!("a=b", parse(&["owns", "a=b"]).unwrap().names[0]);

        assert_eq!(None, parse::<&str>(&[]).unwrap().topic);
        assert_eq!(None, parse(&["--help"]).unwrap().topic);
        assert_eq!(Some(C::Install), parse(&["help", "install"]).unwrap().topic);
        assert_eq!(Some(C::Install), parse(&["help", "-S"]).unwrap().topic);
        // Asking for help never fails on what else was typed.
        let asked = parse(&["remove", "--help"]).unwrap();
        assert_eq!(C::Help, asked.command);
        assert_eq!(Some(C::Remove), asked.topic);
        assert_eq!(Some(C::InstallFiles), parse(&["-U", "-h"]).unwrap().topic);
        assert_eq!(Some(C::Install), parse(&["install", "--bogus", "--sortby", "--help"]).unwrap().topic);
        assert_eq!("--help", parse(&["owns", "--", "--help"]).unwrap().names[0]);
        assert_eq!(Kind::UnknownCommand, kind(parse(&["help", "nonsense"])));
        assert_eq!(Kind::UnexpectedTargets, kind(parse(&["help", "install", "remove"])));
    }

    #[test]
    fn a_color_preference_is_honoured_even_when_the_rest_of_the_line_fails_or_asks_for_help() {
        assert_eq!(Some(ColorMode::Never), color_preference(&["list", "--color=never", "--bogus"]));
        assert_eq!(Some(ColorMode::Always), color_preference(&["install", "--help", "--color", "always"]));
        assert_eq!(None, color_preference(&["owns", "--", "--color=never"]));
        assert_eq!(None, color_preference(&["list", "--color=sometimes"]));
    }

    #[test]
    fn typos_suggest_the_nearest_command_or_option_and_only_when_unambiguous() {
        let names: Vec<&str> = SPECS.iter().map(|e| e.name).collect();
        assert_eq!(Some("install"), nearest("instal", &names));
        assert_eq!(Some("search"), nearest("serach", &names));
        assert_eq!(Some("autoremove"), nearest("auto", &names));
        assert_eq!(Some("upgrade"), nearest("upgrad", &names));
        assert_eq!(None, nearest("up", &names));
        assert_eq!(None, nearest("xyzzy", &names));
        assert_eq!(None, nearest("i", &names));
        assert_eq!(1, distance(b"--sorby", b"--sortby"));
        assert_eq!(3, distance(b"kitten", b"sitting"));
        assert_eq!(Kind::UnknownCommand, kind(parse(&["instal"])));
    }

    #[test]
    fn each_option_is_accepted_exactly_where_its_table_says() {
        for entry in OPTIONS.iter().filter(|e| e.flag != Flag::Help) {
            for command in SPECS.iter().filter(|c| c.command != C::Help) {
                let mut args = vec![command.name];
                if matches!(command.targets, T::Term | T::Required) {
                    args.push("target");
                }
                // reason needs a reason regardless of the option under test.
                if command.command == C::Reason && !matches!(entry.flag, Flag::Asdeps | Flag::Asexplicit) {
                    args.push("--asdeps");
                }
                args.push(entry.long);
                if !entry.value.is_empty() {
                    args.push(match entry.flag {
                        Flag::Searchby | Flag::Sortby => "name",
                        Flag::Elevate => "sudo",
                        _ => "never",
                    });
                }
                let result = parse(&args);
                if entry.commands.contains(command.command) {
                    result.unwrap_or_else(|e| panic!("{args:?}: {e}"));
                } else {
                    assert_eq!(Kind::OptionNotValidForCommand, kind(result), "{args:?}");
                }
            }
        }
    }
}
