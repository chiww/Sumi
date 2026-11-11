//! Sumi's file viewer and editor, the pane files from the left panel open
//! in (after Warp's Markdown viewer and code editor panes).
//!
//! Markdown opens rendered, with Ctrl+P switching to the editable source;
//! every other text file opens in the editor, highlighted with syntect.
//! The file is watched: an outside change reloads it when there are no
//! unsaved edits.
//!
//! It runs as a terminal program in its own pane (`kaku-gui --sumi-view
//! <path>`), so splitting, zooming, closing and session restore come from
//! the pane machinery. It announces itself with the `sumi_viewer` user var;
//! the GUI then routes Cmd+S/Z/F/C/X/A to it as their Ctrl forms and sends
//! further files as a marked paste (`left_panel::viewer_open_request`).

pub(crate) mod buffer;
pub(crate) mod theme;

use self::buffer::{char_width, col_for_x, display_x, Buffer, Pos};
use crate::overlay::ai_chat::markdown::{parse_markdown_blocks, tokenize_inline, wrap_segments};
use crate::overlay::ai_chat::{DiffKind, InlineSpan, InlineStyle, MdBlock};
use base64::Engine as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use syntect::highlighting::{HighlightIterator, HighlightState, Highlighter, Theme, ThemeSet};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use termwiz::caps::Capabilities;
use termwiz::cell::{unicode_column_width, CellAttributes, Intensity, Underline};
use termwiz::input::{InputEvent, KeyCode, KeyEvent, Modifiers, MouseButtons, MouseEvent};
use termwiz::surface::{Change, CursorVisibility, Position};
use termwiz::terminal::{Terminal, UnixTerminal};

/// The argument that starts the viewer instead of the GUI.
pub(crate) const VIEW_FLAG: &str = "--sumi-view";
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Highlighting stops for files this long; plain text keeps them fast.
const MAX_HIGHLIGHT_LINES: usize = 50_000;
const POLL: Duration = Duration::from_millis(400);
const WATCH_INTERVAL: Duration = Duration::from_millis(900);
const HEADER_ROWS: usize = 2;

/// Parses a marked paste from the GUI into a path and an optional line.
pub(crate) fn parse_open_request(text: &str) -> Option<(PathBuf, Option<usize>)> {
    let rest = text.strip_prefix(crate::termwindow::left_panel::VIEWER_OPEN_PREFIX)?;
    if let Some((path, line)) = rest.rsplit_once(':') {
        if let Ok(line) = line.parse::<usize>() {
            return Some((PathBuf::from(path), Some(line)));
        }
    }
    Some((PathBuf::from(rest), None))
}

pub(crate) fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown" | "mdx" | "mdown")
    )
}

/// True when the start of the file holds a NUL byte, like Git's check.
pub(crate) fn looks_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 8192];
    let n = file.read(&mut head).unwrap_or(0);
    head[..n].contains(&0)
}

fn t(key: &str) -> String {
    rust_i18n::t!(key).into_owned()
}

// ---------- Highlighting ----------

struct Syntect {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

fn syntect() -> &'static Syntect {
    static INSTANCE: std::sync::OnceLock<Syntect> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| Syntect {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
    })
}

/// The syntect theme the current viewer theme uses for code.
fn code_theme() -> &'static Theme {
    let st = syntect();
    st.themes
        .themes
        .get(theme::syntax_theme())
        .or_else(|| st.themes.themes.values().next())
        .expect("syntect ships themes")
}

/// Highlights a fenced code block in the current theme. Diff lines keep
/// their own colors, so a block with any of them is left plain.
fn highlight_fence(lines: &[(&str, DiffKind)], lang: &str) -> Vec<(Vec<InlineSpan>, DiffKind)> {
    let plain = |text: &str, diff: DiffKind| {
        (
            vec![InlineSpan {
                text: text.to_string(),
                style: InlineStyle::Plain,
            }],
            diff,
        )
    };
    let st = syntect();
    let syntax = st.syntaxes.find_syntax_by_token(lang);
    let Some(syntax) = syntax.filter(|_| lines.iter().all(|(_, d)| *d == DiffKind::None)) else {
        return lines.iter().map(|(t, d)| plain(t, *d)).collect();
    };
    let mut highlighter = syntect::easy::HighlightLines::new(syntax, code_theme());
    lines
        .iter()
        .map(|(text, diff)| {
            let line = format!("{text}\n");
            match highlighter.highlight_line(&line, &st.syntaxes) {
                Ok(ranges) => (
                    ranges
                        .into_iter()
                        .map(|(style, part)| InlineSpan {
                            text: part.trim_end_matches('\n').to_string(),
                            style: InlineStyle::Highlighted(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            ),
                        })
                        .filter(|span| !span.text.is_empty())
                        .collect(),
                    *diff,
                ),
                Err(_) => plain(text, *diff),
            }
        })
        .collect()
}

type Styled = Vec<(String, Option<(u8, u8, u8)>)>;

/// Highlights lines on demand, keeping the parser state at the start of
/// every line so an edit re-highlights from the changed row only.
struct LineHighlighter {
    syntax: Option<&'static SyntaxReference>,
    states: Vec<(ParseState, HighlightState)>,
    lines: Vec<Styled>,
}

