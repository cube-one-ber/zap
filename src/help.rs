//! Help pages, rendered from the command and option tables in cli.rs.
use crate::cli::{self, Flag, Group, OptionSpec, Spec, Targets, OPTIONS, SPECS};
use crate::ui::{self, NoteKind, Tone};
use crate::util::VERSION;

const GROUPS: [(Group, &str); 5] = [
    (Group::Everyday, "Everyday"),
    (Group::Inspect, "Inspect"),
    (Group::Build, "Build"),
    (Group::Maintain, "Maintain"),
    (Group::About, "About"),
];

// The options worth knowing before reading any command's own page.
const COMMON: [Flag; 7] = [Flag::DryRun, Flag::Aur, Flag::Repo, Flag::Needed, Flag::Quiet, Flag::Color, Flag::Help];

pub fn show(topic: Option<cli::Command>) {
    match topic {
        Some(command) => command_page(cli::spec(command)),
        None => overview(),
    }
}

fn syntax(entry: &Spec) -> String {
    if entry.operands.is_empty() {
        entry.name.to_string()
    } else {
        format!("{} {}", entry.name, entry.operands)
    }
}

fn option_syntax(entry: &OptionSpec) -> String {
    let mut out = entry.long.to_string();
    if !entry.short.is_empty() {
        out.push_str(", ");
        out.push_str(entry.short);
    }
    if !entry.value.is_empty() {
        out.push(' ');
        out.push_str(entry.value);
    }
    out
}

fn overview() {
    ui::print(&format!(
        "\n  {}⚡ zap{} {}{}{}\n",
        ui::style(Tone::Accent),
        ui::reset(),
        ui::style(Tone::Bold),
        VERSION,
        ui::reset()
    ));
    ui::text("A native Arch Linux and AUR helper", 2, Tone::Muted);
    ui::blank();
    ui::text("zap <command> [targets] [options]", 2, Tone::Plain);

    let syntax_width = SPECS.iter().map(|e| ui::display_width(&syntax(e))).max().unwrap_or(0);
    let alias_width = SPECS.iter().map(|e| ui::display_width(e.alias)).max().unwrap_or(0);
    for (group, title) in GROUPS {
        ui::heading(title, "");
        for entry in SPECS.iter().filter(|e| e.group == group) {
            ui::define(&syntax(entry), entry.alias, entry.summary, syntax_width, alias_width);
        }
    }

    ui::heading("Common options", "");
    let common = || OPTIONS.iter().filter(|e| COMMON.contains(&e.flag));
    let option_width = common().map(|e| ui::display_width(&option_syntax(e))).max().unwrap_or(0);
    for entry in common() {
        ui::define(&option_syntax(entry), "", entry.summary, option_width, 0);
    }

    ui::heading("Examples", "");
    for example in ["zap select browser", "zap install ripgrep --needed", "zap upgrade --devel --dry-run"] {
        ui::line(example, 2, Tone::Accent);
    }

    ui::blank();
    ui::note(NoteKind::Info, "zap help <command> shows one command's usage, options and examples.");
    ui::note(NoteKind::Info, "Build files run as your user only after you review them; system changes go through run0 or sudo and need a final confirmation, unless you pass --noconfirm.");
    ui::blank();
}

fn command_page(entry: &Spec) {
    let alias = if entry.alias.is_empty() { String::new() } else { format!("alias {}", entry.alias) };
    ui::heading(&format!("zap {}", syntax(entry)), &alias);
    ui::text(entry.summary, 2, Tone::Plain);
    if !entry.about.is_empty() {
        ui::blank();
        ui::text(entry.about, 2, Tone::Muted);
    }

    let accepted = || OPTIONS.iter().filter(|c| c.commands.contains(entry.command));
    let option_width = accepted().map(|c| ui::display_width(&option_syntax(c))).max().unwrap_or(0);
    ui::heading("Options", "");
    for candidate in accepted() {
        ui::define(&option_syntax(candidate), "", candidate.summary, option_width, 0);
    }
    if entry.targets != Targets::None {
        ui::define("--", "", "Treat everything after it as a target, even if it starts with a hyphen", option_width, 0);
    }

    if !entry.examples.is_empty() {
        ui::heading("Examples", "");
        for example in entry.examples {
            ui::line(example, 2, Tone::Accent);
        }
    }
    ui::blank();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_help_page_renders_within_the_terminal_width_without_control_bytes() {
        let _g = ui::test_lock();
        for width in [40, 80, 120] {
            ui::set_caps(Some(ui::Caps { width: Some(width), ..ui::Caps::default() }));
            ui::begin_capture();
            show(None);
            for entry in &SPECS {
                show(Some(entry.command));
            }
            let output = ui::end_capture();
            for line in output.split('\n') {
                // Example command lines are printed whole so they can be copied.
                if line.starts_with("  zap ") {
                    continue;
                }
                assert!(ui::display_width(line) <= width, "{line:?} at {width}");
                assert!(line.bytes().all(|b| b >= 32 && b != 127));
            }
        }
        ui::set_caps(None);
    }

    #[test]
    fn a_command_page_lists_exactly_the_options_that_command_accepts() {
        let _g = ui::test_lock();
        ui::set_caps(Some(ui::Caps { width: Some(100), ..ui::Caps::default() }));
        ui::begin_capture();
        show(Some(cli::Command::Remove));
        let output = ui::end_capture();
        for expected in [
            "zap remove PACKAGES",
            "alias -R",
            "--recursive",
            "--nosave",
            "--dry-run",
            "--color WHEN",
            "zap remove some-package --recursive",
        ] {
            assert!(output.contains(expected), "{expected}");
        }
        for absent in ["--needed", "--aur", "--sandbox", "--searchby"] {
            assert!(!output.contains(absent), "{absent}");
        }
        ui::set_caps(None);
    }
}
