//! Transaction hooks, as libalpm's hook.c: `*.hook` files from the hook
//! directories (later directories override earlier ones by file name) are
//! parsed, matched against the transaction's packages or file paths, and
//! run in file-name order under the root.
use super::deps::{self, Depend};
use super::log::{self, Event};
use super::{util, Handle, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    PreTransaction,
    PostTransaction,
}

const OP_INSTALL: u8 = 1;
const OP_UPGRADE: u8 = 2;
const OP_REMOVE: u8 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TriggerType {
    Package,
    Path,
}

#[derive(Default)]
struct Trigger {
    op: u8,
    kind: Option<TriggerType>,
    targets: Vec<String>,
}

#[derive(Default)]
struct Hook {
    name: String,
    desc: Option<String>,
    triggers: Vec<Trigger>,
    depends: Vec<String>,
    cmd: Option<Vec<String>>,
    when: Option<When>,
    abort_on_fail: bool,
    needs_targets: bool,
    /// Matched targets, fed to the command when `needs_targets`.
    matches: Vec<String>,
}

const SUFFIX: &str = ".hook";

/// pacman's `wordsplit`: split on whitespace, honouring '…' and "…"
/// quotes and `\'`/`\"` escapes. `None` for an unbalanced quote.
fn wordsplit(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_ascii_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Some(words);
        }
        let mut word = String::new();
        while let Some(c) = chars.next_if(|c| !c.is_ascii_whitespace()) {
            match c {
                '\'' | '"' => loop {
                    match chars.next()? {
                        q if q == c => break,
                        '\\' if chars.peek() == Some(&c) => word.push(chars.next()?),
                        other => word.push(other),
                    }
                },
                '\\' if matches!(chars.peek(), Some('\'' | '"')) => word.push(chars.next()?),
                _ => word.push(c),
            }
        }
        words.push(word);
    }
}

/// Parse a hook file as `parse_ini` with `_alpm_hook_parse_cb`. `None`
/// after reporting the first error.
fn parse(file: &str) -> Option<Hook> {
    let text = match std::fs::read(file) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) => {
            log::error(&format!("error while reading hook {file}: {}\n", super::strerror(&e)));
            return None;
        }
    };
    let mut hook = Hook::default();
    let mut section: Option<String> = None;
    let overwriting = |line: usize, what: &str| {
        log::warning(&format!("hook {file} line {line}: overwriting previous definition of {what}\n"))
    };
    let fail = |message: String| {
        log::error(&format!("hook {file} line {message}\n"));
        None
    };
    for (n, raw) in text.lines().enumerate() {
        let line = n + 1;
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.len() >= 2 && trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = &trimmed[1..trimmed.len() - 1];
            match name {
                "Trigger" => hook.triggers.push(Trigger::default()),
                "Action" => {}
                _ => return fail(format!("{line}: invalid section {name}")),
            }
            section = Some(name.to_string());
            continue;
        }
        let (key, value) = match trimmed.split_once('=') {
            Some((k, v)) => (k.trim(), Some(v.trim())),
            None => (trimmed, None),
        };
        let invalid_value = || fail(format!("{line}: invalid value {}", value.unwrap_or("(null)")));
        match section.as_deref() {
            None => return fail(format!("{line}: invalid option {key}")),
            Some("Trigger") => {
                let t = hook.triggers.last_mut().expect("a Trigger section was opened");
                match (key, value) {
                    ("Operation", Some("Install")) => t.op |= OP_INSTALL,
                    ("Operation", Some("Upgrade")) => t.op |= OP_UPGRADE,
                    ("Operation", Some("Remove")) => t.op |= OP_REMOVE,
                    ("Operation", _) => return invalid_value(),
                    ("Type", v) => {
                        if t.kind.is_some() {
                            overwriting(line, "Type");
                        }
                        t.kind = Some(match v {
                            Some("Package") => TriggerType::Package,
                            Some("File") => {
                                log::debug("File targets are deprecated, use Path instead\n");
                                TriggerType::Path
                            }
                            Some("Path") => TriggerType::Path,
                            _ => return invalid_value(),
                        });
                    }
                    // A bare `Target` is a NULL value in C; nothing sensible matches it.
                    ("Target", v) => t.targets.push(v.unwrap_or("").to_string()),
                    _ => return fail(format!("{line}: invalid option {key}")),
                }
            }
            Some(_) => match (key, value) {
                ("When", v) => {
                    if hook.when.is_some() {
                        overwriting(line, "When");
                    }
                    hook.when = Some(match v {
                        Some("PreTransaction") => When::PreTransaction,
                        Some("PostTransaction") => When::PostTransaction,
                        _ => return invalid_value(),
                    });
                }
                ("Description", v) => {
                    if hook.desc.is_some() {
                        overwriting(line, "Description");
                    }
                    hook.desc = Some(v.unwrap_or("").to_string());
                }
                ("Depends", v) => hook.depends.push(v.unwrap_or("").to_string()),
                ("AbortOnFail", _) => hook.abort_on_fail = true,
                ("NeedsTargets", _) => hook.needs_targets = true,
                ("Exec", v) => {
                    if hook.cmd.is_some() {
                        overwriting(line, "Exec");
                    }
                    match v.and_then(wordsplit) {
                        Some(words) => hook.cmd = Some(words),
                        None => return invalid_value(),
                    }
                }
                _ => return fail(format!("{line}: invalid option {key}")),
            },
        }
    }
    Some(hook)
}

