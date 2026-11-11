//! The left panel, after Warp's tool panel: a strip of tools beside the
//! terminal panes. Tabs stay in the horizontal tab bar above it.
//!
//! - Files: the project tree of the active pane (`file_tree`).
//! - Search: text search across the project (`search`).
//! - Snippets: saved commands and prompts (`snippets`).
//! - AI Chats: Sumi Assistant conversations, reopened in the chat overlay.
//!
//! Layout: the panel's width is added to the left OS border in
//! `get_os_border`, so terminal sizing, pane placement, mouse coordinates
//! and overlays start right of it without knowing it exists. The tab bar
//! keeps the full window width and the panel starts below it.
//!
//! Files open in Sumi's viewer (`crate::file_view`) in a pane split to the
//! right: Markdown renders, and every text file can be edited there.

mod file_tree;
mod search;
mod snippets;

use crate::termwindow::box_model::*;
use crate::termwindow::render::corners::*;
use crate::termwindow::{TermWindowNotif, UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::keyassignment::{
    ClipboardCopyDestination, KeyAssignment, LeftPanelCommand, LeftPanelTool, SpawnCommand,
};
use config::{Dimension, DimensionContext, LEFT_PANEL_MAX_WIDTH, LEFT_PANEL_MIN_WIDTH};
use mux::pane::{CachePolicy, Pane};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use termwiz::cell::unicode_column_width;
use wezterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};
use window::color::LinearRgba;
use window::{MouseCursor, MouseEvent, MouseEventKind as WMEK, MousePress, WindowOps};

pub(crate) use file_tree::project_root;

/// Space kept above the tools for the macOS traffic lights when no tab bar
/// sits over the panel.
const TRAFFIC_LIGHT_CLEARANCE_PT: f32 = 38.0;
const TOP_GAP_PT: f32 = 8.0;
const MARGIN_X_PT: f32 = 8.0;
const TOOL_BUTTON_PT: f32 = 28.0;
const TOOL_STRIP_PT: f32 = 30.0;
const ROW_PAD_X_PT: f32 = 6.0;
const ROW_PAD_Y_PT: f32 = 3.0;
const FIELD_PAD_Y_PT: f32 = 5.0;
const INDENT_PT: f32 = 12.0;
const SECTION_GAP_PT: f32 = 6.0;
/// Hit width of the drag handle on the panel's right edge.
const RESIZE_HANDLE_PT: f32 = 6.0;
const STATE_FILE_NAME: &str = ".sumi_left_panel.json";
/// How often the visible tool re-reads its source (directory listings,
/// snippets file, conversation index).
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Typed pauses this long start a search.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Marks a paste that asks a running viewer to open another file instead
/// of inserting text. U+2063 never appears in pasted prose or code.
pub(crate) const VIEWER_OPEN_PREFIX: &str = "\u{2063}sumi-open\u{2063}";
/// User var the viewer sets so the GUI can find it among a tab's panes.
pub(crate) const VIEWER_USER_VAR: &str = "sumi_viewer";

pub(crate) const TOOLS: [LeftPanelTool; 4] = [
    LeftPanelTool::Files,
    LeftPanelTool::Search,
    LeftPanelTool::Snippets,
    LeftPanelTool::Conversations,
];

fn tool_index(tool: LeftPanelTool) -> usize {
    TOOLS.iter().position(|t| *t == tool).unwrap_or(0)
}

fn tool_icon(tool: LeftPanelTool) -> &'static str {
    match tool {
        LeftPanelTool::Files => "\u{eaf0}",
        LeftPanelTool::Search => "\u{ea6d}",
        LeftPanelTool::Snippets => "\u{eb66}",
        LeftPanelTool::Conversations => "\u{eac7}",
    }
}

fn tool_title(tool: LeftPanelTool) -> String {
    let key = match tool {
        LeftPanelTool::Files => "left_panel.files",
        LeftPanelTool::Search => "left_panel.search",
        LeftPanelTool::Snippets => "left_panel.snippets",
        LeftPanelTool::Conversations => "left_panel.conversations",
    };
    rust_i18n::t!(key).into_owned()
}

/// What a panel region does under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelItem {
    Background,
    /// The strip under the traffic lights; drags the window like a titlebar.
    Header,
    Tool(LeftPanelTool),
    /// A list row; indexes `LeftPanelState::targets`.
    Row(usize),
    Field(FieldKind),
    Toggle(SearchToggle),
    Button(PanelButton),
    /// The right edge, dragged to change the width.
    Resize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    Search,
    ConversationFilter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchToggle {
    MatchCase,
    Regex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelButton {
    NewSnippet,
    EditSnippets,
    NewChat,
}

/// The value a panel text prompt edits; see `tab_rename::EditTarget`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelPrompt {
    Search,
    ConversationFilter,
    NewFile { dir: PathBuf },
    NewFolder { dir: PathBuf },
    Rename { path: PathBuf },
    NewSnippet,
}

/// What a list row stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RowTarget {
    Dir(PathBuf),
    File(PathBuf),
    SearchFile(PathBuf),
    SearchMatch { path: PathBuf, line: usize },
    Snippet(usize),
    Conversation(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct SavedPanel {
    visible: bool,
    width: f32,
    #[serde(default)]
    tool: usize,
    /// The config values when this was saved; a different value in the
    /// config now means the user changed it there, which wins.
    config_visible: bool,
    config_width: f32,
}

/// Startup visibility and width: the last toggle and drag, unless the config
/// changed since they were saved.
fn resolve_saved(
    saved: Option<SavedPanel>,
    config_visible: bool,
    config_width: f32,
) -> (bool, f32, LeftPanelTool) {
    let config_width = clamp_width(config_width);
    match saved {
        None => (config_visible, config_width, LeftPanelTool::Files),
        Some(saved) => (
            if saved.config_visible != config_visible {
                config_visible
            } else {
                saved.visible
            },
            if (saved.config_width - config_width).abs() > 0.01 {
                config_width
            } else {
                clamp_width(saved.width)
            },
            TOOLS
                .get(saved.tool)
                .copied()
                .unwrap_or(LeftPanelTool::Files),
        ),
    }
}

pub(crate) fn clamp_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(LEFT_PANEL_MIN_WIDTH, LEFT_PANEL_MAX_WIDTH)
    } else {
        LEFT_PANEL_MIN_WIDTH
    }
}

fn state_file() -> PathBuf {
    config::CONFIG_DIRS
        .first()
        .cloned()
        .unwrap_or_else(|| config::HOME_DIR.join(".config").join("sumi"))
        .join(STATE_FILE_NAME)
}

