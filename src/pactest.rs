//! A pacman-compatible front end over zap's libalpm port, for running
//! pacman's own pactest suite against the engine. It covers the operations
//! zap's transaction worker uses (-S, -U, -R and -D); queries and other
//! operations exit with status 2 so the harness can tell them apart.
//!
//! Build with `cargo build --release --features pactest --bin zap-pactest`.

// Engine code and re-exports this front end does not need are used by zap
// itself; see the matching allowance in src/main.rs.
#[allow(dead_code, unused_imports)]
#[path = "libalpm/mod.rs"]
mod libalpm;

use libalpm::conflict::FileConflictKind;
use libalpm::log::{Event, Frontend, Level, Question};
use libalpm::trans::*;
use libalpm::{Depend, Error, ErrorKind, Handle, Pkg, Reason};
use std::sync::Arc;

#[derive(Default, PartialEq, Clone, Copy)]
enum Op {
    #[default]
    None,
    Sync,
    Upgrade,
    Remove,
    Database,
    Unsupported,
}

#[derive(Default)]
struct Config {
    op: Op,
    config: String,
    root: Option<String>,
    dbpath: Option<String>,
    cachedirs: Vec<String>,
    hookdirs: Vec<String>,
    logfile: Option<String>,
    gpgdir: Option<String>,
    debug: bool,
    ask: Option<u32>,
    flags: u32,
    nodeps: u32,
    sysupgrade: u32,
    refresh: u32,
    asdeps: bool,
    asexplicit: bool,
    ignore: Vec<String>,
    ignoregroup: Vec<String>,
    assume_installed: Vec<String>,
    overwrite: Vec<String>,
    print: bool,
    targets: Vec<String>,
    // From the configuration file.
    architectures: Vec<String>,
    holdpkg: Vec<String>,
    noupgrade: Vec<String>,
    noextract: Vec<String>,
    siglevel: i32,
    local_siglevel: Option<i32>,
    remote_siglevel: Option<i32>,
    checkspace: bool,
    repos: Vec<Repo>,
}

struct Repo {
    name: String,
    servers: Vec<String>,
    cache_servers: Vec<String>,
    siglevel: Option<i32>,
    usage: u32,
}

fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
}