/// `_alpm_hook_validate`: report what a parsed hook is missing.
fn validate(hook: &Hook, file: &str) -> bool {
    // A hook without triggers is a mask for a lower-priority hook.
    if hook.triggers.is_empty() {
        return true;
    }
    let mut ok = true;
    for t in &hook.triggers {
        if t.targets.is_empty() {
            ok = false;
            log::error(&format!("Missing trigger targets in hook: {file}\n"));
        }
        if t.kind.is_none() {
            ok = false;
            log::error(&format!("Missing trigger type in hook: {file}\n"));
        }
        if t.op == 0 {
            ok = false;
            log::error(&format!("Missing trigger operation in hook: {file}\n"));
        }
    }
    if hook.cmd.is_none() {
        ok = false;
        log::error(&format!("Missing Exec option in hook: {file}\n"));
    }
    match hook.when {
        None => {
            ok = false;
            log::error(&format!("Missing When option in hook: {file}\n"));
        }
        Some(When::PostTransaction) if hook.abort_on_fail => {
            log::warning(&format!("AbortOnFail set for PostTransaction hook: {file}\n"))
        }
        _ => {}
    }
    ok
}

/// Remove the paths present in both sorted lists, returning them.
fn take_common(install: &mut Vec<String>, remove: &mut Vec<String>) -> Vec<String> {
    let common: Vec<String> = install.iter().filter(|p| remove.binary_search(p).is_ok()).cloned().collect();
    let mut common_dedup = common.clone();
    common_dedup.dedup();
    install.retain(|p| common_dedup.binary_search(p).is_err());
    remove.retain(|p| common_dedup.binary_search(p).is_err());
    common_dedup
}