fn load_saved() -> Option<SavedPanel> {
    let text = std::fs::read_to_string(state_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save(saved: &SavedPanel) {
    let path = state_file();
    let result = (|| -> anyhow::Result<()> {
        let text = serde_json::to_string(saved)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if let Err(err) = result {
        log::warn!("saving left panel state to {}: {err:#}", path.display());
    }
}

/// Per-window panel state.
/// Set just before Settings spawns its window; the next window to open
/// takes it and keeps the panel hidden.
pub(crate) static NEXT_WINDOW_IS_UTILITY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) struct LeftPanelState {
    pub visible: bool,
    /// Set for utility windows such as Settings, which never show the panel.
    pub utility: bool,
    pub width_pt: f32,
    pub tool: LeftPanelTool,
    files: file_tree::FileTree,
    search: search::SearchState,
    snippets: snippets::SnippetStore,
    conversations: Vec<crate::ai_conversations::ConversationMeta>,
    active_conversation: Option<String>,
    conversation_filter: String,
    /// First row drawn per tool, moved by the wheel.
    scroll: [usize; 4],
    /// What each `PanelItem::Row` index stands for, from the last build.
    targets: Vec<RowTarget>,
    /// Rows the last build could fit; the wheel stops when the end shows.
    fits_rows: usize,
    last_poll: Option<Instant>,
    search_due: Option<Instant>,
    element: Option<Vec<ComputedElement>>,
}

impl LeftPanelState {
    pub fn new(config: &config::ConfigHandle) -> Self {
        let (visible, width_pt, tool) =
            resolve_saved(load_saved(), config.left_panel, config.left_panel_width);
        Self {
            visible,
            utility: false,
            width_pt,
            tool,
            files: Default::default(),
            search: Default::default(),
            snippets: Default::default(),
            conversations: vec![],
            active_conversation: None,
            conversation_filter: String::new(),
            scroll: [0; 4],
            targets: vec![],
            fits_rows: 0,
            last_poll: None,
            search_due: None,
            element: None,
        }
    }

    pub fn invalidate(&mut self) {
        self.element.take();
    }

    fn persist(&self, config: &config::ConfigHandle) {
        save(&SavedPanel {
            visible: self.visible,
            width: self.width_pt,
            tool: tool_index(self.tool),
            config_visible: config.left_panel,
            config_width: clamp_width(config.left_panel_width),
        });
    }

    /// Applies a reloaded config. Returns true when the layout changes.
    /// `hotkey_window` keeps the global-hotkey drawer without a panel.
    pub fn config_reloaded(
        &mut self,
        old: &config::ConfigHandle,
        new: &config::ConfigHandle,
        hotkey_window: bool,
    ) -> bool {
        if hotkey_window || self.utility {
            return false;
        }
        let mut changed = false;
        if old.left_panel != new.left_panel {
            self.visible = new.left_panel;
            changed = true;
        }
        if (old.left_panel_width - new.left_panel_width).abs() > 0.01 {
            self.width_pt = clamp_width(new.left_panel_width);
            changed = true;
        }
        self.invalidate();
        if changed {
            self.persist(new);
        }
        changed
    }

    fn scroll_mut(&mut self) -> &mut usize {
        &mut self.scroll[tool_index(self.tool)]
    }

    fn filtered_conversations(&self) -> Vec<&crate::ai_conversations::ConversationMeta> {
        let needle = self.conversation_filter.trim().to_lowercase();
        self.conversations
            .iter()
            .filter(|meta| meta.message_count > 0 || meta.summary != "…")
            .filter(|meta| needle.is_empty() || meta.summary.to_lowercase().contains(&needle))
            .collect()
    }
}

fn points_to_pixels(points: f32, dpi: f32) -> f32 {
    Dimension::Points(points).evaluate_as_pixels(DimensionContext {
        dpi,
        pixel_max: 0.0,
        pixel_cell: 0.0,
    })
}

/// The pixel width the panel takes from the window, zero while hidden.
pub(crate) fn panel_pixel_width(visible: bool, width_pt: f32, dpi: f32) -> usize {
    if visible {
        points_to_pixels(clamp_width(width_pt), dpi).round() as usize
    } else {
        0
    }
}

/// Shortens `text` to `max_cells` terminal cells, ending with an ellipsis
/// when cut. Wide (CJK) characters count as two cells.
pub(crate) fn truncate_cells(text: &str, max_cells: usize) -> String {
    if unicode_column_width(text, None) <= max_cells {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_column_width(&ch.to_string(), None);
        if used + w + 1 > max_cells {
            break;
        }
        used += w;
        out.push(ch);
    }
    out.push('…');
    out
}

fn mix(a: SrgbaTuple, b: SrgbaTuple, t: f32) -> SrgbaTuple {
    SrgbaTuple(
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
        1.0,
    )
}

/// Icon and its palette slot for a file name.
fn file_icon(name: &str) -> (&'static str, Option<u8>) {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    match ext {
        "md" | "markdown" | "mdx" => ("\u{eb1d}", Some(4)),
        "json" | "jsonc" | "toml" | "yaml" | "yml" | "lock" => ("\u{eb0f}", Some(3)),
        "rs" | "go" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "swift"
        | "java" | "kt" | "rb" | "lua" | "sh" | "zsh" | "fish" | "bash" | "html" | "css"
        | "scss" | "vue" | "svelte" | "sql" | "zig" | "m" | "mm" | "cs" | "php" => {
            ("\u{eae9}", Some(6))
        }
        _ => ("\u{ea7b}", None),
    }
}

/// `Mon dd hh:mm` for a unix time, in local time.
fn short_time(unix: i64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(unix, 0) {
        chrono::LocalResult::Single(t) => t.format("%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

/// A shell-safe `cd` line for `dir`.
pub(crate) fn cd_command(dir: &Path) -> String {
    let path = dir.to_string_lossy();
    let quoted = shlex::try_quote(&path)
        .map(|q| q.into_owned())
        .unwrap_or_else(|_| format!("'{}'", path.replace('\'', "'\\''")));
    format!("cd {quoted}")
}

/// The command line of a viewer pane for `path`.
pub(crate) fn viewer_args(
    path: &Path,
    line: Option<usize>,
    theme: &str,
    light_terminal: bool,
) -> Vec<String> {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "sumi".to_string());
    let mut args = vec![
        exe,
        crate::file_view::VIEW_FLAG.to_string(),
        path.to_string_lossy().into_owned(),
    ];
    if let Some(line) = line {
        args.push(format!("--line={line}"));
    }
    args.push(format!("--theme={theme}"));
    if light_terminal {
        args.push("--light".to_string());
    }
    args
}

/// True when the terminal background is light, so the viewer's Auto theme
/// picks a light syntax theme for code.
fn is_light(color: SrgbaTuple) -> bool {
    0.2126 * color.0 + 0.7152 * color.1 + 0.0722 * color.2 > 0.5
}

/// The paste that makes a running viewer switch to `path`.
pub(crate) fn viewer_open_request(path: &Path, line: Option<usize>) -> String {
    match line {
        Some(line) => format!("{VIEWER_OPEN_PREFIX}{}:{line}", path.display()),
        None => format!("{VIEWER_OPEN_PREFIX}{}", path.display()),
    }
}

#[cfg(target_os = "macos")]
fn move_to_trash(path: &Path) -> anyhow::Result<()> {
    #[allow(unexpected_cfgs)]
    unsafe {
        use cocoa::base::{id, nil, BOOL, NO};
        use cocoa::foundation::NSString;
        use objc::*;
        let ns_path = NSString::alloc(nil).init_str(&path.to_string_lossy());
        let url: id = msg_send![class!(NSURL), fileURLWithPath: ns_path];
        let manager: id = msg_send![class!(NSFileManager), defaultManager];
        let ok: BOOL = msg_send![manager, trashItemAtURL: url resultingItemURL: nil error: nil];
        let _: () = msg_send![ns_path, release];
        if ok == NO {
            anyhow::bail!("could not move {} to the Trash", path.display());
        }
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn move_to_trash(path: &Path) -> anyhow::Result<()> {
    anyhow::bail!(
        "moving {} to the Trash is not supported here",
        path.display()
    )
}

struct PanelColors {
    background: LinearRgba,
    divider: LinearRgba,
    active: LinearRgba,
    hover: LinearRgba,
    text: LinearRgba,
    dim: LinearRgba,
    faint: LinearRgba,
    accent: LinearRgba,
    field: LinearRgba,
}

/// Padding each row gains above and below for `left_panel_line_height`.
fn row_extra_padding(line_h: f32, line_height: f64) -> f32 {
    let (min, max) = config::LEFT_PANEL_LINE_HEIGHT_RANGE;
    let factor = if line_height.is_finite() {
        line_height.clamp(min, max)
    } else {
        1.0
    };
    (line_h * (factor as f32 - 1.0) / 2.0).max(-line_h * 0.1)
}

/// True for a pane running Sumi's file viewer.
pub(crate) fn is_viewer_pane(pane: &Arc<dyn Pane>) -> bool {
    pane.copy_user_vars()
        .get(VIEWER_USER_VAR)
        .is_some_and(|v| !v.is_empty())
}

/// The Ctrl byte the viewer reads for a Cmd shortcut, so the editor answers
/// to the macOS keys: Cmd+S saves, Cmd+Z undoes, Cmd+Shift+Z redoes.
pub(crate) fn viewer_cmd_key(key: char, shift: bool) -> Option<&'static str> {
    Some(match (key.to_ascii_lowercase(), shift) {
        ('z', true) => "\x19",
        (_, true) => return None,
        ('s', _) => "\x13",
        ('z', _) => "\x1a",
        ('f', _) => "\x06",
        ('g', _) => "\x07",
        ('a', _) => "\x01",
        ('c', _) => "\x03",
        ('x', _) => "\x18",
        ('p', _) => "\x10",
        _ => return None,
    })
}

/// The viewer's byte for a command that reaches the window through the
/// menu bar rather than as a key press.
pub(crate) fn viewer_assignment_key(assignment: &KeyAssignment) -> Option<&'static str> {
    match assignment {
        KeyAssignment::CopyTo(_) => viewer_cmd_key('c', false),
        KeyAssignment::Search(_) => viewer_cmd_key('f', false),
        KeyAssignment::SendStringIfNotAltScreen(text) if text == "\x1f" => {
            viewer_cmd_key('z', false)
        }
        _ => None,
    }
}

impl crate::TermWindow {
    pub(crate) fn left_panel_pixel_width(&self) -> usize {
        panel_pixel_width(
            self.left_panel.visible,
            self.left_panel.width_pt,
            self.dimensions.dpi as f32,
        )
    }

    /// Where the top or bottom tab bar starts: the left panel's right
    /// edge, so the tabs line up with the terminal beside it.
    pub(crate) fn tab_bar_left_offset(&self) -> f32 {
        self.left_panel_pixel_width() as f32
    }

    /// The hotkey drawer and utility windows (Settings) never show the panel.
    fn hotkey_window(&self) -> bool {
        self.left_panel.utility
            || self
                .window_state
                .contains(::window::WindowState::HOTKEY_WINDOW)
    }

    pub(super) fn toggle_left_panel(&mut self) {
        if self.hotkey_window() {
            return;
        }
        self.left_panel.visible = !self.left_panel.visible;
        self.left_panel.persist(&self.config);
        self.left_panel_layout_changed();
    }

    /// Shows `tool`; hides the panel when it already shows that tool, like
    /// Warp's per-tool shortcuts.
    pub(super) fn show_left_panel_tool(&mut self, tool: LeftPanelTool) {
        if self.hotkey_window() {
            return;
        }
        if self.left_panel.visible && self.left_panel.tool == tool {
            self.left_panel.visible = false;
        } else {
            self.left_panel.visible = true;
            self.left_panel.tool = tool;
            self.left_panel.last_poll = None;
        }
        self.left_panel.persist(&self.config);
        self.left_panel_layout_changed();
        if self.left_panel.visible && tool == LeftPanelTool::Search {
            self.focus_left_panel_field(FieldKind::Search);
        }
    }

    fn switch_left_panel_tool(&mut self, tool: LeftPanelTool) {
        if self.left_panel.tool != tool {
            self.left_panel.tool = tool;
            self.left_panel.last_poll = None;
            self.left_panel.persist(&self.config);
        }
        self.left_panel_repaint();
    }

    /// Relayout after the panel appeared, disappeared or changed width.
    pub(super) fn left_panel_layout_changed(&mut self) {
        self.left_panel.invalidate();
        self.invalidate_fancy_tab_bar();
        if let Some(window) = self.window.clone() {
            self.apply_dimensions(&self.dimensions.clone(), None, &window, false);
            self.update_title();
            window.invalidate();
        }
    }

    fn left_panel_repaint(&mut self) {
        self.left_panel.invalidate();
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    fn active_pane_cwd(&self) -> Option<PathBuf> {
        let pane = self.get_active_pane_or_overlay()?;
        let url = pane.get_current_working_dir(CachePolicy::AllowStale)?;
        (url.scheme() == "file").then(|| PathBuf::from(url.path()))
    }

    /// Refreshes the visible tool's model from its source, at most once per
    /// poll interval. Called on every paint while the panel shows.
    fn poll_left_panel(&mut self) {
        let now = Instant::now();
        if let Some(due) = self.left_panel.search_due {
            if now >= due {
                self.left_panel.search_due = None;
                self.start_left_panel_search();
            }
        }
        let cwd = self.active_pane_cwd();
        let mut changed = false;
        if self.left_panel.tool == LeftPanelTool::Files {
            if self.left_panel.files.follow_cwd(cwd.as_deref()) {
                self.left_panel.scroll[0] = 0;
                changed = true;
            }
        }
        let due = self
            .left_panel
            .last_poll
            .map_or(true, |at| now.duration_since(at) >= POLL_INTERVAL);
        if due {
            self.left_panel.last_poll = Some(now);
            match self.left_panel.tool {
                LeftPanelTool::Files => changed |= self.left_panel.files.refresh_if_due(),
                LeftPanelTool::Search => {}
                LeftPanelTool::Snippets => changed |= self.left_panel.snippets.reload_if_changed(),
                LeftPanelTool::Conversations => {
                    let list = crate::ai_conversations::load_index();
                    let active = crate::ai_conversations::active_id();
                    let same = list.len() == self.left_panel.conversations.len()
                        && list
                            .iter()
                            .zip(&self.left_panel.conversations)
                            .all(|(a, b)| {
                                a.id == b.id
                                    && a.summary == b.summary
                                    && a.updated_at == b.updated_at
                            });
                    if !same || active != self.left_panel.active_conversation {
                        self.left_panel.conversations = list;
                        self.left_panel.active_conversation = active;
                        changed = true;
                    }
                }
            }
            // Keep polling while the panel is idle on screen.
            self.update_next_frame_time(Some(now + POLL_INTERVAL));
        }
        if let Some(due) = self.left_panel.search_due {
            self.update_next_frame_time(Some(due));
        }
        if changed {
            self.left_panel.invalidate();
        }
    }

    /// The panel runs the full window height, like Warp's: the traffic
    /// lights sit on its header and the tab bar starts at its right edge.
    fn left_panel_top(&self) -> f32 {
        0.0
    }

    fn left_panel_bottom(&self) -> f32 {
        self.dimensions.pixel_height as f32
    }

    /// Room above the tool strip: clear of the traffic lights, and level
    /// with the bottom of a top tab bar so the tools line up with the
    /// terminal's first row.
    fn left_panel_header_height(&self) -> f32 {
        let dpi = self.dimensions.dpi as f32;
        let has_traffic_lights = self
            .config
            .window_decorations
            .contains(::window::WindowDecorations::INTEGRATED_BUTTONS)
            && !self.layout_is_effective_fullscreen();
        let clearance = points_to_pixels(
            if has_traffic_lights {
                TRAFFIC_LIGHT_CLEARANCE_PT
            } else {
                TOP_GAP_PT
            },
            dpi,
        );
        if self.show_tab_bar && !self.config.tab_bar_at_bottom {
            clearance.max(self.tab_bar_pixel_height().unwrap_or(0.0))
        } else {
            clearance
        }
    }

    fn left_panel_colors(&self, palette: &ColorPalette) -> PanelColors {
        let bg = palette.background;
        let fg = palette.foreground;
        let opacity = self.config.window_background_opacity;
        let to = |c: SrgbaTuple| c.to_linear();
        PanelColors {
            background: to(mix(bg, fg, 0.035)).mul_alpha(opacity),
            divider: to(mix(bg, fg, 0.12)),
            active: to(mix(bg, fg, 0.13)),
            hover: to(mix(bg, fg, 0.07)),
            text: to(fg),
            dim: to(mix(bg, fg, 0.58)),
            faint: to(mix(bg, fg, 0.38)),
            accent: palette
                .resolve_fg(ColorAttribute::PaletteIndex(4))
                .to_linear(),
            field: to(mix(bg, fg, 0.10)),
        }
    }

    fn palette_color(palette: &ColorPalette, idx: Option<u8>, fallback: LinearRgba) -> LinearRgba {
        idx.map(|idx| {
            palette
                .resolve_fg(ColorAttribute::PaletteIndex(idx))
                .to_linear()
        })
        .unwrap_or(fallback)
    }

    fn build_left_panel(
        &mut self,
        palette: &ColorPalette,
    ) -> anyhow::Result<(Vec<ComputedElement>, Vec<RowTarget>, usize)> {
        let font = self.fonts.left_panel_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let dpi = self.dimensions.dpi as f32;
        let px = |pt: f32| points_to_pixels(pt, dpi);
        let width = self.left_panel_pixel_width() as f32;
        let colors = self.left_panel_colors(palette);
        let cell_w = metrics.cell_size.width.max(1) as f32;
        let line_h = metrics.cell_size.height as f32;
        let window_h = self.dimensions.pixel_height as f32;

        let x0 = px(MARGIN_X_PT);
        let inner_w = (width - x0 * 2.0).max(1.0);
        let text_cells = ((inner_w - px(ROW_PAD_X_PT) * 2.0) / cell_w)
            .floor()
            .max(4.0) as usize;
        // `left_panel_line_height` spreads rows apart like the terminal's
        // `line_height`: the extra height goes to the rows' padding.
        let row_pad_y =
            px(ROW_PAD_Y_PT) + row_extra_padding(line_h, self.config.left_panel_line_height);
        let row_h = line_h + row_pad_y * 2.0;

        let gl_state = self.render_state.as_ref().unwrap();
        let layout = |bounds: euclid::Rect<f32, window::PixelUnit>,
                      element: &Element|
         -> anyhow::Result<ComputedElement> {
            self.compute_element(
                &LayoutContext {
                    height: DimensionContext {
                        dpi,
                        pixel_max: window_h,
                        pixel_cell: line_h,
                    },
                    width: DimensionContext {
                        dpi,
                        pixel_max: self.dimensions.pixel_width as f32,
                        pixel_cell: cell_w,
                    },
                    bounds,
                    metrics: &metrics,
                    gl_state,
                    zindex: 10,
                },
                element,
            )
        };
        let rounded = || {
            let r = Dimension::Pixels(px(5.0));
            Some(Corners {
                top_left: SizedPoly {
                    width: r,
                    height: r,
                    poly: TOP_LEFT_ROUNDED_CORNER,
                },
                top_right: SizedPoly {
                    width: r,
                    height: r,
                    poly: TOP_RIGHT_ROUNDED_CORNER,
                },
                bottom_left: SizedPoly {
                    width: r,
                    height: r,
                    poly: BOTTOM_LEFT_ROUNDED_CORNER,
                },
                bottom_right: SizedPoly {
                    width: r,
                    height: r,
                    poly: BOTTOM_RIGHT_ROUNDED_CORNER,
                },
            })
        };
        let text = |s: String, color: LinearRgba| {
            Element::new(&font, ElementContent::Text(s)).colors(ElementColors {
                border: BorderColor::default(),
                bg: LinearRgba::TRANSPARENT.into(),
                text: color.into(),
            })
        };
        let cells = |n: usize| Some(Dimension::Pixels(cell_w * n as f32));
        // A clickable block with hover, optionally highlighted.
        let block = |children: Vec<Element>,
                     item: PanelItem,
                     w: f32,
                     h: f32,
                     pad_x: f32,
                     selected: bool| {
            let bg = if selected {
                colors.active
            } else {
                LinearRgba::TRANSPARENT
            };
            Element::new(&font, ElementContent::Children(children))
                .item_type(UIItemType::LeftPanel(item))
                .display(DisplayType::Block)
                // min sizes are content-box; padding is added around them.
                .min_width(Some(Dimension::Pixels((w - pad_x * 2.0).max(1.0))))
                .min_height(Some(Dimension::Pixels((h - row_pad_y * 2.0).max(1.0))))
                .padding(BoxDimension {
                    left: Dimension::Pixels(pad_x),
                    right: Dimension::Pixels(pad_x),
                    top: Dimension::Pixels(row_pad_y),
                    bottom: Dimension::Pixels(row_pad_y),
                })
                .border_corners(rounded())
                .colors(ElementColors {
                    border: BorderColor::new(bg),
                    bg: bg.into(),
                    text: colors.text.into(),
                })
                .hover_colors((!selected).then(|| ElementColors {
                    border: BorderColor::new(colors.hover),
                    bg: colors.hover.into(),
                    text: colors.text.into(),
                }))
        };

        let mut out = vec![];
        let mut targets = vec![];
        let top = self.left_panel_top();
        let bottom = self.left_panel_bottom();
        let mut y = top + self.left_panel_header_height();

        // Tool strip.
        let tool_w = px(TOOL_BUTTON_PT);
        let tool_h = px(TOOL_STRIP_PT) - px(4.0);
        let mut x = x0;
        for tool in TOOLS {
            let active = tool == self.left_panel.tool;
            let icon_color = if active { colors.text } else { colors.dim };
            let icon = text(tool_icon(tool).to_string(), icon_color)
                .min_width(Some(Dimension::Pixels(cell_w * 2.0)));
            let pad = ((tool_w - cell_w * 2.0) / 2.0).max(0.0);
            let button = block(
                vec![icon],
                PanelItem::Tool(tool),
                tool_w,
                tool_h,
                pad,
                active,
            );
            out.push(layout(euclid::rect(x, y, tool_w, tool_h), &button)?);
            x += tool_w + px(4.0);
        }
        // The active tool's name, right of the buttons when it fits.
        let title = tool_title(self.left_panel.tool);
        let left_cells = ((x0 + inner_w - x) / cell_w).floor() as usize;
        if left_cells > unicode_column_width(&title, None) + 1 {
            let label = text(title, colors.dim);
            out.push(layout(
                euclid::rect(
                    x + px(4.0),
                    y + (tool_h - line_h) / 2.0,
                    x0 + inner_w - x,
                    line_h,
                ),
                &label,
            )?);
        }
        y += px(TOOL_STRIP_PT) + px(SECTION_GAP_PT);

        // Rows shared by every tool: list them, then lay out what fits.
        struct Row {
            target: RowTarget,
            indent: usize,
            icon: Option<(String, LinearRgba)>,
            segments: Vec<(String, LinearRgba)>,
            trailing: Option<String>,
            selected: bool,
        }
        let mut rows: Vec<Row> = vec![];
        let mut footer: Vec<(PanelButton, String, &'static str)> = vec![];
        let mut message: Option<String> = None;

        let field = |kind: FieldKind, value: &str, placeholder: String, w: f32| {
            let (content, color) = if value.is_empty() {
                (placeholder, colors.faint)
            } else {
                (value.to_string(), colors.text)
            };
            let budget = ((w - px(ROW_PAD_X_PT) * 2.0 - cell_w * 2.0) / cell_w).max(1.0) as usize;
            Element::new(
                &font,
                ElementContent::Children(vec![
                    text("\u{ea6d}".to_string(), colors.dim).min_width(cells(2)),
                    text(truncate_cells(&content, budget), color),
                ]),
            )
            .item_type(UIItemType::LeftPanel(PanelItem::Field(kind)))
            .display(DisplayType::Block)
            .min_width(Some(Dimension::Pixels(w - px(ROW_PAD_X_PT) * 2.0)))
            .min_height(Some(Dimension::Pixels(line_h)))
            .padding(BoxDimension {
                left: Dimension::Pixels(px(ROW_PAD_X_PT)),
                right: Dimension::Pixels(px(ROW_PAD_X_PT)),
                top: Dimension::Pixels(px(FIELD_PAD_Y_PT)),
                bottom: Dimension::Pixels(px(FIELD_PAD_Y_PT)),
            })
            // The box model fills rounded corners with the border color, so
            // a field whose outline differs from its fill shows four corner
            // blocks (very visible on dark themes). Corners match the fill.
            .border_corners(rounded())
            .colors(ElementColors {
                border: BorderColor::new(colors.field),
                bg: colors.field.into(),
                text: colors.text.into(),
            })
        };
        let field_h = line_h + px(FIELD_PAD_Y_PT) * 2.0;

        match self.left_panel.tool {
            LeftPanelTool::Files => {
                let root = self.left_panel.files.root().map(Path::to_path_buf);
                match &root {
                    None => message = Some(rust_i18n::t!("left_panel.no_folder").into_owned()),
                    Some(root) => {
                        let name = root
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| root.to_string_lossy().into_owned());
                        let header =
                            text(truncate_cells(&name.to_uppercase(), text_cells), colors.dim);
                        out.push(layout(
                            euclid::rect(x0 + px(ROW_PAD_X_PT), y, inner_w, line_h),
                            &header,
                        )?);
                        y += line_h + px(4.0);
                        let tree_rows = self.left_panel.files.rows();
                        if tree_rows.is_empty() {
                            message = Some(rust_i18n::t!("left_panel.empty_folder").into_owned());
                        }
                        let selected = self.left_panel.files.selected.clone();
                        for row in tree_rows {
                            let entry = row.entry;
                            let name_color = if entry.ignored {
                                colors.faint
                            } else {
                                colors.text
                            };
                            let (chevron, icon, icon_color) = if entry.is_dir {
                                (
                                    if row.expanded { "\u{eab4}" } else { "\u{eab6}" },
                                    if row.expanded { "\u{eaf7}" } else { "\u{ea83}" },
                                    Self::palette_color(palette, Some(4), colors.dim),
                                )
                            } else {
                                let (icon, slot) = file_icon(&entry.name);
                                (" ", icon, Self::palette_color(palette, slot, colors.dim))
                            };
                            let icon_color = if entry.ignored {
                                colors.faint
                            } else {
                                icon_color
                            };
                            rows.push(Row {
                                selected: selected.as_deref() == Some(entry.path.as_path()),
                                target: if entry.is_dir {
                                    RowTarget::Dir(entry.path.clone())
                                } else {
                                    RowTarget::File(entry.path.clone())
                                },
                                indent: row.depth,
                                icon: Some((format!("{chevron}{icon}"), icon_color)),
                                segments: vec![(entry.name, name_color)],
                                trailing: None,
                            });
                        }
                    }
                }
            }
            LeftPanelTool::Search => {
                let toggles_w = px(TOOL_BUTTON_PT) * 2.0 + px(4.0);
                let field_w = (inner_w - toggles_w - px(4.0)).max(cell_w * 6.0);
                let search_field = field(
                    FieldKind::Search,
                    &self.left_panel.search.options.query,
                    rust_i18n::t!("left_panel.search_placeholder").into_owned(),
                    field_w,
                );
                out.push(layout(
                    euclid::rect(x0, y, field_w, field_h),
                    &search_field,
                )?);
                let mut tx = x0 + field_w + px(4.0);
                for (toggle, label, on) in [
                    (
                        SearchToggle::MatchCase,
                        "\u{eab1}",
                        self.left_panel.search.options.match_case,
                    ),
                    (
                        SearchToggle::Regex,
                        "\u{eb38}",
                        self.left_panel.search.options.regex,
                    ),
                ] {
                    let w = px(TOOL_BUTTON_PT);
                    let pad = ((w - cell_w * 2.0) / 2.0).max(0.0);
                    let button = block(
                        vec![text(
                            label.to_string(),
                            if on { colors.accent } else { colors.dim },
                        )
                        .min_width(cells(2))],
                        PanelItem::Toggle(toggle),
                        w,
                        field_h,
                        pad,
                        on,
                    );
                    out.push(layout(euclid::rect(tx, y, w, field_h), &button)?);
                    tx += w + px(4.0);
                }
                y += field_h + px(SECTION_GAP_PT);

                let state = &self.left_panel.search;
                let status = if let Some(err) = &state.outcome.error {
                    Some(truncate_cells(err, text_cells))
                } else if state.running {
                    Some(rust_i18n::t!("left_panel.searching").into_owned())
                } else if state.options.query.is_empty() {
                    Some(rust_i18n::t!("left_panel.search_hint").into_owned())
                } else if state.outcome.total == 0 {
                    Some(rust_i18n::t!("left_panel.no_results").into_owned())
                } else if state.outcome.truncated {
                    Some(
                        rust_i18n::t!("left_panel.results_truncated", count = state.outcome.total)
                            .into_owned(),
                    )
                } else {
                    Some(
                        rust_i18n::t!(
                            "left_panel.results",
                            count = state.outcome.total,
                            files = state.outcome.files.len()
                        )
                        .into_owned(),
                    )
                };
                if let Some(status) = status {
                    let status = text(truncate_cells(&status, text_cells), colors.dim);
                    out.push(layout(
                        euclid::rect(x0 + px(ROW_PAD_X_PT), y, inner_w, line_h),
                        &status,
                    )?);
                    y += line_h + px(4.0);
                }
                let root = state.root.clone();
                let highlight = Self::palette_color(palette, Some(3), colors.accent);
                for file in &state.outcome.files {
                    let collapsed = state.collapsed.contains(&file.path);
                    let rel = file_tree::relative_to(&file.path, root.as_deref());
                    let (dir, name) = match rel.rsplit_once('/') {
                        Some((dir, name)) => (format!("  {dir}"), name.to_string()),
                        None => (String::new(), rel.clone()),
                    };
                    let (icon, slot) = file_icon(&name);
                    rows.push(Row {
                        target: RowTarget::SearchFile(file.path.clone()),
                        indent: 0,
                        icon: Some((
                            format!("{}{icon}", if collapsed { "\u{eab6}" } else { "\u{eab4}" }),
                            Self::palette_color(palette, slot, colors.dim),
                        )),
                        segments: vec![(name, colors.text), (dir, colors.faint)],
                        trailing: Some(file.matches.len().to_string()),
                        selected: false,
                    });
                    if collapsed {
                        continue;
                    }
                    for m in &file.matches {
                        let mut segments = vec![];
                        let chars: Vec<char> = m.text.chars().collect();
                        let mut at = 0;
                        for (s, e) in &m.ranges {
                            let (s, e) = ((*s).min(chars.len()), (*e).min(chars.len()));
                            if s < at {
                                continue;
                            }
                            if s > at {
                                segments.push((chars[at..s].iter().collect(), colors.dim));
                            }
                            segments.push((chars[s..e].iter().collect(), highlight));
                            at = e;
                        }
                        if at < chars.len() {
                            segments.push((chars[at..].iter().collect(), colors.dim));
                        }
                        rows.push(Row {
                            target: RowTarget::SearchMatch {
                                path: file.path.clone(),
                                line: m.line,
                            },
                            indent: 1,
                            icon: None,
                            segments,
                            trailing: Some(m.line.to_string()),
                            selected: false,
                        });
                    }
                }
            }
            LeftPanelTool::Snippets => {
                let store = &self.left_panel.snippets;
                if let Some(err) = &store.error {
                    message = Some(format!("snippets.toml: {err}"));
                } else if store.snippets.is_empty() {
                    message = Some(rust_i18n::t!("left_panel.no_snippets").into_owned());
                }
                let run_color = Self::palette_color(palette, Some(2), colors.dim);
                for (idx, snippet) in store.snippets.iter().enumerate() {
                    let label = snippet.label().to_string();
                    let first_line = snippet.command.lines().next().unwrap_or_default();
                    let detail = if label != first_line {
                        format!("  {first_line}")
                    } else {
                        String::new()
                    };
                    rows.push(Row {
                        target: RowTarget::Snippet(idx),
                        indent: 0,
                        icon: Some(("\u{eb2c}".to_string(), run_color)),
                        segments: vec![(label, colors.text), (detail, colors.faint)],
                        trailing: None,
                        selected: false,
                    });
                }
                footer.push((
                    PanelButton::NewSnippet,
                    rust_i18n::t!("left_panel.new_snippet").into_owned(),
                    "\u{ea60}",
                ));
                footer.push((
                    PanelButton::EditSnippets,
                    rust_i18n::t!("left_panel.edit_snippets").into_owned(),
                    "\u{ea73}",
                ));
            }
            LeftPanelTool::Conversations => {
                let filter = field(
                    FieldKind::ConversationFilter,
                    &self.left_panel.conversation_filter,
                    rust_i18n::t!("left_panel.filter_placeholder").into_owned(),
                    inner_w,
                );
                out.push(layout(euclid::rect(x0, y, inner_w, field_h), &filter)?);
                y += field_h + px(SECTION_GAP_PT);
                let list = self.left_panel.filtered_conversations();
                if list.is_empty() {
                    message = Some(rust_i18n::t!("left_panel.no_conversations").into_owned());
                }
                let active = self.left_panel.active_conversation.clone();
                for meta in list {
                    rows.push(Row {
                        target: RowTarget::Conversation(meta.id.clone()),
                        indent: 0,
                        icon: Some(("\u{ea6b}".to_string(), colors.dim)),
                        segments: vec![(meta.summary.clone(), colors.text)],
                        trailing: Some(short_time(meta.updated_at)),
                        selected: active.as_deref() == Some(meta.id.as_str()),
                    });
                }
                footer.push((
                    PanelButton::NewChat,
                    rust_i18n::t!("left_panel.new_chat").into_owned(),
                    "\u{ea60}",
                ));
            }
        }

        // Footer buttons sit at the bottom edge.
        let footer_h = row_h;
        let list_bottom = bottom - px(SECTION_GAP_PT) - footer.len() as f32 * (footer_h + px(2.0));
        let mut fy = list_bottom + px(SECTION_GAP_PT) / 2.0;
        for (button, label, icon) in footer {
            let element = block(
                vec![
                    text(icon.to_string(), colors.dim).min_width(cells(2)),
                    text(
                        truncate_cells(&label, text_cells.saturating_sub(2)),
                        colors.dim,
                    ),
                ],
                PanelItem::Button(button),
                inner_w,
                footer_h,
                px(ROW_PAD_X_PT),
                false,
            );
            out.push(layout(euclid::rect(x0, fy, inner_w, footer_h), &element)?);
            fy += footer_h + px(2.0);
        }

        if let Some(message) = message {
            // Wrap the message by cells across a few lines.
            let mut line = String::new();
            let mut lines = vec![];
            for ch in message.chars() {
                line.push(ch);
                if unicode_column_width(&line, None) >= text_cells {
                    lines.push(std::mem::take(&mut line));
                }
            }
            if !line.is_empty() {
                lines.push(line);
            }
            for line in lines.into_iter().take(6) {
                let element = text(line, colors.faint);
                out.push(layout(
                    euclid::rect(x0 + px(ROW_PAD_X_PT), y, inner_w, line_h),
                    &element,
                )?);
                y += line_h;
            }
            y += px(4.0);
        }

        let fits = (((list_bottom - y) / (row_h + 1.0)).floor().max(0.0)) as usize;
        let scroll = self.left_panel.scroll[tool_index(self.left_panel.tool)]
            .min(rows.len().saturating_sub(1));
        let indent_px = px(INDENT_PT);
        for row in rows.into_iter().skip(scroll).take(fits) {
            let indent = indent_px * row.indent as f32;
            let row_w = (inner_w - indent).max(cell_w * 4.0);
            let budget = ((row_w - px(ROW_PAD_X_PT) * 2.0) / cell_w).floor().max(2.0) as usize;
            let mut children = vec![];
            let mut used = 0;
            if let Some((icon, color)) = row.icon {
                let icon_cells = icon.chars().count() * 2;
                children.push(text(icon, color).min_width(cells(icon_cells)));
                used += icon_cells;
            }
            let trailing_cells = row
                .trailing
                .as_deref()
                .map(|t| unicode_column_width(t, None) + 1)
                .unwrap_or(0);
            let mut remaining = budget.saturating_sub(used + trailing_cells);
            for (segment, color) in row.segments {
                if remaining == 0 || segment.is_empty() {
                    continue;
                }
                let shown = truncate_cells(&segment, remaining);
                remaining = remaining.saturating_sub(unicode_column_width(&shown, None));
                children.push(text(shown, color));
            }
            if let Some(trailing) = row.trailing {
                children.push(text(trailing, colors.faint).float(Float::Right));
            }
            let idx = targets.len();
            targets.push(row.target);
            let element = block(
                children,
                PanelItem::Row(idx),
                row_w,
                row_h,
                px(ROW_PAD_X_PT),
                row.selected,
            );
            out.push(layout(
                euclid::rect(x0 + indent, y, row_w, row_h),
                &element,
            )?);
            y += row_h + 1.0;
        }
        *self.left_panel.scroll_mut() = scroll;
        Ok((out, targets, fits))
    }

    /// Paints the panel and registers its hit regions. Rows are drawn over
    /// the window's left border region that `get_os_border` reserved.
    pub(super) fn paint_left_panel(
        &mut self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let width = self.left_panel_pixel_width();
        if width == 0 {
            return Ok(());
        }
        self.poll_left_panel();
        let width = width as f32;
        let top = self.left_panel_top();
        let bottom = self.left_panel_bottom();
        let height = (bottom - top).max(0.0);
        let palette = self.palette().clone();
        let colors = self.left_panel_colors(&palette);
        let dpi = self.dimensions.dpi as f32;

        self.filled_rectangle(
            layers,
            1,
            euclid::rect(0.0, top, width, height),
            colors.background,
        )?;
        let divider = points_to_pixels(1.0, dpi).max(1.0).round();
        self.filled_rectangle(
            layers,
            2,
            euclid::rect(width - divider, top, divider, height),
            colors.divider,
        )?;

        if self.left_panel.element.is_none() {
            let (built, targets, fits) = self.build_left_panel(&palette)?;
            self.left_panel.element.replace(built);
            self.left_panel.targets = targets;
            self.left_panel.fits_rows = fits;
        }

        let header_h = self.left_panel_header_height();
        let handle = points_to_pixels(RESIZE_HANDLE_PT, dpi);
        let mut items = vec![
            UIItem {
                x: 0,
                y: top as usize,
                width: width as usize,
                height: height as usize,
                item_type: UIItemType::LeftPanel(PanelItem::Background),
            },
            UIItem {
                x: 0,
                y: top as usize,
                width: width as usize,
                height: header_h as usize,
                item_type: UIItemType::LeftPanel(PanelItem::Header),
            },
        ];
        let elements = self.left_panel.element.take().unwrap_or_default();
        let gl_state = self.render_state.as_ref().unwrap();
        let mut result = Ok(());
        for element in &elements {
            items.extend(element.ui_items());
            if let Err(err) = self.render_element(element, gl_state, None) {
                result = Err(err);
                break;
            }
        }
        self.left_panel.element.replace(elements);
        items.push(UIItem {
            x: (width - handle / 2.0).max(0.0) as usize,
            y: top as usize,
            width: handle as usize,
            height: height as usize,
            item_type: UIItemType::LeftPanel(PanelItem::Resize),
        });
        self.ui_items.extend(items);
        result
    }

    pub(super) fn mouse_event_left_panel(
        &mut self,
        item: PanelItem,
        ui_item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let mut cursor = MouseCursor::Arrow;
        match (item, &event.kind) {
            (PanelItem::Resize, kind) => {
                cursor = MouseCursor::SizeLeftRight;
                if *kind == WMEK::Press(MousePress::Left) {
                    self.dragging.replace((ui_item, event.clone()));
                }
            }
            (PanelItem::Header, _) => {
                self.mouse_event_tab_bar(
                    crate::tabbar::TabBarItem::None,
                    ui_item,
                    event.clone(),
                    context,
                );
                return;
            }
            (PanelItem::Tool(tool), WMEK::Press(MousePress::Left)) => {
                self.switch_left_panel_tool(tool);
            }
            (PanelItem::Field(kind), WMEK::Press(MousePress::Left)) => {
                cursor = MouseCursor::Text;
                self.open_panel_prompt_at(
                    match kind {
                        FieldKind::Search => PanelPrompt::Search,
                        FieldKind::ConversationFilter => PanelPrompt::ConversationFilter,
                    },
                    ui_item,
                );
            }
            (PanelItem::Field(_), _) => cursor = MouseCursor::Text,
            (PanelItem::Toggle(toggle), WMEK::Press(MousePress::Left)) => {
                let options = &mut self.left_panel.search.options;
                match toggle {
                    SearchToggle::MatchCase => options.match_case = !options.match_case,
                    SearchToggle::Regex => options.regex = !options.regex,
                }
                self.start_left_panel_search();
            }
            (PanelItem::Button(button), WMEK::Press(MousePress::Left)) => match button {
                PanelButton::NewSnippet => {
                    self.run_left_panel_command(LeftPanelCommand::NewSnippet)
                }
                PanelButton::EditSnippets => {
                    self.run_left_panel_command(LeftPanelCommand::EditSnippets)
                }
                PanelButton::NewChat => self.open_ai_chat(None),
            },
            (PanelItem::Row(idx), WMEK::Press(press)) => {
                if let Some(target) = self.left_panel.targets.get(idx).cloned() {
                    self.left_panel_row_press(target, *press, &event, context);
                }
            }
            (_, WMEK::VertWheel(n)) => {
                self.scroll_left_panel(if *n > 0 { -3 } else { 3 });
            }
            _ => {}
        }
        context.set_cursor(Some(cursor));
    }

    fn left_panel_row_press(
        &mut self,
        target: RowTarget,
        press: MousePress,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) {
        match (target, press) {
            (RowTarget::Dir(dir), MousePress::Left) => {
                self.left_panel.files.toggle(&dir);
                self.left_panel_repaint();
            }
            (RowTarget::File(path), MousePress::Left) => {
                self.open_file_in_viewer(&path, None, false);
            }
            (RowTarget::File(path), MousePress::Middle) => {
                self.open_file_in_viewer(&path, None, true);
            }
            (RowTarget::SearchFile(path), MousePress::Left) => {
                let collapsed = &mut self.left_panel.search.collapsed;
                if !collapsed.remove(&path) {
                    collapsed.insert(path);
                }
                self.left_panel_repaint();
            }
            (RowTarget::SearchMatch { path, line }, MousePress::Left) => {
                self.open_file_in_viewer(&path, Some(line), false);
            }
            (RowTarget::Snippet(idx), MousePress::Left) => {
                self.run_left_panel_command(LeftPanelCommand::InsertSnippet(idx));
            }
            (RowTarget::Conversation(id), MousePress::Left) => self.open_ai_chat(Some(id)),
            (target, MousePress::Right) => self.show_left_panel_menu(&target, event, context),
            _ => {}
        }
    }

    fn scroll_left_panel(&mut self, delta: isize) {
        let total = self.left_panel_row_count();
        let max = total.saturating_sub(self.left_panel.fits_rows.max(1));
        let scroll = self.left_panel.scroll_mut();
        let next = (*scroll as isize + delta).clamp(0, max as isize) as usize;
        if next != *scroll {
            *scroll = next;
            self.left_panel_repaint();
        }
    }

    fn left_panel_row_count(&self) -> usize {
        match self.left_panel.tool {
            LeftPanelTool::Files => self.left_panel.files.rows().len(),
            LeftPanelTool::Search => {
                let state = &self.left_panel.search;
                state
                    .outcome
                    .files
                    .iter()
                    .map(|f| {
                        1 + if state.collapsed.contains(&f.path) {
                            0
                        } else {
                            f.matches.len()
                        }
                    })
                    .sum()
            }
            LeftPanelTool::Snippets => self.left_panel.snippets.snippets.len(),
            LeftPanelTool::Conversations => self.left_panel.filtered_conversations().len(),
        }
    }

    /// Follows a drag of the panel's right edge.
    pub(super) fn drag_left_panel_resize(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
    ) {
        let dpi = self.dimensions.dpi as f32;
        let one_point = points_to_pixels(1.0, dpi).max(f32::EPSILON);
        let width = clamp_width(event.coords.x.max(0) as f32 / one_point);
        if (width - self.left_panel.width_pt).abs() >= 1.0 {
            self.left_panel.width_pt = width.round();
            self.left_panel_layout_changed();
        }
        self.dragging.replace((item, start_event));
    }

    pub(super) fn finish_left_panel_resize(&mut self) {
        self.left_panel.persist(&self.config);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }

    // ---------- Prompts ----------

    fn focus_left_panel_field(&mut self, kind: FieldKind) {
        // The field's rectangle is known after the next paint; open the
        // editor then.
        let window = self.window.clone();
        if let Some(window) = window {
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let item = term_window
                    .ui_items
                    .iter()
                    .find(|item| item.item_type == UIItemType::LeftPanel(PanelItem::Field(kind)))
                    .cloned();
                if let Some(item) = item {
                    term_window.open_panel_prompt_at(
                        match kind {
                            FieldKind::Search => PanelPrompt::Search,
                            FieldKind::ConversationFilter => PanelPrompt::ConversationFilter,
                        },
                        item,
                    );
                }
            })));
        }
    }

    fn open_panel_prompt_at(&mut self, prompt: PanelPrompt, anchor: UIItem) {
        let initial = match &prompt {
            PanelPrompt::Search => self.left_panel.search.options.query.clone(),
            PanelPrompt::ConversationFilter => self.left_panel.conversation_filter.clone(),
            PanelPrompt::Rename { path } => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            PanelPrompt::NewFile { .. }
            | PanelPrompt::NewFolder { .. }
            | PanelPrompt::NewSnippet => String::new(),
        };
        let modal = crate::termwindow::tab_rename::TabRenameModal::new_panel_prompt(
            self, prompt, anchor, initial,
        );
        self.set_modal(Rc::new(modal));
    }

    /// Opens a prompt over the panel's first row slot, for actions started
    /// from a menu (new file, rename, new snippet).
    fn open_panel_prompt(&mut self, prompt: PanelPrompt) {
        let dpi = self.dimensions.dpi as f32;
        let px = |pt: f32| points_to_pixels(pt, dpi);
        let x = px(MARGIN_X_PT);
        let width = (self.left_panel_pixel_width() as f32 - x * 2.0).max(1.0);
        let y = self.left_panel_top() + self.left_panel_header_height() + px(TOOL_STRIP_PT);
        let height = self
            .fonts
            .left_panel_font()
            .map(|font| {
                RenderMetrics::with_font_metrics(&font.metrics())
                    .cell_size
                    .height as f32
            })
            .unwrap_or(16.0)
            + px(FIELD_PAD_Y_PT) * 2.0;
        let anchor = UIItem {
            x: x as usize,
            y: y as usize,
            width: width as usize,
            height: height as usize,
            item_type: UIItemType::LeftPanel(PanelItem::Background),
        };
        self.open_panel_prompt_at(prompt, anchor);
    }

    /// Called by the prompt editor on every edit.
    pub(crate) fn left_panel_prompt_changed(&mut self, prompt: &PanelPrompt, value: &str) {
        match prompt {
            PanelPrompt::Search if self.left_panel.search.options.query != value => {
                self.left_panel.search.options.query = value.to_string();
                self.left_panel.search_due = Some(Instant::now() + SEARCH_DEBOUNCE);
                self.update_next_frame_time(Some(Instant::now() + SEARCH_DEBOUNCE));
                self.left_panel_repaint();
            }
            PanelPrompt::ConversationFilter if self.left_panel.conversation_filter != value => {
                self.left_panel.conversation_filter = value.to_string();
                *self.left_panel.scroll_mut() = 0;
                self.left_panel_repaint();
            }
            _ => {}
        }
    }

    /// Called by the prompt editor on Enter.
    pub(crate) fn left_panel_prompt_commit(&mut self, prompt: PanelPrompt, value: String) {
        let name = value.trim().to_string();
        let result = match prompt {
            PanelPrompt::Search => {
                self.left_panel.search.options.query = value;
                self.left_panel.search_due = None;
                self.start_left_panel_search();
                Ok(())
            }
            PanelPrompt::ConversationFilter => {
                self.left_panel_prompt_changed(&PanelPrompt::ConversationFilter, &value);
                Ok(())
            }
            _ if name.is_empty() || name.contains('/') => Ok(()),
            PanelPrompt::NewFile { dir } => {
                let path = dir.join(&name);
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map(|_| {
                        self.left_panel.files.reveal(&path);
                        self.left_panel.files.selected = Some(path.clone());
                        self.left_panel.files.refresh();
                        self.open_file_in_viewer(&path, None, false);
                    })
                    .map_err(anyhow::Error::from)
            }
            PanelPrompt::NewFolder { dir } => std::fs::create_dir(dir.join(&name))
                .map(|_| {
                    self.left_panel.files.refresh();
                })
                .map_err(anyhow::Error::from),
            PanelPrompt::Rename { path } => {
                let target = path.with_file_name(&name);
                if target.exists() {
                    Err(anyhow::anyhow!("{} already exists", target.display()))
                } else {
                    std::fs::rename(&path, &target)
                        .map(|_| {
                            self.left_panel.files.refresh();
                        })
                        .map_err(anyhow::Error::from)
                }
            }
            PanelPrompt::NewSnippet => self.left_panel.snippets.add(value.trim().to_string()),
        };
        if let Err(err) = result {
            log::warn!("left panel: {err:#}");
            self.show_left_panel_error(&format!("{err:#}"));
        }
        self.left_panel_repaint();
    }

    fn show_left_panel_error(&mut self, message: &str) {
        self.show_toast(message.to_string());
    }

    // ---------- Search ----------

    fn start_left_panel_search(&mut self) {
        let options = self.left_panel.search.options.clone();
        let root = self
            .active_pane_cwd()
            .map(|cwd| project_root(&cwd))
            .or_else(|| self.left_panel.files.root().map(Path::to_path_buf));
        let (generation, current) = self.left_panel.search.next_generation();
        self.left_panel.search.collapsed.clear();
        *self.left_panel.scroll_mut() = 0;
        if options.query.is_empty() || root.is_none() {
            self.left_panel.search.running = false;
            self.left_panel.search.outcome = Default::default();
            self.left_panel_repaint();
            return;
        }
        let root = root.unwrap();
        self.left_panel.search.root = Some(root.clone());
        self.left_panel.search.running = true;
        self.left_panel_repaint();
        let Some(window) = self.window.clone() else {
            return;
        };
        std::thread::spawn(move || {
            let Some(outcome) = search::run_search(&root, &options, generation, &current) else {
                return;
            };
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let state = &mut term_window.left_panel.search;
                if !state.is_current(generation) {
                    return;
                }
                state.running = false;
                state.outcome = outcome;
                term_window.left_panel_repaint();
            })));
        });
    }

    // ---------- Files ----------

    /// The pane in the active tab that runs the viewer, if any.
    fn viewer_pane_in_active_tab(&self) -> Option<Arc<dyn Pane>> {
        let tab = self.get_active_tab()?;
        tab.iter_panes_ignoring_zoom()
            .into_iter()
            .map(|pos| pos.pane)
            .find(is_viewer_pane)
    }

    /// Shows `path` in the viewer: the tab's viewer pane switches to it, or
    /// a new pane splits to the right of the active one.
    pub(crate) fn open_file_in_viewer(&mut self, path: &Path, line: Option<usize>, new_tab: bool) {
        self.left_panel.files.selected = Some(path.to_path_buf());
        self.left_panel_repaint();
        // Images, PDFs and other binaries go to their default app.
        if path.is_file() && crate::file_view::looks_binary(path) {
            if let Err(err) = std::process::Command::new("/usr/bin/open")
                .arg(path)
                .spawn()
            {
                self.show_left_panel_error(&err.to_string());
            }
            return;
        }
        if !new_tab {
            if let Some(pane) = self.viewer_pane_in_active_tab() {
                if let Err(err) = pane.send_paste(&viewer_open_request(path, line)) {
                    log::warn!("asking the viewer to open {}: {err:#}", path.display());
                }
                if let Some(tab) = self.get_active_tab() {
                    tab.set_active_pane(&pane);
                }
                return;
            }
        }
        let Some(pane) = self.get_active_pane_or_overlay() else {
            return;
        };
        let theme = self.config.markdown_theme.clone();
        let light_terminal = is_light(self.palette().background);
        let spawn = SpawnCommand {
            args: Some(viewer_args(path, line, &theme, light_terminal)),
            cwd: path.parent().map(Path::to_path_buf),
            ..Default::default()
        };
        let action = if new_tab {
            KeyAssignment::SpawnCommandInNewTab(spawn)
        } else {
            KeyAssignment::SplitHorizontal(spawn)
        };
        if let Err(err) = self.perform_key_assignment(&pane, &action) {
            log::warn!("opening viewer for {}: {err:#}", path.display());
        }
    }

    fn get_active_tab(&self) -> Option<Arc<mux::tab::Tab>> {
        mux::Mux::get()
            .get_window(self.mux_window_id)?
            .get_active()
            .cloned()
    }

    // ---------- Menus ----------

    fn show_left_panel_menu(
        &mut self,
        target: &RowTarget,
        event: &MouseEvent,
        context: &dyn WindowOps,
    ) {
        #[cfg(target_os = "macos")]
        {
            use window::os::macos::menu::{Menu, MenuItem};
            let Some(pane_id) = self
                .get_active_pane_or_overlay()
                .map(|pane| pane.pane_id().as_usize())
            else {
                return;
            };
            let t = |key: &str| rust_i18n::t!(key).into_owned();
            let cmd = |c: LeftPanelCommand| KeyAssignment::LeftPanelCommand(c);
            let s = |p: &Path| p.to_string_lossy().into_owned();
            let copy = |text: String| KeyAssignment::CopyTextTo {
                text,
                destination: ClipboardCopyDestination::Clipboard,
            };
            let root = self.left_panel.files.root().map(Path::to_path_buf);
            let mut entries: Vec<Option<(String, KeyAssignment)>> = vec![];
            let path_entries = |path: &Path, entries: &mut Vec<Option<(String, KeyAssignment)>>| {
                entries.push(None);
                entries.push(Some((t("left_panel.menu_copy_path"), copy(s(path)))));
                entries.push(Some((
                    t("left_panel.menu_copy_relative_path"),
                    copy(file_tree::relative_to(path, root.as_deref())),
                )));
                entries.push(Some((
                    t("left_panel.menu_reveal"),
                    cmd(LeftPanelCommand::RevealInFinder(s(path))),
                )));
            };
            match target {
                RowTarget::File(path)
                | RowTarget::SearchFile(path)
                | RowTarget::SearchMatch { path, .. } => {
                    let line = match target {
                        RowTarget::SearchMatch { line, .. } => Some(*line),
                        _ => None,
                    };
                    entries.push(Some((
                        t("left_panel.menu_open"),
                        cmd(LeftPanelCommand::OpenFile {
                            path: s(path),
                            line,
                            new_tab: false,
                        }),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_open_new_tab"),
                        cmd(LeftPanelCommand::OpenFile {
                            path: s(path),
                            line,
                            new_tab: true,
                        }),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_open_default_app"),
                        cmd(LeftPanelCommand::OpenWithDefaultApp(s(path))),
                    )));
                    path_entries(path, &mut entries);
                    if matches!(target, RowTarget::File(_)) {
                        entries.push(None);
                        entries.push(Some((
                            t("left_panel.menu_rename"),
                            cmd(LeftPanelCommand::Rename(s(path))),
                        )));
                        entries.push(Some((
                            t("left_panel.menu_delete"),
                            cmd(LeftPanelCommand::Delete(s(path))),
                        )));
                    }
                }
                RowTarget::Dir(dir) => {
                    entries.push(Some((
                        t("left_panel.menu_new_file"),
                        cmd(LeftPanelCommand::NewFile(s(dir))),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_new_folder"),
                        cmd(LeftPanelCommand::NewFolder(s(dir))),
                    )));
                    entries.push(None);
                    entries.push(Some((
                        t("left_panel.menu_cd"),
                        cmd(LeftPanelCommand::ChangeDirectory(s(dir))),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_new_tab_here"),
                        cmd(LeftPanelCommand::NewTabInDirectory(s(dir))),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_set_root"),
                        cmd(LeftPanelCommand::SetRoot(s(dir))),
                    )));
                    path_entries(dir, &mut entries);
                    entries.push(None);
                    entries.push(Some((
                        t("left_panel.menu_rename"),
                        cmd(LeftPanelCommand::Rename(s(dir))),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_delete"),
                        cmd(LeftPanelCommand::Delete(s(dir))),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_refresh"),
                        cmd(LeftPanelCommand::RefreshFiles),
                    )));
                }
                RowTarget::Snippet(idx) => {
                    let command = self
                        .left_panel
                        .snippets
                        .snippets
                        .get(*idx)
                        .map(|s| s.command.clone())
                        .unwrap_or_default();
                    entries.push(Some((
                        t("left_panel.menu_run"),
                        cmd(LeftPanelCommand::RunSnippet(*idx)),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_insert"),
                        cmd(LeftPanelCommand::InsertSnippet(*idx)),
                    )));
                    entries.push(Some((t("left_panel.menu_copy"), copy(command))));
                    entries.push(None);
                    entries.push(Some((
                        t("left_panel.menu_edit_file"),
                        cmd(LeftPanelCommand::EditSnippets),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_delete"),
                        cmd(LeftPanelCommand::DeleteSnippet(*idx)),
                    )));
                }
                RowTarget::Conversation(id) => {
                    entries.push(Some((
                        t("left_panel.menu_open"),
                        cmd(LeftPanelCommand::OpenConversation(id.clone())),
                    )));
                    entries.push(Some((
                        t("left_panel.menu_delete"),
                        cmd(LeftPanelCommand::DeleteConversation(id.clone())),
                    )));
                }
            }
            let menu = Menu::new_with_title("");
            for entry in entries {
                match entry {
                    None => menu.add_item(&MenuItem::new_separator()),
                    Some((title, action)) => {
                        menu.add_item(&crate::termwindow::mouseevent::pane_action_menu_item(
                            &title, action, pane_id,
                        ))
                    }
                }
            }
            self.size_menu_for_touch(&menu);
            context.show_context_menu(menu, event.screen_coords);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = (target, event, context);
    }

    // ---------- Commands ----------

    pub(super) fn run_left_panel_command(&mut self, command: LeftPanelCommand) {
        let result: anyhow::Result<()> = (|| {
            match command {
                LeftPanelCommand::OpenFile {
                    path,
                    line,
                    new_tab,
                } => self.open_file_in_viewer(Path::new(&path), line, new_tab),
                LeftPanelCommand::OpenWithDefaultApp(path) => {
                    std::process::Command::new("/usr/bin/open")
                        .arg(&path)
                        .spawn()?;
                }
                LeftPanelCommand::RevealInFinder(path) => {
                    std::process::Command::new("/usr/bin/open")
                        .arg("-R")
                        .arg(&path)
                        .spawn()?;
                }
                LeftPanelCommand::ChangeDirectory(dir) => {
                    if let Some(pane) = self.get_active_pane_or_overlay() {
                        pane.send_paste(&cd_command(Path::new(&dir)))?;
                        pane.writer().write_all(b"\r")?;
                    }
                }
                LeftPanelCommand::NewTabInDirectory(dir) => {
                    if let Some(pane) = self.get_active_pane_or_overlay() {
                        self.perform_key_assignment(
                            &pane,
                            &KeyAssignment::SpawnCommandInNewTab(SpawnCommand {
                                cwd: Some(PathBuf::from(dir)),
                                ..Default::default()
                            }),
                        )?;
                    }
                }
                LeftPanelCommand::NewFile(dir) => self.open_panel_prompt(PanelPrompt::NewFile {
                    dir: PathBuf::from(dir),
                }),
                LeftPanelCommand::NewFolder(dir) => {
                    self.open_panel_prompt(PanelPrompt::NewFolder {
                        dir: PathBuf::from(dir),
                    })
                }
                LeftPanelCommand::Rename(path) => self.open_panel_prompt(PanelPrompt::Rename {
                    path: PathBuf::from(path),
                }),
                LeftPanelCommand::Delete(path) => {
                    move_to_trash(Path::new(&path))?;
                    self.left_panel.files.refresh();
                }
                LeftPanelCommand::SetRoot(dir) => {
                    self.left_panel.files.pin_root(PathBuf::from(dir));
                    self.left_panel.scroll[0] = 0;
                }
                LeftPanelCommand::RefreshFiles => {
                    self.left_panel.files.refresh();
                }
                LeftPanelCommand::RunSnippet(idx) | LeftPanelCommand::InsertSnippet(idx) => {
                    let run = matches!(command, LeftPanelCommand::RunSnippet(_));
                    let snippet = self.left_panel.snippets.snippets.get(idx).cloned();
                    if let (Some(snippet), Some(pane)) =
                        (snippet, self.get_active_pane_or_overlay())
                    {
                        pane.send_paste(&snippet.command)?;
                        if run && !snippet.has_placeholder() {
                            pane.writer().write_all(b"\r")?;
                        }
                        self.focus_terminal();
                    }
                }
                LeftPanelCommand::CopySnippet(idx) => {
                    if let Some(snippet) = self.left_panel.snippets.snippets.get(idx) {
                        self.copy_to_clipboard(
                            ClipboardCopyDestination::Clipboard,
                            snippet.command.clone(),
                        );
                    }
                }
                LeftPanelCommand::DeleteSnippet(idx) => self.left_panel.snippets.remove(idx)?,
                LeftPanelCommand::NewSnippet => {
                    self.switch_left_panel_tool(LeftPanelTool::Snippets);
                    self.open_panel_prompt(PanelPrompt::NewSnippet);
                }
                LeftPanelCommand::EditSnippets => {
                    let path = self.left_panel.snippets.ensure_file()?;
                    self.open_file_in_viewer(&path, None, false);
                }
                LeftPanelCommand::OpenConversation(id) => self.open_ai_chat(Some(id)),
                LeftPanelCommand::DeleteConversation(id) => {
                    crate::ai_conversations::delete_conversation(&id)?;
                    self.left_panel.last_poll = None;
                }
            }
            Ok(())
        })();
        if let Err(err) = result {
            log::warn!("left panel: {err:#}");
            self.show_left_panel_error(&format!("{err:#}"));
        }
        self.left_panel_repaint();
    }

    /// Opens the AI chat overlay on `id`, or on a new conversation.
    fn open_ai_chat(&mut self, id: Option<String>) {
        let switched = match &id {
            Some(id) => crate::ai_conversations::switch_active(id).map(|_| ()),
            None => crate::ai_conversations::start_new_active().map(|_| ()),
        };
        if let Err(err) = switched {
            self.show_left_panel_error(&format!("{err:#}"));
            return;
        }
        self.left_panel.active_conversation = crate::ai_conversations::active_id();
        self.left_panel.last_poll = None;
        let Some(pane) = self.get_active_pane_no_overlay() else {
            return;
        };
        // An open overlay holds its own copy of the conversation; reopen it
        // so it loads the one just chosen.
        if self.ai_chat_overlay_panes.contains_key(&pane.pane_id()) {
            self.cancel_overlay_for_pane(pane.pane_id());
        }
        crate::termwindow::ai_chat::toggle_overlay(self, &pane);
        self.left_panel_repaint();
    }

    /// Returns keyboard focus to the terminal after a panel action that
    /// typed into it.
    fn focus_terminal(&mut self) {
        if self.get_modal().is_some() {
            self.cancel_modal();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel caches glyph texture coordinates; an atlas rebuild (CJK
    /// text in the viewer can trigger one) must drop that cache too, or the
    /// panel draws garbled glyphs.
    #[test]
    fn atlas_rebuilds_drop_the_panel_layout() {
        let src = include_str!("../render/fancy_tab_bar.rs");
        let body = src
            .split("pub fn invalidate_fancy_tab_bar(&mut self) {")
            .nth(1)
            .and_then(|rest| rest.split("\n    }").next())
            .unwrap();
        assert!(body.contains("self.left_panel.invalidate()"));
    }

    #[test]
    fn cmd_shortcuts_map_to_the_viewer_ctrl_keys() {
        assert_eq!(viewer_cmd_key('s', false), Some("\x13"));
        assert_eq!(viewer_cmd_key('Z', true), Some("\x19"));
        assert_eq!(viewer_cmd_key('z', false), Some("\x1a"));
        assert_eq!(viewer_cmd_key('t', false), None);
        assert_eq!(viewer_cmd_key('s', true), None);
        assert_eq!(
            viewer_assignment_key(&KeyAssignment::SendStringIfNotAltScreen("\x1f".into())),
            Some("\x1a")
        );
        assert_eq!(viewer_assignment_key(&KeyAssignment::Nop), None);
    }

    #[test]
    fn saved_state_yields_to_a_changed_config() {
        let saved = SavedPanel {
            visible: true,
            width: 300.0,
            tool: 2,
            config_visible: false,
            config_width: 260.0,
        };
        assert_eq!(
            resolve_saved(Some(saved), false, 260.0),
            (true, 300.0, LeftPanelTool::Snippets)
        );
        assert!(
            resolve_saved(Some(saved), true, 260.0).0,
            "config flipped on"
        );
        let (visible, width, _) = resolve_saved(Some(saved), false, 400.0);
        assert!(visible);
        assert_eq!(width, 400.0);
        assert_eq!(
            resolve_saved(None, false, 10_000.0),
            (false, LEFT_PANEL_MAX_WIDTH, LeftPanelTool::Files)
        );
    }

    #[test]
    fn hidden_panel_takes_no_width() {
        assert_eq!(panel_pixel_width(false, 300.0, 144.0), 0);
        assert_eq!(panel_pixel_width(true, 300.0, 72.0), 300);
        assert_eq!(
            panel_pixel_width(true, 1.0, 72.0),
            LEFT_PANEL_MIN_WIDTH as usize
        );
    }

    #[test]
    fn truncation_counts_wide_characters() {
        assert_eq!(truncate_cells("hello", 5), "hello");
        assert_eq!(truncate_cells("hello world", 6), "hello…");
        assert_eq!(truncate_cells("中文标题很长", 7), "中文标…");
    }

    #[test]
    fn cd_lines_quote_their_path() {
        assert_eq!(cd_command(Path::new("/tmp/a b")), "cd '/tmp/a b'");
        assert_eq!(cd_command(Path::new("/tmp/plain")), "cd /tmp/plain");
    }

    #[test]
    fn viewer_requests_carry_path_and_line() {
        let req = viewer_open_request(Path::new("/p/README.md"), Some(12));
        assert_eq!(
            crate::file_view::parse_open_request(&req),
            Some((PathBuf::from("/p/README.md"), Some(12)))
        );
        let req = viewer_open_request(Path::new("/p/a:b.txt"), None);
        assert_eq!(
            crate::file_view::parse_open_request(&req),
            Some((PathBuf::from("/p/a:b.txt"), None))
        );
        assert_eq!(crate::file_view::parse_open_request("plain paste"), None);
    }

    #[test]
    fn file_icons_follow_the_extension() {
        assert_eq!(file_icon("README.md").0, "\u{eb1d}");
        assert_eq!(file_icon("main.rs").0, "\u{eae9}");
        assert_eq!(file_icon("Cargo.toml").0, "\u{eb0f}");
        assert_eq!(file_icon("LICENSE").0, "\u{ea7b}");
    }
}