impl LineHighlighter {
    fn new(path: &Path, first_line: &str) -> Self {
        let st = syntect();
        let syntax = st
            .syntaxes
            .find_syntax_for_file(path)
            .ok()
            .flatten()
            .or_else(|| st.syntaxes.find_syntax_by_first_line(first_line))
            .filter(|s| s.name != "Plain Text");
        Self {
            syntax,
            states: vec![],
            lines: vec![],
        }
    }

    fn invalidate_from(&mut self, row: usize) {
        self.states.truncate(row);
        self.lines.truncate(row);
    }

    /// Highlighted spans for `rows`, computing what is missing.
    fn ensure(&mut self, buffer: &Buffer, upto: usize) {
        let Some(syntax) = self.syntax else {
            return;
        };
        if buffer.lines.len() > MAX_HIGHLIGHT_LINES {
            return;
        }
        let st = syntect();
        let highlighter = Highlighter::new(code_theme());
        let upto = upto.min(buffer.lines.len());
        while self.lines.len() < upto {
            let row = self.lines.len();
            let (mut parse, mut highlight) = match self.states.last() {
                Some(_) if self.states.len() > row => self.states[row].clone(),
                _ => (
                    ParseState::new(syntax),
                    HighlightState::new(&highlighter, ScopeStack::new()),
                ),
            };
            if self.states.len() <= row {
                self.states.push((parse.clone(), highlight.clone()));
            }
            let line = format!("{}\n", buffer.lines[row]);
            let ops = parse.parse_line(&line, &st.syntaxes).unwrap_or_default();
            let styled: Styled = HighlightIterator::new(&mut highlight, &ops, &line, &highlighter)
                .map(|(style, text)| {
                    (
                        text.trim_end_matches('\n').to_string(),
                        Some((style.foreground.r, style.foreground.g, style.foreground.b)),
                    )
                })
                .filter(|(text, _)| !text.is_empty())
                .collect();
            self.lines.push(styled);
            self.states.push((parse, highlight));
            // `states[row + 1]` is now the state after this line.
            self.states.truncate(row + 2);
        }
    }

    fn line(&self, row: usize) -> Option<&Styled> {
        self.lines.get(row)
    }
}

// ---------- Markdown preview ----------

type Cell = (String, CellAttributes);
type DisplayLine = Vec<Cell>;

/// Plain text in the current theme.
fn attrs() -> CellAttributes {
    theme::base()
}

fn accent() -> CellAttributes {
    theme::colored(theme::current().accent)
}

fn muted() -> CellAttributes {
    theme::colored(theme::current().muted)
}

fn warn() -> CellAttributes {
    theme::colored(theme::current().warn)
}

fn code() -> CellAttributes {
    theme::colored(theme::current().code)
}

fn rgb(r: u8, g: u8, b: u8) -> CellAttributes {
    theme::rgb_text(r, g, b)
}

fn dim() -> CellAttributes {
    let mut a = attrs();
    a.set_intensity(Intensity::Half);
    a
}

fn span_attrs(style: InlineStyle, base: &CellAttributes) -> CellAttributes {
    let mut a = base.clone();
    match style {
        InlineStyle::Plain => {}
        InlineStyle::Bold => {
            a.set_intensity(Intensity::Bold);
        }
        InlineStyle::Italic => {
            a.set_italic(true);
        }
        InlineStyle::Code => {
            a = code();
        }
        InlineStyle::HeadingMarker => {
            a.set_intensity(Intensity::Half);
        }
        InlineStyle::Highlighted(r, g, b) => {
            a = rgb(r, g, b);
        }
    }
    a
}

fn spans_to_line(prefix: &[Cell], spans: &[InlineSpan], base: &CellAttributes) -> DisplayLine {
    let mut line: DisplayLine = prefix.to_vec();
    for span in spans {
        line.push((span.text.clone(), span_attrs(span.style, base)));
    }
    line
}