impl Handle {
    /// `_alpm_hook_trigger_match_file`: whether the transaction installs,
    /// upgrades or removes a path the trigger targets. A path both
    /// installed and removed counts as an upgrade.
    fn trigger_match_file(&self, hook: &mut Hook, t: &Trigger) -> bool {
        let trans = self.trans();
        let matches = |name: &str| util::fnmatch_patterns(&t.targets, name) == Some(true);
        let mut install = Vec::new();
        let mut remove = Vec::new();
        for add in &trans.add {
            for f in &add.pkg.files().files {
                if util::fnmatch_patterns(&self.noextract, &f.name) != Some(true) && matches(&f.name) {
                    install.push(f.name.clone());
                }
            }
        }
        let olds = trans.add.iter().filter_map(|a| a.old.as_ref());
        for pkg in olds.chain(&trans.remove) {
            remove.extend(pkg.files().files.iter().filter(|f| matches(&f.name)).map(|f| f.name.clone()));
        }
        install.sort();
        remove.sort();
        let upgrade = take_common(&mut install, &mut remove);
        let hit = |op: u8, list: &[String]| t.op & op != 0 && !list.is_empty();
        let matched = hit(OP_INSTALL, &install) || hit(OP_UPGRADE, &upgrade) || hit(OP_REMOVE, &remove);
        if hook.needs_targets {
            for (op, list) in [(OP_INSTALL, install), (OP_UPGRADE, upgrade), (OP_REMOVE, remove)] {
                if t.op & op != 0 {
                    hook.matches.extend(list);
                }
            }
        }
        matched
    }

    /// `_alpm_hook_trigger_match_pkg`: whether the transaction installs,
    /// upgrades or removes a package the trigger targets.
    fn trigger_match_pkg(&self, hook: &mut Hook, t: &Trigger) -> bool {
        let trans = self.trans();
        let matches = |name: &str| util::fnmatch_patterns(&t.targets, name) == Some(true);
        let mut found = Vec::new();
        if t.op & (OP_INSTALL | OP_UPGRADE) != 0 {
            for add in trans.add.iter().filter(|a| matches(&a.pkg.name)) {
                let op = if add.old.is_some() { OP_UPGRADE } else { OP_INSTALL };
                if t.op & op != 0 {
                    if !hook.needs_targets {
                        return true;
                    }
                    found.push(add.pkg.name.clone());
                }
            }
        }
        if t.op & OP_REMOVE != 0 {
            for pkg in trans.remove.iter().filter(|p| matches(&p.name)) {
                if !trans.add.iter().any(|a| a.pkg.name == pkg.name) {
                    if !hook.needs_targets {
                        return true;
                    }
                    found.push(pkg.name.clone());
                }
            }
        }
        let matched = !found.is_empty();
        hook.matches.extend(found);
        matched
    }

    /// `_alpm_hook_triggered`: whether any trigger matches. With
    /// NeedsTargets every trigger runs so all matches are collected.
    fn hook_triggered(&self, hook: &mut Hook) -> bool {
        let triggers = std::mem::take(&mut hook.triggers);
        let mut triggered = false;
        for t in &triggers {
            let matched = match t.kind {
                Some(TriggerType::Package) => self.trigger_match_pkg(hook, t),
                _ => self.trigger_match_file(hook, t),
            };
            if matched {
                triggered = true;
                if !hook.needs_targets {
                    break;
                }
            }
        }
        hook.triggers = triggers;
        triggered
    }

    /// `_alpm_hook_run_hook`: run one hook's command, its sorted and
    /// deduplicated matches on stdin when it needs targets.
    fn run_hook(&self, hook: &mut Hook) -> bool {
        for dep in &hook.depends {
            if deps::find_satisfier(self.local.pkgs(), &Depend::parse(dep)).is_none() {
                log::error(&format!("unable to run hook {}: could not satisfy dependencies\n", hook.name));
                return false;
            }
        }
        let cmd = hook.cmd.as_deref().unwrap_or_default();
        let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
        let stdin = hook.needs_targets.then(|| {
            hook.matches.sort();
            hook.matches.dedup();
            hook.matches.iter().flat_map(|m| m.bytes().chain(*b"\n")).collect()
        });
        util::run_chroot(self, argv[0], &argv, stdin)
    }