/// Parse pacman's command line: clustered short options and long options
/// with `=value` or a separate value.
fn parse_args(args: &[String]) -> Config {
    let mut c = Config { config: "/etc/pacman.conf".into(), ..Config::default() };
    let with_value = [
        "config", "root", "dbpath", "cachedir", "hookdir", "logfile", "gpgdir", "ask", "ignore", "ignoregroup",
        "assume-installed", "overwrite", "print-format", "arch", "color", "sysroot",
    ];
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        i += 1;
        if arg == "--" {
            c.targets.extend(args[i..].iter().cloned());
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, inline) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            let mut value = || -> String {
                inline.clone().unwrap_or_else(|| {
                    i += 1;
                    args.get(i - 1).cloned().unwrap_or_default()
                })
            };
            let value = if with_value.contains(&name) { value() } else { inline.clone().unwrap_or_default() };
            match name {
                "noconfirm" | "confirm" | "noprogressbar" | "disable-download-timeout" | "disable-sandbox" => {}
                "config" => c.config = value,
                "root" => c.root = Some(value),
                "dbpath" => c.dbpath = Some(value),
                "cachedir" => c.cachedirs.push(value),
                "hookdir" => c.hookdirs.push(value),
                "logfile" => c.logfile = Some(value),
                "gpgdir" => c.gpgdir = Some(value),
                "arch" => c.architectures = vec![value],
                "debug" => c.debug = true,
                "ask" => c.ask = value.parse().ok(),
                "needed" => c.flags |= FLAG_NEEDED,
                "asdeps" => c.asdeps = true,
                "asexplicit" => c.asexplicit = true,
                "ignore" => c.ignore.extend(value.split(',').map(String::from)),
                "ignoregroup" => c.ignoregroup.extend(value.split(',').map(String::from)),
                "assume-installed" => c.assume_installed.push(value),
                "overwrite" => c.overwrite.push(value),
                "dbonly" => c.flags |= FLAG_DBONLY,
                "noscriptlet" => c.flags |= FLAG_NOSCRIPTLET,
                "nodeps" => c.nodeps += 1,
                "print" | "print-format" => c.print = true,
                "recursive" => c.flags |= FLAG_RECURSE,
                "nosave" => c.flags |= FLAG_NOSAVE,
                "cascade" => c.flags |= FLAG_CASCADE,
                "unneeded" => c.flags |= FLAG_UNNEEDED,
                "sysupgrade" => c.sysupgrade += 1,
                "refresh" => c.refresh += 1,
                "downloadonly" => c.flags |= FLAG_DOWNLOADONLY,
                "sync" => c.op = Op::Sync,
                "upgrade" => c.op = Op::Upgrade,
                "remove" => c.op = Op::Remove,
                "database" => c.op = Op::Database,
                _ => c.op = Op::Unsupported,
            }
            continue;
        }
        if let Some(short) = arg.strip_prefix('-').filter(|s| !s.is_empty()) {
            for ch in short.chars() {
                match ch {
                    'S' => c.op = Op::Sync,
                    'U' => c.op = Op::Upgrade,
                    'R' => c.op = Op::Remove,
                    'D' => c.op = Op::Database,
                    'Q' | 'F' | 'T' | 'V' | 'h' => c.op = Op::Unsupported,
                    'd' => c.nodeps += 1,
                    'p' => c.print = true,
                    'v' => {}
                    'u' if c.op == Op::Sync => c.sysupgrade += 1,
                    'u' if c.op == Op::Remove => c.flags |= FLAG_UNNEEDED,
                    'y' => c.refresh += 1,
                    'w' => c.flags |= FLAG_DOWNLOADONLY,
                    's' if c.op == Op::Remove => {
                        if c.flags & FLAG_RECURSE != 0 {
                            c.flags |= FLAG_RECURSEALL;
                        }
                        c.flags |= FLAG_RECURSE;
                    }
                    'n' if c.op == Op::Remove => c.flags |= FLAG_NOSAVE,
                    'c' if c.op == Op::Remove => c.flags |= FLAG_CASCADE,
                    'r' => {
                        c.root = Some(args.get(i).cloned().unwrap_or_default());
                        i += 1;
                    }
                    'b' => {
                        c.dbpath = Some(args.get(i).cloned().unwrap_or_default());
                        i += 1;
                    }
                    _ => c.op = Op::Unsupported,
                }
            }
            continue;
        }
        c.targets.push(arg.clone());
    }
    if c.print {
        c.flags |= FLAG_NOCONFLICTS | FLAG_NOLOCK;
    }
    if c.nodeps > 0 {
        c.flags |= FLAG_NODEPVERSION;
    }
    if c.nodeps > 1 {
        c.flags |= FLAG_NODEPS;
    }
    c
}

/// `SigLevel` tokens, as pacman's conf.c.
fn sig_level(tokens: &[&str], mut level: i32) -> i32 {
    use libalpm::*;
    for token in tokens {
        let (pkg, db, word) = if let Some(w) = token.strip_prefix("Package") {
            (true, false, w)
        } else if let Some(w) = token.strip_prefix("Database") {
            (false, true, w)
        } else {
            (true, true, *token)
        };
        let set = |level: &mut i32, p: i32, d: i32, on: bool| {
            let mask = (if pkg { p } else { 0 }) | (if db { d } else { 0 });
            if on {
                *level |= mask
            } else {
                *level &= !mask
            }
        };
        match word {
            "Never" => set(&mut level, SIG_PACKAGE | SIG_PACKAGE_OPTIONAL, SIG_DATABASE | SIG_DATABASE_OPTIONAL, false),
            "Optional" => set(&mut level, SIG_PACKAGE | SIG_PACKAGE_OPTIONAL, SIG_DATABASE | SIG_DATABASE_OPTIONAL, true),
            "Required" => {
                set(&mut level, SIG_PACKAGE, SIG_DATABASE, true);
                set(&mut level, SIG_PACKAGE_OPTIONAL, SIG_DATABASE_OPTIONAL, false);
            }
            "TrustedOnly" => set(&mut level, SIG_PACKAGE_MARGINAL_OK | SIG_PACKAGE_UNKNOWN_OK, SIG_DATABASE_MARGINAL_OK | SIG_DATABASE_UNKNOWN_OK, false),
            "TrustAll" => set(&mut level, SIG_PACKAGE_MARGINAL_OK | SIG_PACKAGE_UNKNOWN_OK, SIG_DATABASE_MARGINAL_OK | SIG_DATABASE_UNKNOWN_OK, true),
            _ => {}
        }
    }
    level
}

