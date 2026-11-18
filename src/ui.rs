//! Terminal presentation: capability detection, buffered output, and the small
//! set of components every command renders with.
//!
//! Every component sanitizes the text it is given, so package metadata, file
//! contents and remote responses cannot inject terminal control sequences.
use crate::errors::{Kind, Result};
use crate::signals;
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Bold,
    Muted,
    Accent,
    Special,
    Success,
    Warning,
    Danger,
    Underline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

impl ColorMode {
    pub fn parse(s: &str) -> Option<ColorMode> {
        match s {
            "auto" => Some(ColorMode::Auto),
            "always" => Some(ColorMode::Always),
            "never" => Some(ColorMode::Never),
            _ => None,
        }
    }
}

// ── Capabilities ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// Styling on standard output and standard error respectively.
    pub color: bool,
    pub color_err: bool,
    /// Standard output is a terminal that supports cursor control.
    pub live: bool,
    /// The locale and terminal can render the non-ASCII glyphs zap draws with.
    pub unicode: bool,
    /// The locale encodes text as UTF-8. Otherwise only ASCII is written: in
    /// an 8-bit locale, UTF-8 continuation bytes are C1 control codes.
    pub utf8: bool,
    /// Fixed width; None queries the terminal on demand.
    pub width: Option<usize>,
}

impl Default for Caps {
    fn default() -> Caps {
        Caps { color: false, color_err: false, live: false, unicode: true, utf8: true, width: None }
    }
}

struct State {
    color_mode: ColorMode,
    caps: Option<Caps>,
    pending: Vec<u8>,
    depth: usize,
    capturing: bool,
    progress_active: bool,
    progress_key: Option<u64>,
    progress_percent: usize,
    progress_time: i64,
    /// The last two bytes written to standard output, to avoid doubled blank lines.
    tail: [u8; 2],
}

static STATE: Mutex<State> = Mutex::new(State {
    color_mode: ColorMode::Auto,
    caps: None,
    pending: Vec::new(),
    depth: 0,
    capturing: false,
    progress_active: false,
    progress_key: None,
    progress_percent: 0,
    progress_time: 0,
    tail: [0, 0],
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn set_color_mode(mode: ColorMode) {
    let mut s = state();
    s.color_mode = mode;
    s.caps = None;
}

/// Pin capabilities, for deterministic rendering in tests. None detects again.
#[cfg(test)]
pub fn set_caps(value: Option<Caps>) {
    state().caps = value;
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) != 0 }
}

fn detect(mode: ColorMode) -> Caps {
    let term = env("TERM").unwrap_or_default();
    let dumb = term == "dumb";
    // https://no-color.org: any non-empty value disables color.
    let no_color = std::env::var_os("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false);
    let out_tty = isatty(1);
    let err_tty = isatty(2);
    let codeset = unsafe {
        let p = libc::nl_langinfo(libc::CODESET);
        if p.is_null() {
            String::new()
        } else {
            std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    };
    let utf8 = codeset.eq_ignore_ascii_case("UTF-8");
    let pick = |tty: bool| match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => tty && !dumb && !no_color,
    };
    Caps {
        color: pick(out_tty),
        color_err: pick(err_tty),
        live: out_tty && !dumb,
        // The Linux console font lacks the box and symbol glyphs.
        unicode: utf8 && term != "linux",
        utf8,
        width: None,
    }
}

fn caps_of(s: &mut State) -> Caps {
    if let Some(c) = s.caps {
        return c;
    }
    let c = detect(s.color_mode);
    s.caps = Some(c);
    c
}

fn caps() -> Caps {
    caps_of(&mut state())
}

pub fn columns() -> usize {
    if let Some(fixed) = caps().width {
        return fixed;
    }
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::isatty(1) != 0 && libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            return (ws.ws_col as usize).clamp(20, 120);
        }
    }
    if let Some(Ok(parsed)) = env("COLUMNS").map(|v| v.parse::<usize>()) {
        return parsed.clamp(20, 120);
    }
    80
}

fn code(tone: Tone) -> &'static str {
    match tone {
        Tone::Plain => "",
        Tone::Bold => "\x1b[1m",
        Tone::Muted => "\x1b[90m",
        Tone::Accent => "\x1b[1;36m",
        Tone::Special => "\x1b[35m",
        Tone::Success => "\x1b[32m",
        Tone::Warning => "\x1b[33m",
        Tone::Danger => "\x1b[1;31m",
        Tone::Underline => "\x1b[4m",
    }
}

pub fn style(tone: Tone) -> &'static str {
    if caps().color {
        code(tone)
    } else {
        ""
    }
}

pub fn reset() -> &'static str {
    if caps().color {
        "\x1b[0m"
    } else {
        ""
    }
}

/// Whether standard output is being styled.
pub fn styled() -> bool {
    caps().color
}

// ── Output ──────────────────────────────────────────────────────────────────

const FLUSH_THRESHOLD: usize = 32 * 1024;

/// A component under construction, styled for the current capabilities.
struct Out {
    buf: String,
    color: bool,
}

impl Out {
    fn new() -> Out {
        Out { buf: String::new(), color: caps().color }
    }
    fn put(&mut self, s: &str) {
        self.buf.push_str(s);
    }
    fn pad(&mut self, cells: usize) {
        for _ in 0..cells {
            self.buf.push(' ');
        }
    }
    fn repeat(&mut self, glyph: &str, times: usize) {
        for _ in 0..times {
            self.buf.push_str(glyph);
        }
    }
    fn paint(&mut self, tone: Tone, text: &str) {
        if text.is_empty() {
            return;
        }
        if tone == Tone::Plain || !self.color {
            return self.put(text);
        }
        self.put(code(tone));
        self.put(text);
        self.put("\x1b[0m");
    }
    fn reset(&mut self) {
        if self.color {
            self.put("\x1b[0m");
        }
    }
}

fn begin(s: &mut State) {
    if s.depth == 0 && s.progress_active {
        s.pending.extend_from_slice(b"\r\x1b[2K");
        s.progress_active = false;
    }
    s.depth += 1;
}

fn end(s: &mut State) {
    s.depth -= 1;
    // Each component is written as it completes, so output is never stale
    // while a child process or a prompt has the terminal.
    if s.depth == 0 || s.pending.len() >= FLUSH_THRESHOLD {
        flush_state(s);
    }
}

/// Append a finished component to the output.
fn emit(out: Out) {
    let mut s = state();
    begin(&mut s);
    s.pending.extend_from_slice(out.buf.as_bytes());
    end(&mut s);
}

/// Batch the output of a bulk listing into few writes, until `release`.
pub fn hold() {
    begin(&mut state());
}

pub fn release() {
    end(&mut state());
}

/// Write buffered standard output. Components do this themselves; call it
/// before starting a child process or exiting while output may be held.
pub fn flush() {
    flush_state(&mut state());
}

fn flush_state(s: &mut State) {
    if s.capturing || s.pending.is_empty() {
        return;
    }
    let c = caps_of(s);
    let bytes = plain(&s.pending, c);
    write_all(1, &bytes);
    s.tail = ending(s);
    s.pending.clear();
}