    /// Parse every hook file, the last hook directory first so that its
    /// hooks override same-named ones in earlier directories. The flag is
    /// false when a hook or directory could not be read.
    fn load_hooks(&self) -> (Vec<Hook>, bool) {
        let mut hooks: Vec<Hook> = Vec::new();
        let mut ok = true;
        for dir in self.hookdirs.iter().rev() {
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    log::error(&format!("could not open directory: {dir}: {}\n", super::strerror(&e)));
                    ok = false;
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        log::error(&format!("could not read directory: {dir}: {}\n", super::strerror(&e)));
                        ok = false;
                        break;
                    }
                };
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = format!("{dir}{name}");
                if !name.ends_with(SUFFIX) {
                    log::debug(&format!("skipping non-hook file {path}\n"));
                    continue;
                }
                if hooks.iter().any(|h| h.name == name) {
                    log::debug(&format!("skipping overridden hook {path}\n"));
                    continue;
                }
                match std::fs::metadata(&path) {
                    Err(e) => {
                        log::error(&format!("could not stat file {path}: {}\n", super::strerror(&e)));
                        ok = false;
                        continue;
                    }
                    Ok(m) if m.is_dir() => {
                        log::debug(&format!("skipping directory {path}\n"));
                        continue;
                    }
                    Ok(_) => {}
                }
                log::debug(&format!("parsing hook file {path}\n"));
                match parse(&path).filter(|h| validate(h, &path)) {
                    Some(mut hook) => {
                        hook.name = name;
                        hooks.push(hook);
                    }
                    None => {
                        log::debug(&format!("parsing hook file {path} failed\n"));
                        ok = false;
                    }
                }
            }
        }
        (hooks, ok)
    }

    /// `_alpm_hook_run`: run the hooks for `when` that the transaction
    /// triggers. A pre-transaction hook that cannot be loaded, or that fails
    /// with AbortOnFail, makes this an error.
    pub(crate) fn hook_run(&mut self, when: When) -> Result<()> {
        let (mut hooks, mut ok) = self.load_hooks();
        if !ok && when == When::PreTransaction {
            return Err(super::ErrorKind::TransHookFailed.into());
        }
        // Order by name without the suffix, so "a.hook" runs before "a-b.hook".
        hooks.sort_by(|a, b| a.name[..a.name.len() - SUFFIX.len()].cmp(&b.name[..b.name.len() - SUFFIX.len()]));
        let mut triggered: Vec<Hook> = Vec::new();
        for mut hook in hooks {
            if hook.when == Some(when) && self.hook_triggered(&mut hook) {
                triggered.push(hook);
            }
        }
        if triggered.is_empty() {
            return if ok { Ok(()) } else { Err(super::ErrorKind::TransHookFailed.into()) };
        }
        let post = when == When::PostTransaction;
        log::event(&Event::HookStart { post });
        let total = triggered.len();
        for (i, hook) in triggered.iter_mut().enumerate() {
            self.logaction("ALPM", &format!("running '{}'...\n", hook.name));
            let desc = hook.desc.clone().unwrap_or_default();
            let (name, position) = (hook.name.clone(), i + 1);
            log::event(&Event::HookRunStart { name: &name, desc: &desc, position, total });
            if !self.run_hook(hook) && hook.abort_on_fail {
                ok = false;
            }
            log::event(&Event::HookRunDone);
            if !ok && !post {
                break;
            }
        }
        log::event(&Event::HookDone);
        if ok {
            Ok(())
        } else {
            Err(super::ErrorKind::TransHookFailed.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wordsplit_handles_quotes_and_escapes() {
        let words = wordsplit(r#"  /bin/sh -c 'echo "a b"' x\"y "it\"s" "#).unwrap();
        assert_eq!(words, ["/bin/sh", "-c", r#"echo "a b""#, r#"x"y"#, r#"it"s"#]);
        assert!(wordsplit("'unbalanced").is_none());
    }

    #[test]
    fn upgrades_are_paths_both_installed_and_removed() {
        let mut install = vec!["a".to_string(), "b".into(), "b".into(), "c".into()];
        let mut remove = vec!["b".to_string(), "d".into()];
        assert_eq!(take_common(&mut install, &mut remove), ["b"]);
        assert_eq!((install, remove), (vec!["a".to_string(), "c".into()], vec!["d".to_string()]));
    }
}