/// The subset of pacman.conf that pactest writes.
fn parse_config(c: &mut Config) {
    let text = std::fs::read_to_string(&c.config).unwrap_or_else(|_| fail(&format!("config file {} could not be read", c.config)));
    let mut section = String::new();
    let mut siglevel = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.to_string();
            if section != "options" {
                c.repos.push(Repo { name: section.clone(), servers: Vec::new(), cache_servers: Vec::new(), siglevel: None, usage: libalpm::db::USAGE_ALL });
            }
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (line, ""),
        };
        let words: Vec<String> = value.split_whitespace().map(String::from).collect();
        if section == "options" {
            match key {
                "RootDir" if c.root.is_none() => c.root = Some(value.into()),
                "DBPath" if c.dbpath.is_none() => c.dbpath = Some(value.into()),
                "CacheDir" => c.cachedirs.extend(words),
                "HookDir" => c.hookdirs.extend(words),
                "LogFile" if c.logfile.is_none() => c.logfile = Some(value.into()),
                "GPGDir" if c.gpgdir.is_none() => c.gpgdir = Some(value.into()),
                "Architecture" => c.architectures.extend(words),
                "IgnorePkg" => c.ignore.extend(words),
                "IgnoreGroup" => c.ignoregroup.extend(words),
                "NoUpgrade" => c.noupgrade.extend(words),
                "NoExtract" => c.noextract.extend(words),
                "HoldPkg" => c.holdpkg.extend(words),
                "SigLevel" => siglevel.extend(words),
                "LocalFileSigLevel" => {
                    let base = sig_level(&siglevel.iter().map(String::as_str).collect::<Vec<_>>(), 0);
                    c.local_siglevel = Some(sig_level(&words.iter().map(String::as_str).collect::<Vec<_>>(), base));
                }
                "RemoteFileSigLevel" => {
                    let base = sig_level(&siglevel.iter().map(String::as_str).collect::<Vec<_>>(), 0);
                    c.remote_siglevel = Some(sig_level(&words.iter().map(String::as_str).collect::<Vec<_>>(), base));
                }
                "CheckSpace" => c.checkspace = true,
                _ => {}
            }
        } else if let Some(repo) = c.repos.last_mut() {
            match key {
                "Server" => repo.servers.push(value.into()),
                "CacheServer" => repo.cache_servers.push(value.into()),
                "SigLevel" => {
                    let base = sig_level(&siglevel.iter().map(String::as_str).collect::<Vec<_>>(), 0);
                    repo.siglevel = Some(sig_level(&words.iter().map(String::as_str).collect::<Vec<_>>(), base));
                }
                "Usage" => {
                    use libalpm::db::*;
                    repo.usage = 0;
                    for w in &words {
                        repo.usage |= match w.as_str() {
                            "Sync" => USAGE_SYNC,
                            "Search" => USAGE_SEARCH,
                            "Install" => USAGE_INSTALL,
                            "Upgrade" => USAGE_UPGRADE,
                            _ => USAGE_ALL,
                        };
                    }
                }
                _ => {}
            }
        }
    }
    c.siglevel = sig_level(&siglevel.iter().map(String::as_str).collect::<Vec<_>>(), 0);
    if c.architectures.is_empty() || c.architectures.iter().any(|a| a == "auto") {
        let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
        unsafe { libc::uname(&mut uts) };
        let machine = unsafe { std::ffi::CStr::from_ptr(uts.machine.as_ptr()) }.to_string_lossy().into_owned();
        c.architectures.retain(|a| a != "auto");
        c.architectures.push(machine);
    }
}

struct Shim {
    debug: bool,
    /// As pacman's --print: errors only, no events.
    print: bool,
    ask: Option<u32>,
}

impl Frontend for Shim {
    fn log(&self, level: Level, message: &str) {
        match level {
            Level::Error => eprint!("error: {message}"),
            Level::Warning if !self.print => eprint!("warning: {message}"),
            Level::Warning => {}
            Level::Debug if self.debug => print!("debug: {message}"),
            Level::Debug => {}
        }
    }