/// The last two bytes of standard output, counting what is still pending.
fn ending(s: &State) -> [u8; 2] {
    match s.pending.as_slice() {
        [.., a, b] => [*a, *b],
        [b] => [s.tail[1], *b],
        [] => s.tail,
    }
}

fn write_all(fd: i32, bytes: &[u8]) {
    let mut offset = 0;
    while offset < bytes.len() {
        let n = unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if n <= 0 {
            return;
        }
        offset += n as usize;
    }
}

// Single-cell ASCII stand-ins keep alignment when the glyphs cannot render.
const FALLBACKS: [(&str, &str); 12] = [
    ("▌", ">"),
    ("─", "-"),
    ("━", "#"),
    ("·", "-"),
    ("→", ">"),
    ("—", "-"),
    ("…", "~"),
    ("✓", "+"),
    ("×", "x"),
    ("⚡", "*"),
    ("◆", "*"),
    ("\u{fffd}", "?"),
];

fn plain(bytes: &[u8], c: Caps) -> Vec<u8> {
    if c.unicode && c.utf8 {
        return bytes.to_vec();
    }
    let mut result = bytes.to_vec();
    for (glyph, ascii) in FALLBACKS {
        if find(&result, glyph.as_bytes()).is_none() {
            continue;
        }
        result = replace_bytes(&result, glyph.as_bytes(), ascii.as_bytes());
    }
    if c.utf8 {
        return result;
    }
    // One '?' per character keeps the cell count wcwidth reported in this locale.
    let mut ascii = Vec::with_capacity(result.len());
    let mut i = 0;
    while i < result.len() {
        if result[i] < 0x80 {
            ascii.push(result[i]);
            i += 1;
            continue;
        }
        ascii.push(b'?');
        let len = sequence_length(result[i]).unwrap_or(1);
        i += len.min(result.len() - i).max(1);
    }
    ascii
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn replace_bytes(input: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}

/// Run `body`, showing what it prints through the user's pager when that is
/// taller than the terminal, as git does. Otherwise, or when no pager starts,
/// the output is written directly.
pub fn paged<T>(body: impl FnOnce() -> T) -> T {
    let rows = terminal_rows();
    {
        let mut s = state();
        if s.capturing || rows.is_none() || !caps_of(&mut s).live {
            drop(s);
            return body();
        }
        flush_state(&mut s);
        s.capturing = true;
    }
    let result = body();
    let mut s = state();
    s.capturing = false;
    let c = caps_of(&mut s);
    let bytes = plain(&std::mem::take(&mut s.pending), c);
    drop(s);
    let lines = bytes.iter().filter(|&&b| b == b'\n').count();
    if lines + 2 < rows.unwrap_or(usize::MAX) || !page(&bytes) {
        write_all(1, &bytes);
    }
    result
}

/// The terminal's height, when standard output is one.
fn terminal_rows() -> Option<usize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::isatty(1) != 0 && libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0).then_some(ws.ws_row as usize)
    }
}

/// Feed `bytes` to $PAGER (default less) and wait for it to exit. False
/// when the pager failed, such as a $PAGER that is not installed, so the
/// caller prints the text instead.
fn page(bytes: &[u8]) -> bool {
    use std::io::Write;
    let pager = env("PAGER").filter(|p| !p.trim().is_empty()).unwrap_or_else(|| "less".into());
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", &pager]).stdin(std::process::Stdio::piped());
    // Like git: keep colours, and leave short output on screen.
    if env("LESS").is_none() {
        command.env("LESS", "FRX");
    }
    let Ok(mut child) = command.spawn() else { return false };
    // Ctrl-C belongs to the pager while it runs, as with git.
    crate::signals::defer_interrupts();
    if let Some(mut stdin) = child.stdin.take() {
        // The reader may quit early; that is not a failure.
        let _ = stdin.write_all(bytes);
    }
    let shown = child.wait().is_ok_and(|status| status.success());
    crate::signals::restore();
    shown
}

/// Collect output in memory instead of writing it, for tests.
#[cfg(test)]
pub fn begin_capture() {
    let mut s = state();
    flush_state(&mut s);
    s.capturing = true;
}

#[cfg(test)]
pub fn end_capture() -> String {
    let mut s = state();
    s.capturing = false;
    let c = caps_of(&mut s);
    let result = plain(&s.pending, c);
    s.pending.clear();
    String::from_utf8_lossy(&result).into_owned()
}

/// Standard output, written as given: sanitize untrusted values with `safe`
/// first.
pub fn print(text: &str) {
    let mut out = Out::new();
    out.put(text);
    emit(out);
}

pub fn blank() {
    print("\n");
}