/// Renders Markdown into display lines `width` cells wide.
pub(crate) fn render_markdown(text: &str, width: usize) -> Vec<DisplayLine> {
    let width = width.max(10);
    let blocks = parse_markdown_blocks(text);
    let mut out: Vec<DisplayLine> = vec![];
    let mut i = 0;
    while i < blocks.len() {
        match &blocks[i] {
            MdBlock::Blank => {
                if out.last().is_some_and(|l| !l.is_empty()) {
                    out.push(vec![]);
                }
            }
            MdBlock::Heading { level, text } => {
                if out.last().is_some_and(|l| !l.is_empty()) {
                    out.push(vec![]);
                }
                let mut base = if *level <= 2 { accent() } else { attrs() };
                base.set_intensity(Intensity::Bold);
                if *level == 1 {
                    base.set_underline(Underline::Single);
                }
                let spans = tokenize_inline(text);
                for wrapped in wrap_segments(&spans, width) {
                    out.push(spans_to_line(&[], &wrapped, &base));
                }
                if *level <= 2 {
                    out.push(vec![("─".repeat(width.min(80)), dim())]);
                }
            }
            MdBlock::Paragraph(text) => {
                let spans = tokenize_inline(text);
                for wrapped in wrap_segments(&spans, width) {
                    out.push(spans_to_line(&[], &wrapped, &attrs()));
                }
            }
            MdBlock::Quote(text) => {
                let mut base = attrs();
                base.set_italic(true);
                let bar = ("▎ ".to_string(), muted());
                let spans = tokenize_inline(text);
                for wrapped in wrap_segments(&spans, width.saturating_sub(2)) {
                    out.push(spans_to_line(std::slice::from_ref(&bar), &wrapped, &base));
                }
            }
            MdBlock::ListItem { marker, text } => {
                let indent = unicode_column_width(marker, None) + 1;
                let spans = tokenize_inline(text);
                for (n, wrapped) in wrap_segments(&spans, width.saturating_sub(indent))
                    .into_iter()
                    .enumerate()
                {
                    let prefix = if n == 0 {
                        (format!("{marker} "), accent())
                    } else {
                        (" ".repeat(indent), attrs())
                    };
                    out.push(spans_to_line(&[prefix], &wrapped, &attrs()));
                }
            }
            MdBlock::CodeLine { lang, group, .. } => {
                // Gather the fence so it highlights as one block.
                let group = *group;
                let lang = lang.clone();
                let mut lines: Vec<(&str, DiffKind)> = vec![];
                while let Some(MdBlock::CodeLine {
                    text,
                    diff,
                    group: g,
                    ..
                }) = blocks.get(i)
                {
                    if *g != group {
                        break;
                    }
                    lines.push((text.as_str(), *diff));
                    i += 1;
                }
                let highlighted = highlight_fence(&lines, &lang);
                let gutter = ("│ ".to_string(), muted());
                let current = theme::current();
                for (spans, diff) in highlighted {
                    let base = theme::colored(match diff {
                        DiffKind::Add => current.added,
                        DiffKind::Remove => current.removed,
                        DiffKind::Hunk => current.hunk,
                        DiffKind::None => current.code,
                    });
                    let mut line = spans_to_line(std::slice::from_ref(&gutter), &spans, &base);
                    clip_line(&mut line, width);
                    out.push(line);
                }
                continue;
            }
            MdBlock::Hr => out.push(vec![("─".repeat(width.min(80)), dim())]),
        }
        i += 1;
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// Cuts a display line to `width` cells.
fn clip_line(line: &mut DisplayLine, width: usize) {
    let mut used = 0;
    let mut keep = 0;
    for (text, _) in line.iter_mut() {
        let w = unicode_column_width(text, None);
        if used + w <= width {
            used += w;
            keep += 1;
            continue;
        }
        let mut cut = String::new();
        for ch in text.chars() {
            let cw = unicode_column_width(ch.encode_utf8(&mut [0; 4]), None);
            if used + cw > width {
                break;
            }
            used += cw;
            cut.push(ch);
        }
        *text = cut;
        keep += 1;
        break;
    }
    line.truncate(keep);
}

fn line_text(line: &DisplayLine) -> String {
    line.iter().map(|(t, _)| t.as_str()).collect()
}

// ---------- App ----------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Preview,
    Edit,
}

struct Find {
    query: String,
}

struct App {
    path: PathBuf,
    buffer: Buffer,
    highlighter: LineHighlighter,
    mode: Mode,
    markdown: bool,
    /// Edit view: first row and first display column shown.
    top: usize,
    left: usize,
    /// Preview: rendered lines for `preview_width`, and the first shown.
    preview: Vec<DisplayLine>,
    preview_width: usize,
    preview_version: u64,
    preview_top: usize,
    cols: usize,
    rows: usize,
    status: Option<String>,
    find: Option<Find>,
    /// The last match, highlighted in the edit view.
    found: Option<(Pos, Pos)>,
    quit_armed: bool,
    pending_open: Option<(PathBuf, Option<usize>)>,
    mtime: Option<SystemTime>,
    last_watch: Instant,
    mouse_down: bool,
    /// Set when the file could not be shown (binary, too large).
    blocked: Option<String>,
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn read_file(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if len > MAX_FILE_BYTES {
        return Err(
            rust_i18n::t!("file_view.too_large", mb = MAX_FILE_BYTES / 1024 / 1024).into_owned(),
        );
    }
    if looks_binary(path) {
        return Err(t("file_view.binary"));
    }
    std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|err| rust_i18n::t!("file_view.read_failed", error = err.to_string()).into_owned())
}

impl App {
    fn open(path: PathBuf, line: Option<usize>, cols: usize, rows: usize) -> Self {
        let (text, blocked) = match read_file(&path) {
            Ok(text) => (text, None),
            Err(err) => (String::new(), Some(err)),
        };
        let markdown = is_markdown(&path);
        let buffer = Buffer::from_text(&text);
        let highlighter = LineHighlighter::new(
            &path,
            buffer.lines.first().map(String::as_str).unwrap_or(""),
        );
        let mut app = Self {
            mtime: file_mtime(&path),
            path,
            buffer,
            highlighter,
            mode: if markdown && line.is_none() {
                Mode::Preview
            } else {
                Mode::Edit
            },
            markdown,
            top: 0,
            left: 0,
            preview: vec![],
            preview_width: 0,
            preview_version: u64::MAX,
            preview_top: 0,
            cols,
            rows,
            status: None,
            find: None,
            found: None,
            quit_armed: false,
            pending_open: None,
            last_watch: Instant::now(),
            mouse_down: false,
            blocked,
        };
        if let Some(line) = line {
            let row = line.saturating_sub(1).min(app.buffer.lines.len() - 1);
            app.buffer.move_to(Pos { row, col: 0 }, false);
            app.top = row.saturating_sub(app.view_rows() / 3);
        }
        app
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.to_string_lossy().into_owned())
    }

    fn view_rows(&self) -> usize {
        self.rows.saturating_sub(HEADER_ROWS + 1).max(1)
    }

    fn gutter(&self) -> usize {
        self.buffer.lines.len().to_string().len().max(3) + 2
    }

    fn text_cols(&self) -> usize {
        self.cols.saturating_sub(self.gutter()).max(1)
    }

    fn ensure_preview(&mut self) {
        let width = self.cols.saturating_sub(4).min(110);
        if self.preview_width != width || self.preview_version != self.buffer.version {
            self.preview = render_markdown(&self.buffer.text(), width);
            self.preview_width = width;
            self.preview_version = self.buffer.version;
            self.preview_top = self.preview_top.min(self.preview.len().saturating_sub(1));
        }
    }

    fn scroll_cursor_into_view(&mut self) {
        let rows = self.view_rows();
        let row = self.buffer.cursor.row;
        if row < self.top {
            self.top = row;
        } else if row >= self.top + rows {
            self.top = row + 1 - rows;
        }
        let x = display_x(&self.buffer.lines[row], self.buffer.cursor.col);
        let cols = self.text_cols();
        if x < self.left {
            self.left = x.saturating_sub(cols / 4);
        } else if x >= self.left + cols {
            self.left = x + 1 - cols + cols / 4;
        }
    }

    fn save(&mut self) {
        let text = self.buffer.text();
        let result = (|| -> std::io::Result<()> {
            let tmp = self.path.with_extension(format!(
                "{}.sumi-save-{}",
                self.path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("tmp"),
                std::process::id()
            ));
            std::fs::write(&tmp, &text)?;
            if let Ok(meta) = std::fs::metadata(&self.path) {
                let _ = std::fs::set_permissions(&tmp, meta.permissions());
            }
            // Write through a symlink to its target.
            let target = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
            std::fs::rename(&tmp, &target)
        })();
        match result {
            Ok(()) => {
                self.buffer.mark_saved();
                self.mtime = file_mtime(&self.path);
                self.status = Some(t("file_view.saved"));
                self.quit_armed = false;
                if let Some((path, line)) = self.pending_open.take() {
                    self.switch_to(path, line);
                }
            }
            Err(err) => {
                self.status = Some(
                    rust_i18n::t!("file_view.save_failed", error = err.to_string()).into_owned(),
                );
            }
        }
    }

    fn switch_to(&mut self, path: PathBuf, line: Option<usize>) {
        if self.buffer.is_dirty() {
            self.status = Some(
                rust_i18n::t!(
                    "file_view.unsaved_open",
                    name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                )
                .into_owned(),
            );
            self.pending_open = Some((path, line));
            return;
        }
        let same = path == self.path;
        let keep_mode = self.mode;
        *self = Self::open(path, line, self.cols, self.rows);
        if same && line.is_none() {
            self.mode = keep_mode;
        }
    }

    /// Reloads after an outside change, unless edits would be lost.
    fn watch(&mut self) -> bool {
        if self.last_watch.elapsed() < WATCH_INTERVAL {
            return false;
        }
        self.last_watch = Instant::now();
        let mtime = file_mtime(&self.path);
        if mtime == self.mtime {
            return false;
        }
        self.mtime = mtime;
        if self.buffer.is_dirty() {
            self.status = Some(t("file_view.changed_on_disk"));
            return true;
        }
        match read_file(&self.path) {
            Ok(text) => {
                if text != self.buffer.text() {
                    self.buffer.reload(&text);
                    self.highlighter.invalidate_from(0);
                    self.status = Some(t("file_view.reloaded"));
                }
                self.blocked = None;
            }
            Err(err) => self.blocked = Some(err),
        }
        true
    }

    fn copy(&mut self, text: String) -> Change {
        self.status = Some(t("file_view.copied"));
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        Change::Text(format!("\x1b]52;c;{encoded}\x07"))
    }

    fn indent_unit(&self) -> &'static str {
        if self.buffer.lines.iter().any(|l| l.starts_with('\t')) {
            "\t"
        } else {
            "    "
        }
    }

    fn find_next(&mut self) {
        let Some(find) = &self.find else {
            return;
        };
        let query = find.query.clone();
        if query.is_empty() {
            return;
        }
        if self.mode == Mode::Preview {
            let start = self.preview_top + 1;
            let n = self.preview.len();
            let lower = query.to_lowercase();
            for step in 0..n {
                let idx = (start + step) % n.max(1);
                if line_text(&self.preview[idx])
                    .to_lowercase()
                    .contains(&lower)
                {
                    self.preview_top = idx.saturating_sub(2);
                    self.status = None;
                    return;
                }
            }
        } else {
            let from = self.found.map(|(_, end)| end).unwrap_or(self.buffer.cursor);
            if let Some((start, end)) = self.buffer.find_next(&query, from) {
                self.found = Some((start, end));
                self.buffer.move_to(start, false);
                self.buffer.move_to(end, true);
                self.scroll_cursor_into_view();
                self.status = None;
                return;
            }
        }
        self.status = Some(rust_i18n::t!("file_view.not_found", query = query).into_owned());
    }

    /// Handles a key; returns false to quit.
    fn key(&mut self, key: KeyCode, mods: Modifiers, out: &mut Vec<Change>) -> bool {
        let ctrl = mods.contains(Modifiers::CTRL);
        let shift = mods.contains(Modifiers::SHIFT);
        if !matches!(key, KeyCode::Char('q') | KeyCode::Char('w')) || !ctrl {
            self.quit_armed = false;
        }
        // The find prompt takes plain typing.
        if let Some(find) = &mut self.find {
            match key {
                KeyCode::Escape => {
                    self.find = None;
                    self.status = None;
                }
                KeyCode::Enter => self.find_next(),
                KeyCode::Char('g') if ctrl => self.find_next(),
                KeyCode::Char('f') if ctrl => self.find_next(),
                KeyCode::Backspace => {
                    find.query.pop();
                }
                KeyCode::Char(c) if !ctrl => find.query.push(c),
                _ => {}
            }
            return true;
        }
        match key {
            KeyCode::Char('q') | KeyCode::Char('w') if ctrl => {
                if self.buffer.is_dirty() && !self.quit_armed {
                    self.quit_armed = true;
                    self.status = Some(t("file_view.unsaved_quit"));
                    return true;
                }
                return false;
            }
            KeyCode::Char('s') if ctrl => {
                if self.blocked.is_none() {
                    self.save();
                }
                return true;
            }
            KeyCode::Char('t') if ctrl => {
                let next = theme::cycle();
                self.highlighter.invalidate_from(0);
                self.preview_version = u64::MAX;
                self.status = Some(rust_i18n::t!("file_view.theme", name = next.name).into_owned());
                return true;
            }
            KeyCode::Char('p') if ctrl && self.markdown => {
                self.mode = if self.mode == Mode::Preview {
                    Mode::Edit
                } else {
                    Mode::Preview
                };
                self.found = None;
                return true;
            }
            KeyCode::Char('f') if ctrl => {
                self.find = Some(Find {
                    query: self
                        .buffer
                        .selected_text()
                        .filter(|t| !t.contains('\n'))
                        .unwrap_or_default(),
                });
                self.found = None;
                return true;
            }
            KeyCode::Char('g') if ctrl => {
                self.find_next();
                return true;
            }
            _ => {}
        }
        if self.blocked.is_some() {
            return true;
        }
        if self.mode == Mode::Preview {
            let page = self.view_rows().saturating_sub(2).max(1);
            let max = self.preview.len().saturating_sub(1);
            let top = &mut self.preview_top;
            match key {
                KeyCode::UpArrow | KeyCode::Char('k') => *top = top.saturating_sub(1),
                KeyCode::DownArrow | KeyCode::Char('j') | KeyCode::Enter => {
                    *top = (*top + 1).min(max)
                }
                KeyCode::PageUp | KeyCode::Char('b') => *top = top.saturating_sub(page),
                KeyCode::PageDown | KeyCode::Char(' ') => *top = (*top + page).min(max),
                KeyCode::Home | KeyCode::Char('g') => *top = 0,
                KeyCode::End | KeyCode::Char('G') => *top = max,
                KeyCode::Char('e') | KeyCode::Char('i') => self.mode = Mode::Edit,
                KeyCode::Char('q') if !self.buffer.is_dirty() => return false,
                _ => {}
            }
            return true;
        }
        let page = self.view_rows().saturating_sub(1).max(1) as isize;
        let before = self.buffer.version;
        match key {
            KeyCode::Char('z') if ctrl && shift => {
                self.buffer.redo();
            }
            KeyCode::Char('z') if ctrl => {
                self.buffer.undo();
            }
            KeyCode::Char('y') if ctrl => {
                self.buffer.redo();
            }
            KeyCode::Char('a') if ctrl => self.buffer.select_all(),
            KeyCode::Char('c') if ctrl => {
                if let Some(text) = self.buffer.selected_text() {
                    let change = self.copy(text);
                    out.push(change);
                }
            }
            KeyCode::Char('x') if ctrl => {
                if let Some(text) = self.buffer.cut() {
                    let change = self.copy(text);
                    out.push(change);
                }
            }
            KeyCode::Escape => {
                self.buffer.anchor = None;
                self.found = None;
            }
            KeyCode::Char(c) if !ctrl && !mods.contains(Modifiers::ALT) => {
                self.buffer.insert(c.encode_utf8(&mut [0; 4]))
            }
            KeyCode::Char(c) if mods == Modifiers::ALT => {
                self.buffer.insert(c.encode_utf8(&mut [0; 4]))
            }
            KeyCode::Enter => self.buffer.newline(),
            KeyCode::Tab => {
                let unit = self.indent_unit();
                self.buffer.insert(unit);
            }
            KeyCode::Backspace => self.buffer.backspace(),
            KeyCode::Delete => self.buffer.delete(),
            KeyCode::LeftArrow | KeyCode::ApplicationLeftArrow => self.buffer.left(shift),
            KeyCode::RightArrow | KeyCode::ApplicationRightArrow => self.buffer.right(shift),
            KeyCode::UpArrow | KeyCode::ApplicationUpArrow => self.buffer.vertical(-1, shift),
            KeyCode::DownArrow | KeyCode::ApplicationDownArrow => self.buffer.vertical(1, shift),
            KeyCode::PageUp => self.buffer.vertical(-page, shift),
            KeyCode::PageDown => self.buffer.vertical(page, shift),
            KeyCode::Home if ctrl => self.buffer.move_to(Pos::default(), shift),
            KeyCode::End if ctrl => {
                let row = self.buffer.lines.len() - 1;
                let col = self.buffer.line_len(row);
                self.buffer.move_to(Pos { row, col }, shift)
            }
            KeyCode::Home => self.buffer.home(shift),
            KeyCode::End => self.buffer.end(shift),
            _ => return true,
        }
        if self.buffer.version != before {
            self.found = None;
            self.status = None;
        }
        self.scroll_cursor_into_view();
        true
    }

    fn paste(&mut self, text: &str) {
        if let Some((path, line)) = parse_open_request(text) {
            self.switch_to(path, line);
            return;
        }
        if self.find.is_some() {
            if let Some(find) = &mut self.find {
                find.query.push_str(text.lines().next().unwrap_or_default());
            }
            return;
        }
        if self.mode == Mode::Edit && self.blocked.is_none() {
            self.buffer.insert(text);
            self.scroll_cursor_into_view();
        }
    }

    /// Position in the buffer under a 0-based cell.
    fn pos_at(&self, x: usize, y: usize) -> Option<Pos> {
        let view_y = y.checked_sub(HEADER_ROWS)?;
        if view_y >= self.view_rows() {
            return None;
        }
        let row = (self.top + view_y).min(self.buffer.lines.len() - 1);
        let target = (x.saturating_sub(self.gutter())) + self.left;
        Some(Pos {
            row,
            col: col_for_x(&self.buffer.lines[row], target),
        })
    }

    fn mouse(&mut self, event: &MouseEvent) {
        let x = (event.x as usize).saturating_sub(1);
        let y = (event.y as usize).saturating_sub(1);
        if event.mouse_buttons.contains(MouseButtons::VERT_WHEEL) {
            let up = event.mouse_buttons.contains(MouseButtons::WHEEL_POSITIVE);
            if self.mode == Mode::Preview {
                let max = self.preview.len().saturating_sub(1);
                self.preview_top = if up {
                    self.preview_top.saturating_sub(3)
                } else {
                    (self.preview_top + 3).min(max)
                };
            } else {
                let max = self.buffer.lines.len().saturating_sub(1);
                self.top = if up {
                    self.top.saturating_sub(3)
                } else {
                    (self.top + 3).min(max)
                };
            }
            return;
        }
        let pressed = event.mouse_buttons.contains(MouseButtons::LEFT);
        if pressed && !self.mouse_down && y == 0 && self.markdown {
            // The mode switch in the header.
            if let Some(mode) = self.header_mode_at(x) {
                self.mode = mode;
            }
            self.mouse_down = true;
            return;
        }
        if self.mode == Mode::Edit && self.blocked.is_none() {
            if pressed {
                if let Some(pos) = self.pos_at(x, y) {
                    let select = self.mouse_down || event.modifiers.contains(Modifiers::SHIFT);
                    self.buffer.move_to(pos, select);
                }
            }
        }
        self.mouse_down = pressed;
    }

    fn header_tabs(&self) -> Vec<(Mode, String)> {
        if !self.markdown {
            return vec![];
        }
        vec![
            (Mode::Preview, format!(" {} ", t("file_view.preview"))),
            (Mode::Edit, format!(" {} ", t("file_view.edit"))),
        ]
    }

    fn header_tabs_x(&self) -> usize {
        let width: usize = self
            .header_tabs()
            .iter()
            .map(|(_, l)| unicode_column_width(l, None) + 1)
            .sum();
        self.cols.saturating_sub(width + 1)
    }

    fn header_mode_at(&self, x: usize) -> Option<Mode> {
        let mut at = self.header_tabs_x();
        for (mode, label) in self.header_tabs() {
            let w = unicode_column_width(&label, None);
            if x >= at && x < at + w {
                return Some(mode);
            }
            at += w + 1;
        }
        None
    }

    fn render(&mut self) -> Vec<Change> {
        let mut out = vec![
            Change::Text("\x1b[?2026h".into()),
            Change::CursorVisibility(CursorVisibility::Hidden),
            Change::AllAttributes(attrs()),
            Change::ClearScreen(theme::background()),
        ];
        let put = |out: &mut Vec<Change>, x: usize, y: usize, cells: &[Cell]| {
            out.push(Change::CursorPosition {
                x: Position::Absolute(x),
                y: Position::Absolute(y),
            });
            for (text, a) in cells {
                out.push(Change::AllAttributes(a.clone()));
                out.push(Change::Text(text.clone()));
            }
            out.push(Change::AllAttributes(attrs()));
        };

        // Header: name, dirty mark, folder; mode switch on the right.
        let mut bold = attrs();
        bold.set_intensity(Intensity::Bold);
        let dir = self
            .path
            .parent()
            .map(|p| {
                let s = p.to_string_lossy().into_owned();
                let home = config::HOME_DIR.to_string_lossy().into_owned();
                match s.strip_prefix(&home) {
                    Some(rest) => format!("~{rest}"),
                    None => s,
                }
            })
            .unwrap_or_default();
        let mut header: DisplayLine = vec![(format!(" {}", self.name()), bold)];
        if self.buffer.is_dirty() {
            header.push((" ●".into(), warn()));
        }
        header.push((format!("  {dir}"), dim()));
        let tabs_x = self.header_tabs_x();
        clip_line(&mut header, tabs_x.saturating_sub(1));
        put(&mut out, 0, 0, &header);
        let mut x = tabs_x;
        for (mode, label) in self.header_tabs() {
            let mut a = if mode == self.mode { attrs() } else { dim() };
            if mode == self.mode {
                a.set_reverse(true);
            }
            let w = unicode_column_width(&label, None);
            put(&mut out, x, 0, &[(label, a)]);
            x += w + 1;
        }
        put(&mut out, 0, 1, &[("─".repeat(self.cols), dim())]);

        let rows = self.view_rows();
        let mut cursor_at = None;
        if let Some(blocked) = &self.blocked {
            put(&mut out, 2, HEADER_ROWS + 1, &[(blocked.clone(), dim())]);
        } else if self.mode == Mode::Preview {
            self.ensure_preview();
            for (i, line) in self
                .preview
                .iter()
                .skip(self.preview_top)
                .take(rows)
                .enumerate()
            {
                put(&mut out, 2, HEADER_ROWS + i, line);
            }
        } else {
            if self.buffer.dirty_from != usize::MAX {
                self.highlighter.invalidate_from(self.buffer.dirty_from);
                self.buffer.dirty_from = usize::MAX;
            }
            self.highlighter
                .ensure(&self.buffer, (self.top + rows).min(self.buffer.lines.len()));
            let gutter = self.gutter();
            let cols = self.text_cols();
            let selection = self.buffer.selection();
            for i in 0..rows {
                let row = self.top + i;
                let y = HEADER_ROWS + i;
                if row >= self.buffer.lines.len() {
                    put(
                        &mut out,
                        0,
                        y,
                        &[(format!("{:>w$} ", "~", w = gutter - 2), dim())],
                    );
                    continue;
                }
                let number_attrs = if row == self.buffer.cursor.row {
                    attrs()
                } else {
                    dim()
                };
                put(
                    &mut out,
                    0,
                    y,
                    &[(format!("{:>w$}  ", row + 1, w = gutter - 2), number_attrs)],
                );
                let line = &self.buffer.lines[row];
                let styled: Styled = self
                    .highlighter
                    .line(row)
                    .cloned()
                    .unwrap_or_else(|| vec![(line.clone(), None)]);
                // Walk characters, expanding tabs and clipping to the view.
                let mut cells: DisplayLine = vec![];
                let mut x = 0;
                let mut col = 0;
                for (text, color) in &styled {
                    let base = match color {
                        Some((r, g, b)) => rgb(*r, *g, *b),
                        None => attrs(),
                    };
                    for ch in text.chars() {
                        let w = char_width(ch, x);
                        let pos = Pos { row, col };
                        let selected = selection.is_some_and(|(s, e)| pos >= s && pos < e);
                        if x >= self.left && x + w <= self.left + cols {
                            let mut a = base.clone();
                            if selected {
                                a.set_reverse(true);
                            }
                            let shown = if ch == '\t' {
                                " ".repeat(w)
                            } else {
                                ch.to_string()
                            };
                            match cells.last_mut() {
                                Some((t, last)) if *last == a => t.push_str(&shown),
                                _ => cells.push((shown, a)),
                            }
                        }
                        x += w;
                        col += 1;
                    }
                }
                put(&mut out, gutter, y, &cells);
            }
            let cursor = self.buffer.cursor;
            if cursor.row >= self.top && cursor.row < self.top + rows {
                let x = display_x(&self.buffer.lines[cursor.row], cursor.col);
                if x >= self.left && x < self.left + cols {
                    cursor_at = Some((gutter + x - self.left, HEADER_ROWS + cursor.row - self.top));
                }
            }
        }

        // Status line: message or find prompt, then position and hints.
        let status_y = self.rows.saturating_sub(1);
        let left: DisplayLine = if let Some(find) = &self.find {
            let prompt = t("file_view.find");
            let x =
                1 + unicode_column_width(&prompt, None) + unicode_column_width(&find.query, None);
            cursor_at = Some((x, status_y));
            vec![
                (format!(" {prompt}"), accent()),
                (find.query.clone(), attrs()),
            ]
        } else if let Some(status) = &self.status {
            vec![(format!(" {status}"), warn())]
        } else if self.mode == Mode::Edit {
            let c = self.buffer.cursor;
            vec![(
                format!(
                    " {}",
                    rust_i18n::t!("file_view.position", line = c.row + 1, col = c.col + 1)
                ),
                dim(),
            )]
        } else {
            vec![]
        };
        let hints = t(match (self.mode, self.markdown) {
            (Mode::Preview, _) => "file_view.hints_preview",
            (Mode::Edit, true) => "file_view.hints_markdown_edit",
            (Mode::Edit, false) => "file_view.hints_edit",
        });
        let left_w: usize = left
            .iter()
            .map(|(t, _)| unicode_column_width(t, None))
            .sum();
        let hints_w = unicode_column_width(&hints, None);
        put(&mut out, 0, status_y, &left);
        if left_w + hints_w + 3 < self.cols {
            put(
                &mut out,
                self.cols - hints_w - 1,
                status_y,
                &[(hints, dim())],
            );
        }

        if let Some((x, y)) = cursor_at {
            out.push(Change::CursorPosition {
                x: Position::Absolute(x),
                y: Position::Absolute(y),
            });
            out.push(Change::CursorVisibility(CursorVisibility::Visible));
        }
        out.push(Change::Text("\x1b[?2026l".into()));
        out
    }
}