    fn event(&self, event: &Event) {
        if self.print {
            return;
        }
        match event {
            Event::OptdepRemoval { pkg, optdep } => println!(":: {} optionally requires {}", pkg.name, optdep),
            Event::ScriptletInfo { line } => print!("{line}"),
            Event::PacnewCreated { file } => eprintln!("warning: {file} installed as {file}.pacnew"),
            Event::PacsaveCreated { file } => eprintln!("warning: {file} saved as {file}.pacsave"),
            Event::HookStart { post } => {
                println!(":: Running {}-transaction hooks...", if *post { "post" } else { "pre" })
            }
            Event::HookRunStart { name, desc, position, total } => {
                let w = total.to_string().len();
                println!("({position:>w$}/{total:>w$}) {}", if desc.is_empty() { name } else { desc });
            }
            _ => {}
        }
    }

    /// Answers as pacman with --noconfirm: each question's default, inverted
    /// for the question types set in --ask.
    fn question(&self, q: &mut Question) {
        let invert = |bit: u32| self.ask.is_some_and(|ask| ask & bit != 0);
        match q {
            Question::InstallIgnorePkg { install, .. } => *install = !invert(1),
            Question::Replace { replace, .. } => *replace = !invert(2),
            Question::Conflict { remove, .. } => *remove = invert(4),
            Question::CorruptedPkg { remove, .. } => *remove = !invert(8),
            Question::RemovePkgs { skip, .. } => *skip = invert(16),
            Question::SelectProvider { use_index, .. } => *use_index = if invert(32) { 1 } else { 0 },
            Question::ImportKey { import, .. } => *import = !invert(64),
        }
    }
}

fn strerror(e: &Error) -> &'static str {
    e.kind.message()
}

fn setup(c: &mut Config) -> Handle {
    parse_config(c);
    let root = c.root.clone().unwrap_or_else(|| "/".into());
    let root_slash = if root.ends_with('/') { root.clone() } else { format!("{root}/") };
    let dbpath = c.dbpath.clone().unwrap_or_else(|| format!("{root_slash}var/lib/pacman/"));
    let mut h = Handle::new(&root, &dbpath).unwrap_or_else(|e| fail(&format!("failed to initialize alpm library ({})", strerror(&e))));
    h.logfile = c.logfile.clone().unwrap_or_else(|| format!("{root_slash}var/log/pacman.log"));
    h.set_gpgdir(&c.gpgdir.clone().unwrap_or_else(|| format!("{root_slash}etc/pacman.d/gnupg/")));
    if c.cachedirs.is_empty() {
        c.cachedirs.push(format!("{root_slash}var/cache/pacman/pkg/"));
    }
    for dir in &c.cachedirs {
        h.add_cachedir(dir);
    }
    if !c.hookdirs.is_empty() {
        h.hookdirs.clear();
        h.hookdirs.push(format!("{root_slash}usr/share/libalpm/hooks/"));
    }
    for dir in &c.hookdirs {
        h.add_hookdir(dir);
    }
    h.architectures = c.architectures.clone();
    h.siglevel = c.siglevel;
    h.local_file_siglevel = c.local_siglevel.unwrap_or(c.siglevel);
    h.remote_file_siglevel = c.remote_siglevel.unwrap_or(c.siglevel);
    h.checkspace = c.checkspace;
    h.ignorepkg = c.ignore.clone();
    h.ignoregroup = c.ignoregroup.clone();
    h.noupgrade = c.noupgrade.clone();
    h.noextract = c.noextract.clone();
    h.overwrite = c.overwrite.clone();
    h.assumeinstalled = c.assume_installed.iter().map(|s| Depend::parse(s)).collect();
    for repo in &c.repos {
        let siglevel = repo.siglevel.unwrap_or(c.siglevel);
        let arch = c.architectures.first().cloned().unwrap_or_default();
        let db = h.register_syncdb(&repo.name, siglevel).unwrap_or_else(|_| fail("could not register database"));
        db.usage = repo.usage;
        for server in &repo.servers {
            db.servers.push(server.replace("$repo", &repo.name).replace("$arch", &arch));
        }
        for server in &repo.cache_servers {
            db.cache_servers.push(server.replace("$repo", &repo.name).replace("$arch", &arch));
        }
    }
    h
}