/// A path for display, with the home directory as ~, as a shell takes it.
pub fn home_path(path: &str) -> String {
    match env("HOME").filter(|home| home.len() > 1) {
        Some(home) if path.starts_with(&format!("{home}/")) => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// A blank line, unless the output already ends with one.
pub fn separate() {
    if ending(&state()) != *b"\n\n" {
        blank();
    }
}

/// Standard error, ordered after pending standard output.
pub fn eprint(message: &str) {
    let mut s = state();
    begin(&mut s);
    s.depth -= 1;
    flush_state(&mut s);
    if s.capturing {
        s.pending.extend_from_slice(message.as_bytes());
        return;
    }
    let c = caps_of(&mut s);
    write_all(2, &plain(message.as_bytes(), c));
}

/// Hold the output lock for tests that render through the shared state.
#[cfg(test)]
pub fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe {
        libc::setlocale(libc::LC_CTYPE, c"C.UTF-8".as_ptr());
    }
    guard
}

// ── Text ────────────────────────────────────────────────────────────────────

/// The length of the UTF-8 sequence a leading byte starts, if it can.
fn sequence_length(byte: u8) -> Option<usize> {
    match byte {
        0x00..=0x7f => Some(1),
        0xc0..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf7 => Some(4),
        _ => None,
    }
}

/// Neutralize terminal control in arbitrary bytes: C0/C1 control characters
/// become spaces, bidirectional overrides and invalid UTF-8 become visible
/// placeholders. The result has the same byte length as the input.
pub fn safe_bytes(input: &[u8]) -> String {
    let mut result = input.to_vec();
    let mut i = 0;
    while i < result.len() {
        let byte = result[i];
        if byte < 0x80 {
            if byte < 32 || byte == 127 {
                result[i] = b' ';
            }
            i += 1;
            continue;
        }
        let Some(len) = sequence_length(byte) else {
            result[i] = b'?';
            i += 1;
            continue;
        };
        if i + len > result.len() {
            result[i] = b'?';
            i += 1;
            continue;
        }
        let point = match std::str::from_utf8(&result[i..i + len]) {
            Ok(s) => s.chars().next().unwrap() as u32,
            Err(_) => {
                result[i] = b'?';
                i += 1;
                continue;
            }
        };
        if (0x80..=0x9f).contains(&point) {
            result[i..i + len].fill(b' ');
        } else if reordering(point) {
            // U+FFFD where it fits the sequence's length; U+061C is shorter.
            if len == 3 {
                result[i..i + len].copy_from_slice("\u{fffd}".as_bytes());
            } else {
                result[i..i + len].fill(b'?');
            }
        }
        i += len;
    }
    // Every invalid byte was replaced, so this is valid UTF-8.
    String::from_utf8(result).unwrap_or_default()
}

pub fn safe(s: &str) -> String {
    safe_bytes(s.as_bytes())
}

/// Characters that reorder or hide neighbouring text when displayed.
fn reordering(point: u32) -> bool {
    matches!(point, 0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0x2028 | 0x2029)
}

extern "C" {
    // libc accounts for wide and combining characters in the active locale.
    fn wcwidth(c: libc::wchar_t) -> libc::c_int;
}

fn cell_width(ch: char) -> usize {
    let width = unsafe { wcwidth(ch as libc::wchar_t) };
    if width < 0 {
        1
    } else {
        width as usize
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(cell_width).sum()
}

/// Wrap at word boundaries; a word longer than the line is kept whole.
/// Continuation lines are indented; the first line is not.
pub fn wrapped(s: &str, width: usize, indent: usize) -> String {
    let mut out = String::new();
    let available = width.saturating_sub(indent).max(1);
    let mut rest = s.trim_matches(' ');
    while !rest.is_empty() {
        let bytes = rest.as_bytes();
        let mut stop = 0;
        let mut cells = 0;
        let mut space: Option<usize> = None;
        for (at, ch) in rest.char_indices() {
            let wide = cell_width(ch);
            if cells + wide > available && at > 0 {
                break;
            }
            if ch == ' ' {
                space = Some(at);
            }
            cells += wide;
            stop = at + ch.len_utf8();
        }
        if stop < rest.len() && bytes[stop] != b' ' {
            match space {
                Some(last) if last > 0 => stop = last,
                // A word longer than the line, such as a path, overflows
                // whole so it can still be copied.
                _ => stop = rest[stop..].find(' ').map_or(rest.len(), |i| stop + i),
            }
        }
        out.push_str(rest[..stop].trim_end_matches(' '));
        rest = rest[stop..].trim_start_matches(' ');
        if !rest.is_empty() {
            out.push('\n');
            for _ in 0..indent {
                out.push(' ');
            }
        }
    }
    out
}

/// Shorten to a cell budget with an ellipsis, without splitting UTF-8.
pub fn clipped(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut stop = 0;
    let mut cells = 0;
    for (at, ch) in s.char_indices() {
        let wide = cell_width(ch);
        if cells + wide > width - 1 {
            break;
        }
        cells += wide;
        stop = at + ch.len_utf8();
    }
    format!("{}…", &s[..stop])
}

/// A sanitized, wrapped paragraph.
pub fn text(s: &str, indent: usize, tone: Tone) {
    let content = wrapped(&safe(s), columns(), indent);
    let mut out = Out::new();
    out.pad(indent);
    out.paint(tone, &content);
    out.put("\n");
    emit(out);
}

/// Sanitized text on one line, never wrapped: links and paths stay intact.
pub fn line(s: &str, indent: usize, tone: Tone) {
    line_bytes(s.as_bytes(), indent, tone)
}

pub fn line_bytes(s: &[u8], indent: usize, tone: Tone) {
    let mut out = Out::new();
    out.pad(indent);
    out.paint(tone, &safe_bytes(s));
    out.put("\n");
    emit(out);
}

// ── Components ──────────────────────────────────────────────────────────────

/// Section heading with an optional muted detail, such as a count.
/// Neither part is ever cut: a detail that does not fit beside the title
/// goes on its own line beneath it.
pub fn heading(title: &str, detail: &str) {
    let width = columns();
    let clean_title = safe(title);
    let clean_detail = safe(detail);
    let mut out = Out::new();
    // A heading follows one blank line, never two.
    if ending(&state()) != *b"\n\n" {
        out.put("\n");
    }
    out.paint(Tone::Accent, "▌");
    out.put(" ");
    out.paint(Tone::Bold, &wrapped(&clean_title, width, 2));
    let last_line = match clean_title.rfind('\n') {
        Some(at) => &clean_title[at + 1..],
        None => &clean_title[..],
    };
    let used = 2 + display_width(last_line).min(width);
    if !clean_detail.is_empty() && used + 2 + display_width(&clean_detail) <= width {
        out.put("  ");
        out.paint(Tone::Muted, &clean_detail);
    } else if !clean_detail.is_empty() {
        out.put("\n");
        out.pad(2);
        out.paint(Tone::Muted, &wrapped(&clean_detail, width, 2));
    }
    out.put("\n");
    emit(out);
}

/// A labelled horizontal rule that separates long bodies, such as files.
pub fn rule(label: &str) {
    let width = columns();
    let clean = safe(label);
    let mut out = Out::new();
    out.put("\n");
    // A label that does not fit beside the rule, such as a long path, is
    // printed whole above a plain one rather than cut.
    if display_width(&clean) + 8 > width {
        out.paint(Tone::Bold, &clean);
        out.put("\n");
        if out.color {
            out.put(code(Tone::Muted));
        }
        out.repeat("─", width);
        out.reset();
        out.put("\n");
        return emit(out);
    }
    if out.color {
        out.put(code(Tone::Muted));
    }
    out.put("── ");
    if out.color {
        out.put("\x1b[0m\x1b[1m");
    }
    out.put(&clean);
    if out.color {
        out.put("\x1b[0m\x1b[90m");
    }
    out.put(" ");
    out.repeat("─", width.saturating_sub(display_width(&clean) + 4));
    out.reset();
    out.put("\n");
    emit(out);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    Info,
    Success,
    Warning,
    Failure,
}

pub fn note(kind: NoteKind, message: &str) {
    let (mark, tone) = match kind {
        NoteKind::Info => ("·", Tone::Muted),
        NoteKind::Success => ("✓", Tone::Success),
        NoteKind::Warning => ("!", Tone::Warning),
        NoteKind::Failure => ("×", Tone::Danger),
    };
    marked(mark, tone, message, if kind == NoteKind::Info { Tone::Muted } else { Tone::Plain });
}

/// A line led by a one-column mark in its own tone, wrapping beneath its
/// text: the shape of notes and of flagged list items.
pub fn marked(mark: &str, mark_tone: Tone, message: &str, tone: Tone) {
    marked_link(mark, mark_tone, message, tone, "");
}

/// `marked`, with the text as a hyperlink to `link` where the terminal
/// shows colour. Returns whether it was linked, so the caller can print
/// the URL itself otherwise.
pub fn marked_link(mark: &str, mark_tone: Tone, message: &str, tone: Tone, link: &str) -> bool {
    let content = wrapped(&safe(message), columns(), 4);
    let mut out = Out::new();
    let linked = out.color && linkable(link);
    out.pad(2);
    out.paint(mark_tone, mark);
    out.put(" ");
    if linked {
        out.put(&format!("\x1b]8;;{link}\x1b\\"));
    }
    out.paint(tone, &content);
    if linked {
        out.put("\x1b]8;;\x1b\\");
    }
    out.put("\n");
    emit(out);
    linked
}

/// Only a plain https URL is written into an OSC 8 escape sequence.
fn linkable(link: &str) -> bool {
    link.starts_with("https://") && link.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// A problem report on standard error: what failed, then how to proceed.
pub fn problem(kind: NoteKind, message: &str, hint: &str) {
    let colored = caps().color_err;
    let tone = if kind == NoteKind::Warning { Tone::Warning } else { Tone::Danger };
    let mark = if kind == NoteKind::Warning { "!" } else { "×" };
    let width = columns();
    let content = wrapped(&safe(message), width, 4);
    let on = |t: Tone| if colored { code(t) } else { "" };
    let off = if colored { "\x1b[0m" } else { "" };
    eprint(&format!("  {}{}{} {}{}{}\n", on(tone), mark, off, on(Tone::Bold), content, off));
    if hint.is_empty() {
        return;
    }
    // The hint says what to do next, so it is plain text, not dimmed.
    let advice = wrapped(&safe(hint), width, 4);
    eprint(&format!("    {advice}\n"));
}

pub fn warn(message: &str) {
    problem(NoteKind::Warning, message, "");
}

/// Library diagnostics stay on standard error and never overwrite a live
/// progress line.
pub fn diagnostic(severe: bool, message: &str) {
    let clean = message.trim_matches(|c| c == ' ' || c == '\r' || c == '\n');
    if clean.is_empty() {
        return;
    }
    problem(if severe { NoteKind::Failure } else { NoteKind::Warning }, &format!("libalpm: {clean}"), "");
}

const LABEL_WIDTH: usize = 20;

/// A labelled value. Long values wrap beneath themselves, not the label.
pub fn field(label: &str, value: &str) {
    field_lines(label, &[if value.is_empty() { "none" } else { value }]);
}

/// A labelled value that is omitted when empty.
pub fn field_optional(label: &str, value: &str) {
    if !value.is_empty() {
        field_lines(label, &[value]);
    }
}

/// A labelled list rendered as one wrapped line; omitted when empty.
pub fn field_list<S: AsRef<str>>(label: &str, values: &[S]) {
    if values.is_empty() {
        return;
    }
    let joined = values.iter().map(|v| v.as_ref()).collect::<Vec<_>>().join("  ");
    field_lines(label, &[joined.as_str()]);
}

/// A labelled value that must stay whole, such as a path or a digest. It
/// moves beneath its label when it does not fit beside it, and runs past the
/// edge rather than wrapping inside the token.
pub fn field_raw(label: &str, value: &str) {
    let width = columns();
    let clean = safe(value);
    let mut out = Out::new();
    out.pad(2);
    out.paint(Tone::Muted, label);
    if width >= 56 && 2 + LABEL_WIDTH + display_width(&clean) <= width {
        out.pad(LABEL_WIDTH.saturating_sub(display_width(label)));
    } else {
        out.put("\n");
        out.pad(4);
    }
    out.put(&clean);
    out.put("\n");
    emit(out);
}

/// A labelled value with one entry per line.
pub fn field_lines<S: AsRef<str>>(label: &str, values: &[S]) {
    if values.is_empty() {
        return;
    }
    let width = columns();
    let stacked = width < 56;
    let indent = if stacked { 4 } else { 2 + LABEL_WIDTH };
    let mut out = Out::new();
    out.pad(2);
    out.paint(Tone::Muted, label);
    if stacked {
        out.put("\n");
    }
    for (i, value) in values.iter().enumerate() {
        let content = wrapped(&safe(value.as_ref()), width, indent);
        if stacked || i > 0 {
            out.pad(indent);
        } else {
            out.pad(LABEL_WIDTH.saturating_sub(display_width(label)));
        }
        out.put(&content);
        out.put("\n");
    }
    emit(out);
}

/// A term, an optional secondary term, and a description that wraps beneath
/// itself. Widths are supplied so that separate groups share one alignment.
pub fn define(term: &str, extra: &str, description: &str, term_width: usize, extra_width: usize) {
    let width = columns();
    let offset = 2 + term_width + 2 + if extra_width > 0 { extra_width + 2 } else { 0 };
    let mut out = Out::new();
    out.pad(2);
    out.paint(Tone::Bold, term);
    if width < offset + 24 {
        if !extra.is_empty() {
            out.put("  ");
            out.paint(Tone::Muted, extra);
        }
        out.put("\n");
        out.pad(6);
        out.put(&wrapped(description, width, 6));
        out.put("\n");
        return emit(out);
    }
    out.pad((2 + term_width).saturating_sub(display_width(term)));
    if extra_width > 0 {
        out.paint(Tone::Muted, extra);
        out.pad((2 + extra_width).saturating_sub(display_width(extra)));
    }
    out.put(&wrapped(description, width, offset));
    out.put("\n");
    emit(out);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub title: &'static str,
    pub tone: Tone,
    pub alignment: Align,
    /// What precedes this cell when a narrow terminal stacks the row, such as
    /// " → " between a current and a new version.
    pub joiner: &'static str,
    /// Names the cell when a stacked row loses its column title, such as
    /// "updated" before a bare date.
    pub label: &'static str,
}

impl Column {
    pub const fn new(title: &'static str) -> Column {
        Column { title, tone: Tone::Plain, alignment: Align::Left, joiner: " · ", label: "" }
    }
    pub const fn tone(mut self, tone: Tone) -> Column {
        self.tone = tone;
        self
    }
    pub const fn right(mut self) -> Column {
        self.alignment = Align::Right;
        self
    }
    pub const fn joiner(mut self, joiner: &'static str) -> Column {
        self.joiner = joiner;
        self
    }
    pub const fn label(mut self, label: &'static str) -> Column {
        self.label = label;
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Cell {
    pub text: String,
    pub tone: Option<Tone>,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Cell {
        Cell { text: text.into(), tone: None }
    }
    pub fn toned(text: impl Into<String>, tone: Tone) -> Cell {
        Cell { text: text.into(), tone: Some(tone) }
    }
    pub fn maybe(text: impl Into<String>, tone: Option<Tone>) -> Cell {
        Cell { text: text.into(), tone }
    }
}

/// Rows aligned in columns. On a terminal too narrow for columns at all,
/// each row is stacked: its first cell, then the rest beneath it.
pub struct Table {
    pub columns: Vec<Column>,
    rows: Vec<Vec<Cell>>,
    indent: usize,
}

impl Table {
    pub fn new(columns: &[Column]) -> Table {
        Table { columns: columns.to_vec(), rows: Vec::new(), indent: 2 }
    }

    pub fn add(&mut self, cells: Vec<Cell>) {
        debug_assert_eq!(cells.len(), self.columns.len());
        self.rows.push(cells.into_iter().map(|c| Cell { text: safe(&c.text), tone: c.tone }).collect());
    }

    pub fn add_text<S: AsRef<str>>(&mut self, values: &[S]) {
        debug_assert_eq!(values.len(), self.columns.len());
        self.rows.push(values.iter().map(|v| Cell::new(safe(v.as_ref()))).collect());
    }

    /// Column widths for a terminal `available` cells wide, or None when the
    /// rows must be stacked instead. If a few outlying rows are all that keep
    /// the table from fitting, the widest columns give way and those rows
    /// overflow on their own lines: one long value neither breaks the layout
    /// for every row nor is ever cut.
    fn layout(&self, available: usize) -> Option<Vec<usize>> {
        let mut result: Vec<usize> = self.columns.iter().map(|c| display_width(c.title)).collect();
        let cell_widths: Vec<Vec<usize>> =
            self.rows.iter().map(|row| row.iter().map(|c| display_width(&c.text)).collect()).collect();
        for row in &cell_widths {
            for (w, cell) in result.iter_mut().zip(row) {
                *w = (*w).max(*cell);
            }
        }
        let mut total = self.indent + 2 * (self.columns.len() - 1);
        let mut floors = Vec::with_capacity(result.len());
        for (column, width) in self.columns.iter().zip(&result) {
            floors.push((*width).min(display_width(column.title).max(8)));
            total += width;
        }
        if total <= available {
            return Some(result);
        }
        while total > available {
            let mut widest: Option<usize> = None;
            for (i, (width, floor)) in result.iter().zip(&floors).enumerate() {
                if width > floor && widest.is_none_or(|w| *width > result[w]) {
                    widest = Some(i);
                }
            }
            result[widest?] -= 1;
            total -= 1;
        }
        // Worth it only when the long values are outliers.
        let overflowing = cell_widths.iter().filter(|row| row.iter().zip(&result).any(|(c, w)| c > w)).count();
        if overflowing * 8 > self.rows.len() {
            None
        } else {
            Some(result)
        }
    }

    #[cfg(test)]
    pub fn fits(&self, width: usize) -> bool {
        self.layout(width).is_some()
    }

    pub fn render(&self) {
        if self.rows.is_empty() {
            return;
        }
        let mut out = Out::new();
        match self.layout(columns()) {
            None => self.render_stacked(&mut out),
            Some(widths) => {
                if self.columns.iter().any(|c| !c.title.is_empty()) {
                    let titles: Vec<Cell> =
                        self.columns.iter().map(|c| Cell::toned(c.title, Tone::Underline)).collect();
                    self.render_row(&mut out, &titles, &widths);
                }
                for row in &self.rows {
                    self.render_row(&mut out, row, &widths);
                }
            }
        }
        emit(out);
    }

    fn render_row(&self, out: &mut Out, row: &[Cell], widths: &[usize]) {
        // Nothing is padded after the last cell with content.
        let filled = row.iter().rposition(|c| !c.text.is_empty()).map_or(0, |i| i + 1);
        out.pad(self.indent);
        for i in 0..filled {
            let (cell, column, width) = (&row[i], &self.columns[i], widths[i]);
            let gap = width.saturating_sub(display_width(&cell.text));
            if column.alignment == Align::Right {
                out.pad(gap);
            }
            out.paint(cell.tone.unwrap_or(column.tone), &cell.text);
            if i + 1 < filled {
                out.pad(2 + if column.alignment == Align::Left { gap } else { 0 });
            }
        }
        out.put("\n");
    }

    fn render_stacked(&self, out: &mut Out) {
        let width = columns();
        for row in &self.rows {
            out.pad(self.indent);
            out.paint(row[0].tone.unwrap_or(self.columns[0].tone), &wrapped(&row[0].text, width, self.indent));
            out.put("\n");
            // The other cells follow on one line, each in its own tone and
            // labelled where a bare value would be ambiguous, wrapping only
            // between cells.
            let indent = self.indent + 2;
            let mut used = indent;
            let mut first = true;
            for (cell, column) in row[1..].iter().zip(&self.columns[1..]) {
                if cell.text.is_empty() {
                    continue;
                }
                let joiner = if first { "" } else { column.joiner };
                let label_cells = if column.label.is_empty() { 0 } else { display_width(column.label) + 1 };
                let cells = display_width(joiner) + label_cells + display_width(&cell.text);
                if first {
                    out.pad(indent);
                } else if used + cells > width {
                    // A wrapped cell starts its own line; only a joiner that
                    // means something, such as an arrow, is carried over.
                    out.put("\n");
                    out.pad(indent);
                    used = indent;
                    let lead = joiner.trim_matches(' ');
                    if lead != "·" {
                        out.paint(Tone::Muted, lead);
                        out.put(" ");
                        used += display_width(lead) + 1;
                    }
                } else {
                    out.paint(Tone::Muted, joiner);
                    used += display_width(joiner);
                }
                if !column.label.is_empty() {
                    out.paint(Tone::Muted, column.label);
                    out.put(" ");
                    used += label_cells;
                }
                out.paint(cell.tone.unwrap_or(column.tone), &cell.text);
                used += display_width(&cell.text);
                first = false;
            }
            if !first {
                out.put("\n");
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Badge {
    pub text: String,
    pub tone: Tone,
}

/// A package in a result list: identity, badges, description and a muted
/// metadata line.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub index: Option<usize>,
    pub source: String,
    pub name: String,
    /// A web page for the package; styled output makes the name a link to it.
    pub link: String,
    pub version: String,
    pub badges: Vec<Badge>,
    pub description: String,
    pub meta: String,
}

pub fn entry(e: &Entry) {
    let width = columns();
    let source = safe(&e.source);
    let name = safe(&e.name);
    let version = safe(&e.version);
    let mut out = Out::new();
    let mut used = 2;
    if let Some(n) = e.index {
        let number = format!("{n:>4} ");
        out.paint(Tone::Muted, &number);
        used = display_width(&number);
    } else {
        out.pad(2);
    }
    let body = used + 4;
    out.paint(if source == "aur" { Tone::Special } else { Tone::Accent }, &source);
    out.paint(Tone::Muted, "/");
    // An OSC 8 hyperlink takes no columns.
    let linked = out.color && linkable(&e.link);
    if linked {
        out.put("\x1b]8;;");
        out.put(&e.link);
        out.put("\x1b\\");
    }
    out.paint(Tone::Bold, &name);
    if linked {
        out.put("\x1b]8;;\x1b\\");
    }
    used += display_width(&source) + 1 + display_width(&name);
    if used + 1 + display_width(&version) > width {
        out.put("\n");
        out.pad(body);
        used = body;
    } else {
        out.put(" ");
        used += 1;
    }
    out.paint(Tone::Success, &version);
    used += display_width(&version);
    for badge in &e.badges {
        let label = safe(&badge.text);
        if used + 2 + display_width(&label) > width {
            out.put("\n");
            out.pad(body);
            used = body;
        } else {
            out.put("  ");
            used += 2;
        }
        out.paint(badge.tone, &label);
        used += display_width(&label);
    }
    out.put("\n");
    for (value, tone) in [(&e.description, Tone::Plain), (&e.meta, Tone::Muted)] {
        if value.is_empty() {
            continue;
        }
        out.pad(body);
        out.paint(tone, &wrapped(&safe(value), width, body));
        out.put("\n");
    }
    emit(out);
}

/// A bar whose length is `part / whole` of `width` cells; any positive part
/// shows at least one cell.
pub fn bar(part: i64, whole: i64, width: usize) -> String {
    if whole <= 0 || part <= 0 || width == 0 {
        return String::new();
    }
    let scaled = (part as i128 * width as i128) / whole as i128;
    let cells = scaled.clamp(1, width as i128) as usize;
    "━".repeat(cells)
}

// ── Progress ────────────────────────────────────────────────────────────────

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A transient line saying what is under way, such as a clone. The next
/// output replaces it. Shown only on a live terminal.
pub fn status(message: &str) {
    if !caps().live {
        return;
    }
    let mut out = Out::new();
    out.put("\r\x1b[2K  ");
    out.paint(Tone::Muted, &clipped(&safe(message), columns().saturating_sub(4)));
    let mut s = state();
    begin(&mut s);
    s.pending.extend_from_slice(out.buf.as_bytes());
    s.progress_active = true;
    end(&mut s);
}

/// A single live status line; a completed step leaves one finished line.
pub fn progress(label: &str, name: &str, percent: usize, current: usize, total: usize) {
    let value = percent.min(100);
    let key = progress_key(label, name, current, total);
    let now = now_millis();
    {
        let mut s = state();
        let same = s.progress_key == Some(key);
        if same && (s.progress_percent == value || (value < 100 && now - s.progress_time < 100)) {
            return;
        }
        s.progress_key = Some(key);
        s.progress_percent = value;
        s.progress_time = now;
    }
    let message = safe(&format!("{}{}{}", label, if name.is_empty() { "" } else { " " }, name));
    let width = columns();
    if !caps().live || width < 40 {
        if value == 100 {
            note(NoteKind::Success, &message);
        }
        return;
    }
    let position = if total > 0 { format!(" · {current}/{total}") } else { String::new() };
    let bar_width = (width / 5).min(18);
    let available = width.saturating_sub(bar_width + display_width(&position) + 11);
    let caption = clipped(&message, available);
    let filled = value * bar_width / 100;
    let mut out = Out::new();
    out.put("\r\x1b[2K  ");
    out.put(&caption);
    out.pad(available.saturating_sub(display_width(&caption)));
    out.put("  ");
    if out.color {
        out.put(code(Tone::Accent));
    }
    out.repeat("━", filled);
    if out.color {
        out.put("\x1b[0m\x1b[90m");
    }
    out.repeat("─", bar_width - filled);
    out.reset();
    out.put(&format!(" {value:>3}%"));
    out.paint(Tone::Muted, &position);
    if value == 100 {
        out.put("\n");
    }
    let mut s = state();
    begin(&mut s);
    s.pending.extend_from_slice(out.buf.as_bytes());
    s.progress_active = value < 100;
    end(&mut s);
}

fn progress_key(label: &str, name: &str, current: usize, total: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (label, name, current, total).hash(&mut h);
    h.finish()
}

// ── Prompts ─────────────────────────────────────────────────────────────────

/// Set by --noconfirm, and in the worker by the request: prompts are answered
/// without reading input, and no terminal is required.
static UNATTENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_unattended(on: bool) {
    UNATTENDED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn unattended() -> bool {
    UNATTENDED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Show a question with the reply given on the user's behalf, so an
/// unattended log records what was decided.
pub fn assumed(prompt: &str, reply: &str) {
    let content = wrapped(&safe(prompt.trim_matches(|c| c == ' ' || c == '\n')), columns().saturating_sub(2), 4);
    let mut out = Out::new();
    out.pad(2);
    out.paint(Tone::Warning, "?");
    out.put(" ");
    out.paint(Tone::Bold, &content);
    out.put(" ");
    out.put(reply);
    out.put(" (--noconfirm)\n");
    emit(out);
    flush();
}

/// Ask a question and read one line from standard input. End of input is
/// never treated as consent.
pub fn answer(prompt: &str) -> Result<String> {
    let content = wrapped(&safe(prompt.trim_matches(|c| c == ' ' || c == '\n')), columns().saturating_sub(2), 4);
    let mut out = Out::new();
    out.pad(2);
    out.paint(Tone::Warning, "?");
    out.put(" ");
    out.paint(Tone::Bold, &content);
    out.put(" ");
    emit(out);
    flush();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        signals::check()?;
        let mut fds = [libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 }];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 1, 100) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if ready == 0 {
            continue;
        }
        signals::check()?;
        let mut byte = [0u8; 1];
        let n = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if n == 0 {
            // No newline was echoed; leave the prompt's line.
            blank();
            return Err(Kind::ConfirmationRequired.into());
        }
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() == 4096 {
            return Err(Kind::InputTooLong.into());
        }
        buf.push(byte[0]);
    }
    let reply = String::from_utf8_lossy(&buf).into_owned();
    Ok(reply.trim_matches(|c| c == ' ' || c == '\r' || c == '\t').to_string())
}

/// Whether a reply to `prompt` means yes: y or yes, or an empty reply when
/// the prompt's default is yes ([Y/n]).
fn yes(prompt: &str, reply: &str) -> bool {
    reply.eq_ignore_ascii_case("y")
        || reply.eq_ignore_ascii_case("yes")
        || (reply.is_empty() && prompt.ends_with("[Y/n]"))
}

/// Ask until the reply is yes or no, as pacman does, so a typo cannot
/// cancel a long run. Ok(true) for yes.
fn ask(prompt: &str) -> Result<bool> {
    loop {
        let reply = answer(prompt)?;
        if yes(prompt, &reply) {
            return Ok(true);
        }
        if reply.is_empty() || reply.eq_ignore_ascii_case("n") || reply.eq_ignore_ascii_case("no") {
            return Ok(false);
        }
        note(NoteKind::Warning, "Answer y or n.");
    }
}

/// Ask a yes/no question. Input that ends first counts as no.
pub fn confirm(prompt: &str) -> bool {
    ask(prompt).unwrap_or(false)
}

/// Ask, and refuse unless the answer is yes. A prompt ending in [Y/n] takes
/// an empty answer as yes. Input that ends first is ConfirmationRequired, as
/// at any other prompt. Unattended, the answer is yes.
pub fn require(prompt: &str) -> Result<()> {
    if unattended() {
        assumed(prompt, "yes");
        return Ok(());
    }
    if ask(prompt)? {
        Ok(())
    } else {
        Err(Kind::Declined.into())
    }
}

// ── Values ──────────────────────────────────────────────────────────────────

/// Binary size with a unit that keeps two or three significant digits.
pub fn size(bytes: i64) -> String {
    let magnitude = bytes.unsigned_abs() as f64;
    let sign = if bytes < 0 { "-" } else { "" };
    let kib = 1024.0;
    if magnitude < kib {
        format!("{sign}{} B", bytes.unsigned_abs())
    } else if magnitude < kib * kib {
        format!("{sign}{} KiB", fixed(magnitude / kib, 1))
    } else if magnitude < kib * kib * kib {
        format!("{sign}{} MiB", fixed(magnitude / (kib * kib), 1))
    } else {
        format!("{sign}{} GiB", fixed(magnitude / (kib * kib * kib), 2))
    }
}

/// Fixed-point formatting that rounds exact halves away from zero, as zap's
/// original formatter did; other values round to nearest.
pub fn fixed(value: f64, digits: usize) -> String {
    let scale = 10f64.powi(digits as i32);
    let scaled = value * scale;
    // A value exactly halfway between two outputs (such as 1.25 at one
    // digit) is exactly representable; round it up rather than to even.
    if scaled.fract() == 0.5 {
        return format!("{:.*}", digits, (scaled.trunc() + 1.0) / scale);
    }
    format!("{value:.digits$}")
}

/// A size change with an explicit sign.
pub fn size_delta(bytes: i64) -> String {
    if bytes <= 0 {
        size(bytes)
    } else {
        format!("+{}", size(bytes))
    }
}

pub fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{} {}", grouped(n), if n == 1 { singular } else { plural })
}

/// A number with its thousands grouped: 492,103.
pub fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Civil date (year, month, day) and time of day for a Unix timestamp in UTC.
fn civil(timestamp: i64) -> (i64, u32, u32, u32, u32) {
    let days = timestamp.div_euclid(86400);
    let secs = timestamp.rem_euclid(86400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32)
}

/// Calendar date in UTC.
pub fn date(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "unknown".into();
    }
    let (y, m, d, _, _) = civil(timestamp);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Date and time in UTC.
#[cfg(test)]
pub fn datetime(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "unknown".into();
    }
    let (y, m, d, hh, mm) = civil(timestamp);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02} UTC")
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Coarse distance between two timestamps, for orientation beside a date.
pub fn age(timestamp: i64, now: i64) -> String {
    if timestamp <= 0 {
        return "unknown".into();
    }
    let days = (now - timestamp).max(0) / 86400;
    match days {
        0 => "today".into(),
        1 => "yesterday".into(),
        d if d < 14 => format!("{d} days ago"),
        d if d < 63 => format!("{} weeks ago", d / 7),
        d if d < 730 => format!("{} months ago", d / 30),
        d => format!("{} years ago", d / 365),
    }
}

/// A date with its age, such as `2026-07-16 · 4 months ago`.
pub fn dated(timestamp: i64) -> String {
    if timestamp <= 0 {
        return "unknown".into();
    }
    format!("{} · {}", date(timestamp), age(timestamp, now()))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(width: usize) {
        set_caps(Some(Caps { width: Some(width), ..Caps::default() }));
        begin_capture();
    }

    #[test]
    fn counts_group_thousands() {
        assert_eq!(count(1, "file", "files"), "1 file");
        assert_eq!(count(999, "file", "files"), "999 files");
        assert_eq!(count(492103, "file", "files"), "492,103 files");
        assert_eq!(grouped(1234567), "1,234,567");
    }

    #[test]
    fn remote_metadata_cannot_inject_terminal_escapes() {
        assert_eq!("abc [31m xyz ", safe("abc\x1b[31m\nxyz\x7f"));
        // C1 controls include an 8-bit CSI that some terminals honour.
        assert_eq!("a  [31mb", safe("a\u{9b}[31mb"));
        // Bidirectional overrides would let reviewed source display misleadingly.
        assert_eq!("a\u{fffd}b\u{fffd}c", safe("a\u{202e}b\u{2066}c"));
        assert_eq!("a\u{fffd}b??c", safe("a\u{200f}b\u{061c}c"));
        assert_eq!("a?b", safe_bytes(b"a\xffb"));
        assert_eq!("ok?", safe_bytes(b"ok\xe2"));
        assert_eq!("日本語 café", safe("日本語 café"));
    }

    #[test]
    fn terminal_wrapping_keeps_words_whole_and_utf8_intact() {
        let _g = test_lock();
        for (input, width, indent, expected) in [
            ("one two three", 11, 4, "one two\n    three"),
            ("one two three", 12, 4, "one two\n    three"),
            // A path longer than the line stays whole, so it can be copied.
            ("abcdefghij", 8, 2, "abcdefghij"),
            ("see /a/long/path ok", 8, 2, "see\n  /a/long/path\n  ok"),
            ("café café", 7, 2, "café\n  café"),
            ("  one   two  ", 10, 2, "one\n  two"),
            ("abc", 2, 4, "abc"),
            ("", 80, 4, ""),
        ] {
            assert_eq!(expected, wrapped(input, width, indent), "{input:?} at {width}");
        }
    }

    #[test]
    fn status_captions_keep_valid_unicode_within_a_terminal_cell_budget() {
        let _g = test_lock();
        for caption in ["a long package name", "日本語のパッケージ", "café e\u{0301}clair"] {
            for width in 0..12 {
                assert!(display_width(&clipped(caption, width)) <= width);
            }
        }
    }

    #[test]
    fn tables_align_unicode_sanitize_cells_and_stack_when_the_terminal_is_narrow() {
        let _g = test_lock();
        let mut table = Table::new(&[Column::new("Package"), Column::new("Size").right(), Column::new("Change")]);
        table.add_text(&["日本語-package\x1b", "4.3 MiB", "upgrade"]);
        table.add_text(&["b", "12 B", "remove"]);
        assert!(!table.fits(24));
        assert!(table.fits(40));
        rendered(40);
        table.render();
        assert_eq!(
            "  Package             Size  Change\n  日本語-package   4.3 MiB  upgrade\n  b                   12 B  remove\n",
            end_capture()
        );
        rendered(24);
        table.render();
        assert_eq!("  日本語-package\n    4.3 MiB · upgrade\n  b\n    12 B · remove\n", end_capture());
        set_caps(None);
    }

    #[test]
    fn one_outlying_row_overflows_instead_of_forcing_every_row_to_stack() {
        let _g = test_lock();
        let mut table = Table::new(&[Column::new("Package"), Column::new("Version")]);
        for _ in 0..8 {
            table.add_text(&["short", "1.0-1"]);
        }
        table.add_text(&["a-package-with-a-long-name", "2.0-1"]);
        rendered(24);
        table.render();
        let output = end_capture();
        assert!(output.starts_with("  Package        Version\n  short          1.0-1\n"));
        assert!(output.ends_with("  a-package-with-a-long-name  2.0-1\n"));
        let mut ragged = Table::new(&[Column::new("Package"), Column::new("Version")]);
        ragged.add_text(&["short", "1.0-1"]);
        ragged.add_text(&["a-package-with-a-long-name", "2.0-1"]);
        assert!(!ragged.fits(24));
        assert!(ragged.fits(40));
        set_caps(None);
    }

    #[test]
    fn fields_wrap_beneath_their_value_and_skip_empty_optional_values() {
        let _g = test_lock();
        rendered(60);
        field("Depends on", "glibc  libgcc  pcre2  a-rather-long-name");
        field("Groups", "");
        field_optional("Conflicts", "");
        field_list::<&str>("Provides", &[]);
        field_lines("Optional", &["sudo: elevation", "doas: elevation"]);
        assert_eq!(
            "  Depends on          glibc  libgcc  pcre2\n                      a-rather-long-name\n  Groups              none\n  Optional            sudo: elevation\n                      doas: elevation\n",
            end_capture()
        );
        rendered(30);
        field("Depends on", "glibc  libgcc");
        assert_eq!("  Depends on\n    glibc  libgcc\n", end_capture());
        set_caps(None);
    }

    #[test]
    fn entries_keep_identity_badges_and_metadata_readable_at_any_width() {
        let _g = test_lock();
        let sample = Entry {
            index: Some(3),
            source: "extra".into(),
            name: "ripgrep".into(),
            version: "15.2.0-1".into(),
            badges: vec![Badge { text: "✓ installed".into(), tone: Tone::Success }],
            description: "A search tool".into(),
            meta: "4.3 MiB · built 2026-07-16".into(),
            ..Entry::default()
        };
        rendered(80);
        entry(&sample);
        assert_eq!(
            "   3 extra/ripgrep 15.2.0-1  ✓ installed\n         A search tool\n         4.3 MiB · built 2026-07-16\n",
            end_capture()
        );
        rendered(24);
        entry(&sample);
        assert_eq!(
            "   3 extra/ripgrep\n         15.2.0-1\n         ✓ installed\n         A search tool\n         4.3 MiB · built\n         2026-07-16\n",
            end_capture()
        );
        set_caps(None);
    }

    #[test]
    fn glyphs_fall_back_to_single_cell_ascii_without_changing_alignment() {
        let _g = test_lock();
        set_caps(Some(Caps { width: Some(40), unicode: false, ..Caps::default() }));
        begin_capture();
        heading("Updates", "2 packages");
        note(NoteKind::Success, "Done · 1 → 2");
        let output = end_capture();
        assert_eq!("\n> Updates  2 packages\n  + Done - 1 > 2\n", output);
        assert!(output.is_ascii());
        set_caps(None);
    }

    #[test]
    fn an_8_bit_locale_receives_ascii_only() {
        let _g = test_lock();
        set_caps(Some(Caps { width: Some(40), unicode: false, utf8: false, ..Caps::default() }));
        begin_capture();
        // U+011B is C4 9B; 0x9B alone is CSI on an 8-bit terminal.
        text("caf\u{e9} \u{11b} \u{65e5}", 2, Tone::Plain);
        assert_eq!("  caf? ? ?\n", end_capture());
        set_caps(None);
    }

    #[test]
    fn headings_never_cut_their_title_or_detail_and_raw_fields_keep_tokens_whole() {
        let _g = test_lock();
        rendered(44);
        heading("Review 1/2 · some-package", "1.0-1");
        heading("Review 1/2 · a-package-with-a-long-name", "2026.11.14.r123.gabcdef-1");
        assert_eq!(
            "\n▌ Review 1/2 · some-package  1.0-1\n\n▌ Review 1/2 · a-package-with-a-long-name\n  2026.11.14.r123.gabcdef-1\n",
            end_capture()
        );
        let digest = "9400afb49bd60984062972cbaf220808f39011fcc5946c089d6efccc87ef047d";
        rendered(90);
        field_raw("SHA-256", digest);
        set_caps(Some(Caps { width: Some(60), ..Caps::default() }));
        field_raw("SHA-256", digest);
        assert_eq!(format!("  SHA-256             {digest}\n  SHA-256\n    {digest}\n"), end_capture());
        set_caps(None);
    }

    #[test]
    fn stacked_rows_join_a_version_change_with_an_arrow() {
        let _g = test_lock();
        let columns =
            [Column::new("Package"), Column::new("Current"), Column::new("New").joiner(" → "), Column::new("Source")];
        let mut table = Table::new(&columns);
        table.add_text(&["cpak-bin", "2.14.6-1", "2.14.18-1", "aur"]);
        table.add_text(&["a-much-longer-package-name", "1-1", "2-1", "aur"]);
        rendered(30);
        table.render();
        assert_eq!(
            "  cpak-bin\n    2.14.6-1 → 2.14.18-1 · aur\n  a-much-longer-package-name\n    1-1 → 2-1 · aur\n",
            end_capture()
        );
        let mut narrow = Table::new(&columns);
        narrow.add_text(&["cpak-bin", "2.14.6-1", "2.14.18-1", "aur"]);
        rendered(18);
        narrow.render();
        assert_eq!("  cpak-bin\n    2.14.6-1\n    → 2.14.18-1\n    aur\n", end_capture());
        set_caps(None);
    }

    #[test]
    fn sizes_counts_and_ages_read_naturally() {
        assert_eq!("0 B", size(0));
        assert_eq!("512 B", size(512));
        assert_eq!("1.5 KiB", size(1536));
        assert_eq!("4.3 MiB", size(4_508_877));
        assert_eq!("1.71 GiB", size(1_838_887_731));
        assert_eq!("-2.0 MiB", size_delta(-2 * 1024 * 1024));
        assert_eq!("+2.0 MiB", size_delta(2 * 1024 * 1024));
        assert_eq!("1 package", count(1, "package", "packages"));
        assert_eq!("0 packages", count(0, "package", "packages"));
        let day = 86400;
        let now = 1_800_000_000;
        assert_eq!("today", age(now - 3600, now));
        assert_eq!("today", age(now + 3600, now));
        assert_eq!("yesterday", age(now - day, now));
        assert_eq!("13 days ago", age(now - 13 * day, now));
        assert_eq!("3 weeks ago", age(now - 21 * day, now));
        assert_eq!("4 months ago", age(now - 120 * day, now));
        assert_eq!("3 years ago", age(now - 3 * 365 * day, now));
        assert_eq!("unknown", age(0, now));
        assert_eq!("2025-06-15", date(1750000000));
        assert_eq!("2025-06-15 15:06 UTC", datetime(1750000000));
        assert_eq!("2024-02-29", date(1709164800));
        assert_eq!("unknown", date(0));
    }

    #[test]
    fn repeated_progress_identities_distinguish_phase_and_count() {
        let name = "日本語-terminal-with-an-extraordinarily-long-name";
        let key = progress_key("Upgrading", name, 1, 3);
        assert_eq!(key, progress_key("Upgrading", name, 1, 3));
        assert_ne!(key, progress_key("Installing", name, 1, 3));
        assert_ne!(key, progress_key("Upgrading", name, 2, 3));
        assert_ne!(key, progress_key("Upgrading", name, 1, 4));
    }

    #[test]
    fn styled_entries_link_the_name_only_to_a_plain_https_page() {
        let _g = test_lock();
        set_caps(Some(Caps { width: Some(80), color: true, ..Caps::default() }));
        begin_capture();
        entry(&Entry {
            source: "aur".into(),
            name: "zap".into(),
            link: "https://aur.archlinux.org/packages/zap".into(),
            version: "1-1".into(),
            ..Entry::default()
        });
        assert!(end_capture().contains("\x1b]8;;https://aur.archlinux.org/packages/zap\x1b\\"));
        begin_capture();
        entry(&Entry {
            source: "aur".into(),
            name: "zap".into(),
            link: "https://evil\x07\x1b]8;;x".into(),
            version: "1-1".into(),
            ..Entry::default()
        });
        assert!(!end_capture().contains("\x1b]8"));
        set_caps(None);
    }
}