fn set_user_var(name: &str, value: &str) -> Change {
    let encoded = base64::engine::general_purpose::STANDARD.encode(value);
    Change::Text(format!("\x1b]1337;SetUserVar={name}={encoded}\x07"))
}

/// Runs the viewer and exits non-zero with the error on stderr.
pub fn run(args: &[String]) {
    if let Err(err) = main(args) {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

/// Entry point for `kaku-gui --sumi-view <path> [--line=N] [--light]`.
pub fn main(args: &[String]) -> anyhow::Result<()> {
    rust_i18n::set_locale(&config::i18n::resolve_locale(config::i18n::LANGUAGE_AUTO));
    let mut path = None;
    let mut line = None;
    let mut light = false;
    let mut theme_name = String::new();
    for arg in args {
        if let Some(n) = arg.strip_prefix("--line=") {
            line = n.parse().ok();
        } else if let Some(name) = arg.strip_prefix("--theme=") {
            theme_name = name.to_string();
        } else if arg == "--light" {
            light = true;
        } else if !arg.starts_with("--") && path.is_none() {
            path = Some(PathBuf::from(arg));
        }
    }
    let path = path.ok_or_else(|| anyhow::anyhow!("usage: sumi view <file> [--line=N]"))?;
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };

    let caps = Capabilities::new_from_env()?;
    let mut term = UnixTerminal::new(caps)?;
    term.enter_alternate_screen()?;
    term.set_raw_mode()?;
    let size = term.get_screen_size()?;
    theme::set(theme::index_of(&theme_name), light);
    let mut app = App::open(path, line, size.cols, size.rows);
    term.render(&[
        set_user_var(crate::termwindow::left_panel::VIEWER_USER_VAR, "1"),
        Change::Title(app.name()),
    ])?;
    let mut title = app.name();

    let result = (|| -> anyhow::Result<()> {
        let mut redraw = true;
        loop {
            if redraw {
                let changes = app.render();
                term.render(&changes)?;
                if title != app.name() {
                    title = app.name();
                    term.render(&[Change::Title(title.clone())])?;
                }
                redraw = false;
            }
            let mut extra = vec![];
            match term.poll_input(Some(POLL))? {
                Some(InputEvent::Key(KeyEvent { key, modifiers })) => {
                    if !app.key(key, modifiers, &mut extra) {
                        return Ok(());
                    }
                    redraw = true;
                }
                Some(InputEvent::Paste(text)) => {
                    app.paste(&text);
                    redraw = true;
                }
                Some(InputEvent::Mouse(event)) => {
                    app.mouse(&event);
                    redraw = true;
                }
                Some(InputEvent::Resized { cols, rows }) => {
                    app.cols = cols;
                    app.rows = rows;
                    app.scroll_cursor_into_view();
                    redraw = true;
                }
                Some(_) => {}
                None => {}
            }
            if !extra.is_empty() {
                term.render(&extra)?;
            }
            redraw |= app.watch();
        }
    })();

    let _ = term.render(&[set_user_var(
        crate::termwindow::left_panel::VIEWER_USER_VAR,
        "",
    )]);
    let _ = term.set_cooked_mode();
    let _ = term.exit_alternate_screen();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[DisplayLine]) -> Vec<String> {
        lines.iter().map(line_text).collect()
    }

    #[test]
    fn markdown_renders_headings_lists_quotes_and_code() {
        let md = "# Title\n\nSome **bold** text.\n\n- one\n- two\n\n> quoted\n\n```rust\nfn main() {}\n```\n";
        let text = plain(&render_markdown(md, 40));
        assert_eq!(text[0], "Title");
        assert!(text[1].starts_with('─'));
        assert!(text.contains(&"Some bold text.".to_string()));
        assert!(text
            .iter()
            .any(|l| l.ends_with("one") && l.contains('•') || l == "- one"));
        assert!(text.contains(&"▎ quoted".to_string()));
        assert!(text.contains(&"│ fn main() {}".to_string()));
    }

    #[test]
    fn long_paragraphs_wrap_to_the_width() {
        let md = "word ".repeat(30);
        for line in render_markdown(&md, 20) {
            assert!(unicode_column_width(&line_text(&line), None) <= 20);
        }
    }

    #[test]
    fn clipping_respects_wide_characters() {
        let mut line: DisplayLine = vec![("中文字符".into(), attrs()), ("tail".into(), attrs())];
        clip_line(&mut line, 5);
        assert_eq!(line_text(&line), "中文");
    }

    #[test]
    fn open_requests_round_trip_paths_with_colons() {
        let prefix = crate::termwindow::left_panel::VIEWER_OPEN_PREFIX;
        assert_eq!(
            parse_open_request(&format!("{prefix}/a/b.md:7")),
            Some((PathBuf::from("/a/b.md"), Some(7)))
        );
        assert_eq!(
            parse_open_request(&format!("{prefix}/a/x:y")),
            Some((PathBuf::from("/a/x:y"), None))
        );
        assert_eq!(parse_open_request("/a/b.md"), None);
    }

    #[test]
    fn highlighter_reuses_state_and_recomputes_after_an_edit() {
        let mut buffer = Buffer::from_text("fn main() {\n    let s = \"x\";\n}\n");
        let mut h = LineHighlighter::new(Path::new("main.rs"), "fn main() {");
        assert!(h.syntax.is_some());
        h.ensure(&buffer, 3);
        assert_eq!(h.lines.len(), 3);
        let joined: String = h.line(1).unwrap().iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "    let s = \"x\";");
        buffer.move_to(Pos { row: 1, col: 4 }, false);
        buffer.insert("// ");
        h.invalidate_from(buffer.dirty_from.min(1));
        h.ensure(&buffer, 3);
        let joined: String = h.line(1).unwrap().iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(joined, "    // let s = \"x\";");
        assert!(LineHighlighter::new(Path::new("notes.unknownext"), "")
            .syntax
            .is_none());
    }
}