fn trans_init(h: &mut Handle, flags: u32) -> bool {
    match h.trans_init(flags) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("error: failed to init transaction ({})", strerror(&e));
            if e.kind == ErrorKind::HandleLock {
                eprintln!("  if you're sure a package manager is not already running,\n  you can remove {}", h.lockfile);
            }
            false
        }
    }
}

fn print_missing(h: &Handle, e: &Error) {
    for miss in &e.missing {
        match &miss.causing {
            None => println!(":: unable to satisfy dependency '{}' required by {}", miss.depend, miss.target),
            Some(causing) => match h.trans.as_ref().and_then(|t| t.add.iter().find(|a| &a.pkg.name == causing)) {
                Some(add) => println!(
                    ":: installing {} ({}) breaks dependency '{}' required by {}",
                    causing, add.pkg.version, miss.depend, miss.target
                ),
                None => println!(":: removing {causing} breaks dependency '{}' required by {}", miss.depend, miss.target),
            },
        }
    }
}

fn remove(h: &mut Handle, c: &Config) -> i32 {
    if c.targets.is_empty() {
        eprintln!("error: no targets specified (use -h for help)");
        return 1;
    }
    if !trans_init(h, c.flags) {
        return 1;
    }
    let mut retval = 0;
    for target in &c.targets {
        let target = target.strip_prefix("local/").unwrap_or(target);
        let pkgs: Vec<Arc<Pkg>> = match h.local.get(target) {
            Some(p) => vec![p.clone()],
            None => h.local.pkgs().iter().filter(|p| p.info().groups.iter().any(|g| g == target)).cloned().collect(),
        };
        if pkgs.is_empty() {
            eprintln!("error: target not found: {target}");
            retval = 1;
        }
        for pkg in pkgs {
            if let Err(e) = h.remove_pkg(&pkg) {
                eprintln!("error: '{target}': {}", strerror(&e));
                retval = 1;
            }
        }
    }
    if retval == 1 {
        h.trans_release();
        return 1;
    }
    if let Err(e) = h.trans_prepare() {
        eprintln!("error: failed to prepare transaction ({})", strerror(&e));
        if e.kind == ErrorKind::UnsatisfiedDeps {
            print_missing(h, &e);
        }
        h.trans_release();
        return 1;
    }
    let held: Vec<String> = h
        .trans()
        .remove
        .iter()
        .filter(|p| c.holdpkg.iter().any(|pat| libalpm::fnmatch(pat, &p.name)))
        .map(|p| p.name.clone())
        .collect();
    for name in held.iter().filter(|_| !c.print) {
        eprintln!("warning: {name} is designated as a HoldPkg.");
    }
    // The question's --noconfirm answer is no.
    if !held.is_empty() {
        println!(":: HoldPkg was found in target list. Do you want to continue? [y/N] ");
        h.trans_release();
        return 1;
    }
    if h.trans().remove.is_empty() {
        println!(" there is nothing to do");
        h.trans_release();
        return 0;
    }
    if c.print {
        for p in &h.trans().remove {
            println!("{}-{}", p.name, p.version);
        }
        h.trans_release();
        return 0;
    }
    for p in &h.trans().remove {
        println!("removing {}-{}", p.name, p.version);
    }
    let mut retval = 0;
    if let Err(e) = h.trans_commit() {
        eprintln!("error: failed to commit transaction ({})", strerror(&e));
        retval = 1;
    }
    h.trans_release();
    retval
}

/// `sync_prepare_execute`: prepare, show and commit the open transaction.
fn prepare_execute(h: &mut Handle, c: &Config) -> i32 {
    if let Err(e) = h.trans_prepare() {
        eprintln!("error: failed to prepare transaction ({})", strerror(&e));
        match e.kind {
            ErrorKind::PkgInvalidArch => {
                for name in &e.names {
                    println!(":: package {name} does not have a valid architecture");
                }
            }
            ErrorKind::UnsatisfiedDeps => print_missing(h, &e),
            ErrorKind::ConflictingDeps => {
                for c in &e.conflicts {
                    let (a, b) = (&c.package1, &c.package2);
                    if c.reason_is_new() {
                        println!(":: {}-{} and {}-{} are in conflict ({})", a.name, a.version, b.name, b.version, c.reason);
                    } else {
                        println!(":: {}-{} and {}-{} are in conflict", a.name, a.version, b.name, b.version);
                    }
                }
            }
            _ => {}
        }
        h.trans_release();
        return 1;
    }
    if h.trans().add.is_empty() {
        if !c.print {
            println!(" there is nothing to do");
        }
        h.trans_release();
        return 0;
    }
    if c.print {
        for a in &h.trans().add {
            println!("{}", location(h, &a.pkg));
        }
        h.trans_release();
        return 0;
    }
    for p in &h.trans().remove {
        println!("removing {}-{}", p.name, p.version);
    }
    for a in &h.trans().add {
        println!("installing {}-{}", a.pkg.name, a.pkg.version);
    }
    let mut retval = 0;
    if let Err(e) = h.trans_commit() {
        eprintln!("error: failed to commit transaction ({})", strerror(&e));
        match e.kind {
            ErrorKind::FileConflicts => {
                for c in &e.file_conflicts {
                    match c.kind {
                        FileConflictKind::Target => eprintln!("{} exists in both '{}' and '{}'", c.file, c.target, c.ctarget),
                        FileConflictKind::Filesystem if !c.ctarget.is_empty() => {
                            eprintln!("{}: {} exists in filesystem (owned by {})", c.target, c.file, c.ctarget)
                        }
                        FileConflictKind::Filesystem => eprintln!("{}: {} exists in filesystem", c.target, c.file),
                    }
                }
            }
            ErrorKind::PkgInvalid | ErrorKind::PkgInvalidChecksum | ErrorKind::PkgInvalidSig => {
                for name in &e.names {
                    eprintln!("{name} is invalid or corrupted");
                }
            }
            _ => {}
        }
        println!("Errors occurred, no packages were upgraded.");
        retval = 1;
    }
    h.trans_release();
    retval
}

/// `pkg_get_location`: where a package to print comes from.
fn location(h: &Handle, pkg: &Pkg) -> String {
    match &pkg.origin {
        libalpm::package::Origin::Sync { .. } => {
            let filename = &pkg.info().filename;
            if h.download_size(pkg) == 0 {
                for dir in &h.cachedirs {
                    let path = format!("{dir}{filename}");
                    if std::fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                        return format!("file://{path}");
                    }
                }
            }
            match h.syncdbs.iter().find(|d| d.name == pkg.db_name()).and_then(|d| d.servers.first()) {
                Some(server) => format!("{server}/{filename}"),
                None => filename.clone(),
            }
        }
        libalpm::package::Origin::File { path } => path.display().to_string(),
        libalpm::package::Origin::Local { .. } => format!("{}-{}", pkg.name, pkg.version),
    }
}

fn add(h: &mut Handle, pkg: Arc<Pkg>, what: &str) -> bool {
    match h.add_pkg(pkg) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("error: '{what}': {}", strerror(&e));
            false
        }
    }
}

fn sync(h: &mut Handle, c: &Config) -> i32 {
    if c.refresh > 0 {
        println!(":: Synchronizing package databases...");
        if let Err(e) = h.db_update(c.refresh > 1) {
            eprintln!("error: failed to synchronize all databases ({})", strerror(&e));
            return 1;
        }
    }
    if c.targets.is_empty() && c.sysupgrade == 0 {
        if c.refresh > 0 {
            return 0;
        }
        eprintln!("error: no targets specified (use -h for help)");
        return 1;
    }
    if !trans_init(h, c.flags) {
        return 1;
    }
    let mut retval = 0;
    for target in &c.targets {
        let (dbs, name): (Vec<usize>, &str) = match target.split_once('/') {
            Some((repo, name)) if !repo.is_empty() => match h.syncdbs.iter().position(|d| d.name == repo) {
                Some(i) => (vec![i], name),
                None => {
                    eprintln!("error: database not found: {repo}");
                    retval = 1;
                    continue;
                }
            },
            _ => ((0..h.syncdbs.len()).collect(), target.as_str()),
        };
        // A repository named with the target is valid for installs.
        let saved: Vec<u32> = dbs.iter().map(|&i| h.syncdbs[i].usage).collect();
        if dbs.len() == 1 && target.contains('/') {
            h.syncdbs[dbs[0]].usage |= libalpm::db::USAGE_INSTALL;
        }
        let found = {
            let list: Vec<&libalpm::Db> = dbs.iter().map(|&i| &h.syncdbs[i]).collect();
            h.resolvedep(&Depend::parse(name), &list, &[], true)
        };
        for (&i, usage) in dbs.iter().zip(saved) {
            h.syncdbs[i].usage = usage;
        }
        let ok = match found {
            Ok(pkg) => add(h, pkg, name),
            Err(e) if e.kind == ErrorKind::PkgIgnored => {
                eprintln!("warning: skipping target: {name}");
                true
            }
            Err(_) => {
                let group: Vec<Arc<Pkg>> = h.find_group_pkgs(&dbs, name);
                if group.is_empty() {
                    eprintln!("error: target not found: {name}");
                    false
                } else {
                    group.into_iter().all(|p| add(h, p, name))
                }
            }
        };
        if !ok {
            retval = 1;
            if std::path::Path::new(target).exists() {
                eprintln!("warning: '{target}' is a file, did you mean -U/--upgrade instead of -S/--sync?");
            }
        }
    }
    if retval != 0 {
        h.trans_release();
        return retval;
    }
    if c.sysupgrade > 0 {
        if !c.print {
            println!(":: Starting full system upgrade...");
            h.logaction("PACMAN", "starting full system upgrade\n");
        }
        if let Err(e) = h.sync_sysupgrade(c.sysupgrade >= 2) {
            eprintln!("error: {}", strerror(&e));
            h.trans_release();
            return 1;
        }
    }
    prepare_execute(h, c)
}

fn upgrade(h: &mut Handle, c: &Config) -> i32 {
    if c.targets.is_empty() {
        eprintln!("error: no targets specified (use -h for help)");
        return 1;
    }
    let mut targets = Vec::new();
    for target in &c.targets {
        if target.contains("://") {
            match h.fetch_pkgurl(target) {
                Ok(path) => targets.push((path, true)),
                Err(_) => return 1,
            }
        } else {
            targets.push((target.clone(), false));
        }
    }
    if !trans_init(h, c.flags) {
        return 1;
    }
    println!("loading packages...");
    let mut retval = 0;
    for (target, remote) in &targets {
        let siglevel = if *remote { h.remote_file_siglevel } else { h.local_file_siglevel };
        match h.pkg_load(target, true, siglevel) {
            Ok(pkg) => {
                if !add(h, pkg, target) {
                    retval = 1;
                }
            }
            Err(e) => {
                eprintln!("error: '{target}': {}", strerror(&e));
                retval = 1;
            }
        }
    }
    if retval != 0 {
        h.trans_release();
        return retval;
    }
    prepare_execute(h, c)
}

fn database(h: &mut Handle, c: &Config) -> i32 {
    if !c.asdeps && !c.asexplicit {
        return 2;
    }
    if !trans_init(h, 0) {
        return 1;
    }
    let reason = if c.asdeps { Reason::Depend } else { Reason::Explicit };
    let mut retval = 0;
    for target in &c.targets {
        match h.local.get(target).cloned() {
            Some(pkg) => {
                if h.pkg_set_reason(&pkg, reason).is_err() {
                    eprintln!("error: could not set install reason for package {target}");
                    retval = 1;
                }
            }
            None => {
                eprintln!("error: could not set install reason for package {target} (could not find or read package)");
                retval = 1;
            }
        }
    }
    h.trans_release();
    retval
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut c = parse_args(&args);
    if matches!(c.op, Op::None | Op::Unsupported) || (c.op == Op::Sync && c.print && c.sysupgrade == 0 && c.targets.is_empty()) {
        eprintln!("zap-pactest: unsupported operation");
        std::process::exit(2);
    }
    if c.asdeps {
        c.flags |= FLAG_ALLDEPS;
    }
    if c.asexplicit {
        c.flags |= FLAG_ALLEXPLICIT;
    }
    libalpm::log::set_frontend(Arc::new(Shim { debug: c.debug, print: c.print, ask: c.ask }));
    let mut h = setup(&mut c);
    let code = match c.op {
        Op::Remove => remove(&mut h, &c),
        Op::Sync => sync(&mut h, &c),
        Op::Upgrade => upgrade(&mut h, &c),
        Op::Database => database(&mut h, &c),
        _ => 2,
    };
    drop(h);
    std::process::exit(code);
}
