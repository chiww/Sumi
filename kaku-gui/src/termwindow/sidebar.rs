//! Tab sidebar: the window's tabs as a list at its left edge, an alternative
//! to the horizontal tab bar (`config.vertical_tabs`, View > Tab Sidebar).
//!
//! Layout: the sidebar's width is added to the left OS border in
//! `get_os_border`, so terminal sizing, pane placement, mouse coordinates and
//! overlays all start right of it without knowing it exists. The horizontal
//! tab bar is hidden while the sidebar shows (`should_show_tab_bar`).
//!
//! Each row shows the tab title (or an agent's topic), a second line chosen by
//! `vertical_tabs_subtitle`, a status dot and its Cmd+N shortcut. The dot
//! reads signals the terminal already receives; nothing is injected into the
//! agents (see the root AGENTS.md rule on hooks).

use crate::tabbar::{compute_tab_plain_title, foreground_agent_cli, foreground_process_title};
use crate::termwindow::box_model::*;
use crate::termwindow::render::corners::*;
use crate::termwindow::{UIItem, UIItemType};
use crate::utilsprites::RenderMetrics;
use config::{
    Dimension, DimensionContext, VerticalTabsSubtitle, VERTICAL_TABS_MAX_WIDTH,
    VERTICAL_TABS_MIN_WIDTH,
};
use mux::pane::CachePolicy;
use mux::Mux;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use termwiz::cell::unicode_column_width;
use wezterm_term::color::{ColorAttribute, ColorPalette, SrgbaTuple};
use wezterm_term::Progress;
use window::color::LinearRgba;
use window::{MouseCursor, MouseEvent, MouseEventKind as WMEK, MousePress, WindowOps};

/// Space kept above the first row for the macOS traffic lights.
const TRAFFIC_LIGHT_CLEARANCE_PT: f32 = 40.0;
const TOP_GAP_PT: f32 = 10.0;
const ROW_MARGIN_X_PT: f32 = 8.0;
const ROW_GAP_PT: f32 = 2.0;
const ROW_PAD_X_PT: f32 = 8.0;
const ROW_PAD_Y_PT: f32 = 6.0;
/// Hit width of the drag handle on the sidebar's right edge.
const RESIZE_HANDLE_PT: f32 = 6.0;
const STATE_FILE_NAME: &str = ".sumi_tab_sidebar.json";

/// What a sidebar region does under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarItem {
    /// Empty space below the rows.
    Background,
    /// The strip under the traffic lights; drags the window like a titlebar.
    Header,
    Tab {
        tab_idx: usize,
        active: bool,
    },
    NewTab,
    /// The right edge, dragged to change the width.
    Resize,
}

/// A tab's state as the dot shows it, most urgent first in `row_status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowStatus {
    None,
    Working,
    Done,
    NeedsInput,
    Error,
}

/// The per-pane signals a row's status is derived from.
#[derive(Clone, Debug, Default)]
pub(crate) struct PaneSignals {
    /// OSC 7501 state: "working", "blocked", "done", "error" or "idle".
    pub program_status: Option<String>,
    pub progress: Progress,
    pub unread_notification: bool,
    pub unread_bell: bool,
    /// An agent CLI is in the foreground and its title shows a spinner.
    pub agent_spinning: bool,
}

/// Folds every pane of a tab into one status. A pane that wants the user
/// outranks one that is merely busy, so a tab with one blocked agent and one
/// working agent reads as blocked.
pub(crate) fn row_status(panes: &[PaneSignals]) -> RowStatus {
    let mut status = RowStatus::None;
    for pane in panes {
        let program = pane.program_status.as_deref();
        let this = if program == Some("error") || matches!(pane.progress, Progress::Error(_)) {
            RowStatus::Error
        } else if program == Some("blocked")
            || matches!(pane.progress, Progress::Paused)
            || pane.unread_bell
            || (pane.unread_notification && program != Some("done"))
        {
            RowStatus::NeedsInput
        } else if pane.unread_notification && program == Some("done") {
            RowStatus::Done
        } else if program == Some("working")
            || matches!(
                pane.progress,
                Progress::Indeterminate | Progress::Percentage(_)
            )
            || pane.agent_spinning
        {
            RowStatus::Working
        } else {
            RowStatus::None
        };
        status = status.max_by_urgency(this);
    }
    status
}

impl RowStatus {
    fn urgency(self) -> u8 {
        match self {
            RowStatus::None => 0,
            RowStatus::Working => 1,
            RowStatus::Done => 2,
            RowStatus::NeedsInput => 3,
            RowStatus::Error => 4,
        }
    }

    fn max_by_urgency(self, other: RowStatus) -> RowStatus {
        if other.urgency() > self.urgency() {
            other
        } else {
            self
        }
    }

    /// The ANSI palette slot the dot takes its color from.
    fn palette_index(self) -> Option<u8> {
        match self {
            RowStatus::None => None,
            RowStatus::Working => Some(4),
            RowStatus::Done => Some(2),
            RowStatus::NeedsInput => Some(3),
            RowStatus::Error => Some(1),
        }
    }
}

/// Whether an agent's pane title starts with a busy spinner: Claude Code's
/// half-filled circles or a braille spinner (Claude Code, Codex). Claude's
/// idle mark ✳ does not count.
pub(crate) fn title_shows_spinner(title: &str) -> bool {
    match title.trim_start().chars().next() {
        Some(c) => matches!(c, '\u{2801}'..='\u{28ff}' | '\u{25d0}'..='\u{25d3}'),
        None => false,
    }
}

/// Everything a row draws. Rows are rebuilt from this, so an unchanged model
/// skips the relayout.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SidebarRow {
    pub tab_idx: usize,
    pub active: bool,
    pub title: String,
    pub subtitle: Option<String>,
    pub status: RowStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
struct SavedSidebar {
    visible: bool,
    width: f32,
    /// The config values when this was saved; a different value in the
    /// config now means the user changed it there, which wins.
    config_visible: bool,
    config_width: f32,
}

/// Startup visibility and width: the last toggle and drag, unless the config
/// changed since they were saved.
fn resolve_saved(
    saved: Option<SavedSidebar>,
    config_visible: bool,
    config_width: f32,
) -> (bool, f32) {
    let config_width = clamp_width(config_width);
    match saved {
        None => (config_visible, config_width),
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
        ),
    }
}

pub(crate) fn clamp_width(width: f32) -> f32 {
    if width.is_finite() {
        width.clamp(VERTICAL_TABS_MIN_WIDTH, VERTICAL_TABS_MAX_WIDTH)
    } else {
        VERTICAL_TABS_MIN_WIDTH
    }
}

fn state_file() -> PathBuf {
    config::CONFIG_DIRS
        .first()
        .cloned()
        .unwrap_or_else(|| config::HOME_DIR.join(".config").join("sumi"))
        .join(STATE_FILE_NAME)
}

fn load_saved() -> Option<SavedSidebar> {
    let text = std::fs::read_to_string(state_file()).ok()?;
    serde_json::from_str(&text).ok()
}

fn save(saved: &SavedSidebar) {
    let path = state_file();
    let result = (|| -> anyhow::Result<()> {
        let text = serde_json::to_string(saved)?;
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })();
    if let Err(err) = result {
        log::warn!("saving tab sidebar state to {}: {err:#}", path.display());
    }
}

/// Per-window sidebar state.
pub(crate) struct SidebarState {
    pub visible: bool,
    pub width_pt: f32,
    /// First row drawn, moved by the wheel when rows overflow.
    pub scroll: usize,
    pub rows: Vec<SidebarRow>,
    element: Option<Vec<ComputedElement>>,
}

impl SidebarState {
    pub fn new(config: &config::ConfigHandle) -> Self {
        let (visible, width_pt) = resolve_saved(
            load_saved(),
            config.vertical_tabs,
            config.vertical_tabs_width,
        );
        Self {
            visible,
            width_pt,
            scroll: 0,
            rows: vec![],
            element: None,
        }
    }

    pub fn invalidate(&mut self) {
        self.element.take();
    }

    fn persist(&self, config: &config::ConfigHandle) {
        save(&SavedSidebar {
            visible: self.visible,
            width: self.width_pt,
            config_visible: config.vertical_tabs,
            config_width: clamp_width(config.vertical_tabs_width),
        });
    }

    /// Applies a reloaded config. Returns true when the layout changes.
    /// `hotkey_window` keeps the global-hotkey drawer without a sidebar.
    pub fn config_reloaded(
        &mut self,
        old: &config::ConfigHandle,
        new: &config::ConfigHandle,
        hotkey_window: bool,
    ) -> bool {
        if hotkey_window {
            return false;
        }
        let mut changed = false;
        if old.vertical_tabs != new.vertical_tabs {
            self.visible = new.vertical_tabs;
            changed = true;
        }
        if (old.vertical_tabs_width - new.vertical_tabs_width).abs() > 0.01 {
            self.width_pt = clamp_width(new.vertical_tabs_width);
            changed = true;
        }
        self.invalidate();
        if changed {
            self.persist(new);
        }
        changed
    }
}

fn points_to_pixels(points: f32, dpi: f32) -> f32 {
    Dimension::Points(points).evaluate_as_pixels(DimensionContext {
        dpi,
        pixel_max: 0.0,
        pixel_cell: 0.0,
    })
}

/// The pixel width the sidebar takes from the window, zero while hidden.
pub(crate) fn sidebar_pixel_width(visible: bool, width_pt: f32, dpi: f32) -> usize {
    if visible {
        points_to_pixels(clamp_width(width_pt), dpi).round() as usize
    } else {
        0
    }
}

// ---------- Git branch lookup ----------

const BRANCH_TTL: Duration = Duration::from_secs(3);

lazy_static::lazy_static! {
    static ref BRANCH_CACHE: Mutex<HashMap<PathBuf, (Instant, Option<String>)>> =
        Mutex::new(HashMap::new());
}

/// The branch checked out in the repository containing `dir`, or the short
/// commit id when HEAD is detached. Reads `.git/HEAD` directly (worktrees and
/// submodules point there through a `gitdir:` file) instead of running git.
pub(crate) fn git_branch_for(dir: &Path) -> Option<String> {
    let mut cache = BRANCH_CACHE.lock().ok()?;
    if let Some((at, branch)) = cache.get(dir) {
        if at.elapsed() < BRANCH_TTL {
            return branch.clone();
        }
    }
    let branch = read_git_branch(dir);
    if cache.len() > 256 {
        cache.clear();
    }
    cache.insert(dir.to_path_buf(), (Instant::now(), branch.clone()));
    branch
}

fn read_git_branch(dir: &Path) -> Option<String> {
    for ancestor in dir.ancestors() {
        let dot_git = ancestor.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let target = Path::new(target);
            if target.is_absolute() {
                target.to_path_buf()
            } else {
                ancestor.join(target)
            }
        } else {
            continue;
        };
        let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
        return branch_from_head(&head);
    }
    None
}

pub(crate) fn branch_from_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        return Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        );
    }
    if head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(head[..7].to_string());
    }
    None
}

/// The Cmd+digit that selects tab `idx`: Cmd+1..8 the first eight tabs and
/// Cmd+9 always the last one.
pub(crate) fn tab_shortcut(idx: usize, tab_count: usize) -> Option<String> {
    if idx < 8 {
        Some(format!("⌘{}", idx + 1))
    } else if idx + 1 == tab_count {
        Some("⌘9".to_string())
    } else {
        None
    }
}

/// `~/x/y` for paths under the home directory.
fn display_path(path: &str) -> String {
    display_path_with_home(path, &config::HOME_DIR.to_string_lossy())
}

fn display_path_with_home(path: &str, home: &str) -> String {
    // OSC 7 directories often end in a slash.
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };
    let home = home.trim_end_matches('/');
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

pub(crate) fn compose_subtitle(
    mode: VerticalTabsSubtitle,
    agent: Option<&str>,
    directory: Option<&str>,
    branch: Option<&str>,
    command: Option<&str>,
) -> Option<String> {
    let mut parts: Vec<&str> = vec![];
    if let Some(agent) = agent {
        parts.push(agent);
    }
    match mode {
        VerticalTabsSubtitle::None => return None,
        VerticalTabsSubtitle::DirectoryBranch => {
            parts.extend(directory);
            parts.extend(branch);
        }
        VerticalTabsSubtitle::Directory => parts.extend(directory),
        VerticalTabsSubtitle::Branch => parts.extend(branch.or(directory)),
        VerticalTabsSubtitle::Command => {
            if agent.is_none() {
                parts.extend(command);
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// Cuts `text` to roughly `max_cells` terminal columns, ending in an
/// ellipsis. The box model also clips at the row edge; this keeps the cut
/// from landing mid-glyph without one.
pub(crate) fn truncate_cells(text: &str, max_cells: usize) -> String {
    if unicode_column_width(text, None) <= max_cells {
        return text.to_string();
    }
    let budget = max_cells.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_column_width(ch.encode_utf8(&mut [0; 4]), None);
        if used + w > budget {
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

impl crate::TermWindow {
    pub(crate) fn sidebar_pixel_width(&self) -> usize {
        sidebar_pixel_width(
            self.sidebar.visible,
            self.sidebar.width_pt,
            self.dimensions.dpi as f32,
        )
    }

    pub(super) fn toggle_vertical_tabs(&mut self) {
        if self
            .window_state
            .contains(::window::WindowState::HOTKEY_WINDOW)
        {
            return;
        }
        self.sidebar.visible = !self.sidebar.visible;
        self.sidebar.persist(&self.config);
        self.sidebar_layout_changed();
    }

    /// Relayout after the sidebar appeared, disappeared or changed width.
    pub(super) fn sidebar_layout_changed(&mut self) {
        self.sidebar.invalidate();
        self.sync_tab_bar_visibility_for_window_state("tab-sidebar");
        self.invalidate_fancy_tab_bar();
        if let Some(window) = self.window.clone() {
            self.apply_dimensions(&self.dimensions.clone(), None, &window, false);
            self.update_title();
            window.invalidate();
        }
    }

    /// Recomputes the row model; called from `update_title_impl`.
    pub(super) fn refresh_sidebar_rows(&mut self, tab_infos: &[crate::termwindow::TabInformation]) {
        if !self.sidebar.visible {
            return;
        }
        let rows = self.compute_sidebar_rows(tab_infos);
        if rows != self.sidebar.rows {
            self.sidebar.rows = rows;
            self.sidebar.invalidate();
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    fn compute_sidebar_rows(
        &self,
        tab_infos: &[crate::termwindow::TabInformation],
    ) -> Vec<SidebarRow> {
        let mux = Mux::get();
        let Some(window) = mux.get_window(self.mux_window_id) else {
            return vec![];
        };
        let active_idx = window.get_active_idx();
        let tabs: Vec<_> = window.iter().cloned().collect();
        drop(window);

        let mode = self.config.vertical_tabs_subtitle;
        let pane_state = self.pane_state.borrow();
        tabs.iter()
            .enumerate()
            .map(|(idx, tab)| {
                let panes = tab.iter_panes_ignoring_zoom();
                let signals: Vec<PaneSignals> = panes
                    .iter()
                    .map(|pos| {
                        let pane = &pos.pane;
                        let state = pane_state.get(&pane.pane_id());
                        PaneSignals {
                            program_status: pane.get_program_status(),
                            progress: pane.get_progress(),
                            unread_notification: state
                                .is_some_and(|state| state.has_unread_notification),
                            unread_bell: state.is_some_and(|state| state.has_unread_bell),
                            agent_spinning: foreground_agent_cli(&**pane).is_some()
                                && title_shows_spinner(&pane.get_title()),
                        }
                    })
                    .collect();

                let active_pane = tab.get_active_pane();
                let agent = active_pane
                    .as_ref()
                    .and_then(|pane| foreground_agent_cli(&**pane).map(|p| (p, pane)));
                let title = match &agent {
                    Some((program, pane)) if tab.get_title().is_empty() => {
                        crate::termwindow::render::agent_title::agent_topic(
                            program,
                            &pane.get_title(),
                        )
                    }
                    _ => tab_infos
                        .get(idx)
                        .map(compute_tab_plain_title)
                        .unwrap_or_default(),
                };
                let cwd = active_pane
                    .as_ref()
                    .and_then(|pane| pane.get_current_working_dir(CachePolicy::AllowStale))
                    .filter(|url| url.scheme() == "file")
                    .map(|url| url.path().to_string());
                let needs_branch = matches!(
                    mode,
                    VerticalTabsSubtitle::DirectoryBranch | VerticalTabsSubtitle::Branch
                );
                let branch = cwd
                    .as_deref()
                    .filter(|_| needs_branch)
                    .and_then(|dir| git_branch_for(Path::new(dir)));
                let directory = cwd.as_deref().map(display_path);
                let command = active_pane
                    .as_ref()
                    .and_then(|pane| foreground_process_title(&**pane));
                let agent_label = agent.as_ref().map(|(program, _)| {
                    crate::termwindow::render::agent_title::agent_label(program)
                });
                SidebarRow {
                    tab_idx: idx,
                    active: idx == active_idx,
                    title,
                    subtitle: compose_subtitle(
                        mode,
                        agent_label,
                        directory.as_deref(),
                        branch.as_deref(),
                        command.as_deref(),
                    ),
                    status: row_status(&signals),
                }
            })
            .collect()
    }

    fn sidebar_header_height(&self) -> f32 {
        let dpi = self.dimensions.dpi as f32;
        let has_traffic_lights = self
            .config
            .window_decorations
            .contains(::window::WindowDecorations::INTEGRATED_BUTTONS)
            && !self.layout_is_effective_fullscreen();
        points_to_pixels(
            if has_traffic_lights {
                TRAFFIC_LIGHT_CLEARANCE_PT
            } else {
                TOP_GAP_PT
            },
            dpi,
        )
    }

    fn sidebar_colors(&self, palette: &ColorPalette) -> SidebarColors {
        let bg = palette.background;
        let fg = palette.foreground;
        let opacity = self.config.window_background_opacity;
        let to = |c: SrgbaTuple| c.to_linear();
        SidebarColors {
            background: to(mix(bg, fg, 0.035)).mul_alpha(opacity),
            divider: to(mix(bg, fg, 0.12)),
            active: to(mix(bg, fg, 0.13)),
            hover: to(mix(bg, fg, 0.07)),
            title: to(fg),
            dim: to(mix(bg, fg, 0.55)),
            status: |palette: &ColorPalette, status: RowStatus| {
                status.palette_index().map(|idx| {
                    palette
                        .resolve_fg(ColorAttribute::PaletteIndex(idx))
                        .to_linear()
                })
            },
        }
    }

    fn build_sidebar(&self, palette: &ColorPalette) -> anyhow::Result<Vec<ComputedElement>> {
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let dpi = self.dimensions.dpi as f32;
        let px = |pt: f32| points_to_pixels(pt, dpi);
        let width = self.sidebar_pixel_width() as f32;
        let height = self.dimensions.pixel_height as f32;
        let colors = self.sidebar_colors(palette);
        let cell_w = metrics.cell_size.width.max(1) as f32;
        let line_h = metrics.cell_size.height as f32;

        let row_x = px(ROW_MARGIN_X_PT);
        let row_w = (width - row_x * 2.0).max(1.0);
        let text_cells = ((row_w - px(ROW_PAD_X_PT) * 2.0) / cell_w).floor().max(4.0) as usize;
        let dot_cells = 2;
        let shortcut_cells = 3;

        let layout = |bounds: euclid::Rect<f32, window::PixelUnit>,
                      element: &Element|
         -> anyhow::Result<ComputedElement> {
            self.compute_element(
                &LayoutContext {
                    height: DimensionContext {
                        dpi,
                        pixel_max: height,
                        pixel_cell: line_h,
                    },
                    width: DimensionContext {
                        dpi,
                        pixel_max: self.dimensions.pixel_width as f32,
                        pixel_cell: cell_w,
                    },
                    bounds,
                    metrics: &metrics,
                    gl_state: self.render_state.as_ref().unwrap(),
                    zindex: 10,
                },
                element,
            )
        };
        let rounded = || {
            let r = Dimension::Pixels(px(6.0));
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

        let mut out = vec![];
        let header_h = self.sidebar_header_height();
        let new_tab_h = line_h + px(ROW_PAD_Y_PT) * 2.0;
        let list_bottom = height - new_tab_h - px(TOP_GAP_PT);
        let mut y = header_h;
        let has_subtitles = self.config.vertical_tabs_subtitle != VerticalTabsSubtitle::None;
        let row_h = line_h * if has_subtitles { 2.0 } else { 1.0 } + px(ROW_PAD_Y_PT) * 2.0;

        for row in self.sidebar.rows.iter().skip(self.sidebar.scroll) {
            if y + row_h > list_bottom {
                break;
            }
            let shortcut = tab_shortcut(row.tab_idx, self.sidebar.rows.len());
            let title_budget =
                text_cells
                    .saturating_sub(dot_cells)
                    .saturating_sub(if shortcut.is_some() {
                        shortcut_cells
                    } else {
                        0
                    });
            let dot_color = (colors.status)(palette, row.status);
            let dot = text(
                if dot_color.is_some() { "●" } else { " " }.to_string(),
                dot_color.unwrap_or(colors.dim),
            )
            .min_width(Some(Dimension::Pixels(cell_w * dot_cells as f32)));
            let mut first_line = vec![
                dot,
                text(truncate_cells(&row.title, title_budget), colors.title),
            ];
            if let Some(shortcut) = shortcut {
                first_line.push(text(shortcut, colors.dim).float(Float::Right));
            }
            let mut lines = vec![Element::new(&font, ElementContent::Children(first_line))
                .display(DisplayType::Block)
                .min_width(Some(Dimension::Pixels(row_w - px(ROW_PAD_X_PT) * 2.0)))];
            if has_subtitles {
                let subtitle = row.subtitle.clone().unwrap_or_default();
                lines.push(
                    Element::new(
                        &font,
                        ElementContent::Children(vec![
                            text(" ".repeat(dot_cells), colors.dim),
                            text(
                                truncate_cells(&subtitle, text_cells.saturating_sub(dot_cells)),
                                colors.dim,
                            ),
                        ]),
                    )
                    .display(DisplayType::Block),
                );
            }
            let bg = if row.active {
                colors.active
            } else {
                LinearRgba::TRANSPARENT
            };
            let element = Element::new(&font, ElementContent::Children(lines))
                .item_type(UIItemType::Sidebar(SidebarItem::Tab {
                    tab_idx: row.tab_idx,
                    active: row.active,
                }))
                .display(DisplayType::Block)
                // min sizes are content-box; padding is added around them.
                .min_width(Some(Dimension::Pixels(row_w - px(ROW_PAD_X_PT) * 2.0)))
                .min_height(Some(Dimension::Pixels(row_h - px(ROW_PAD_Y_PT) * 2.0)))
                .padding(BoxDimension {
                    left: Dimension::Pixels(px(ROW_PAD_X_PT)),
                    right: Dimension::Pixels(px(ROW_PAD_X_PT)),
                    top: Dimension::Pixels(px(ROW_PAD_Y_PT)),
                    bottom: Dimension::Pixels(px(ROW_PAD_Y_PT)),
                })
                .border_corners(rounded())
                .colors(ElementColors {
                    border: BorderColor::new(bg),
                    bg: bg.into(),
                    text: colors.title.into(),
                })
                .hover_colors((!row.active).then(|| ElementColors {
                    border: BorderColor::new(colors.hover),
                    bg: colors.hover.into(),
                    text: colors.title.into(),
                }));
            out.push(layout(euclid::rect(row_x, y, row_w, row_h), &element)?);
            y += row_h + px(ROW_GAP_PT);
        }

        let new_tab = Element::new(
            &font,
            ElementContent::Children(vec![
                text("+".to_string(), colors.dim)
                    .min_width(Some(Dimension::Pixels(cell_w * dot_cells as f32))),
                text(rust_i18n::t!("sidebar.new_tab").into_owned(), colors.dim),
            ]),
        )
        .item_type(UIItemType::Sidebar(SidebarItem::NewTab))
        .min_width(Some(Dimension::Pixels(row_w - px(ROW_PAD_X_PT) * 2.0)))
        .min_height(Some(Dimension::Pixels(new_tab_h - px(ROW_PAD_Y_PT) * 2.0)))
        .padding(BoxDimension {
            left: Dimension::Pixels(px(ROW_PAD_X_PT)),
            right: Dimension::Pixels(px(ROW_PAD_X_PT)),
            top: Dimension::Pixels(px(ROW_PAD_Y_PT)),
            bottom: Dimension::Pixels(px(ROW_PAD_Y_PT)),
        })
        .border_corners(rounded())
        .colors(ElementColors {
            border: BorderColor::new(LinearRgba::TRANSPARENT),
            bg: LinearRgba::TRANSPARENT.into(),
            text: colors.dim.into(),
        })
        .hover_colors(Some(ElementColors {
            border: BorderColor::new(colors.hover),
            bg: colors.hover.into(),
            text: colors.title.into(),
        }));
        out.push(layout(
            euclid::rect(
                row_x,
                height - new_tab_h - px(ROW_GAP_PT * 3.0),
                row_w,
                new_tab_h,
            ),
            &new_tab,
        )?);
        Ok(out)
    }

    /// Paints the sidebar and registers its hit regions. Rows are drawn over
    /// the window's left border region that `get_os_border` reserved.
    pub(super) fn paint_sidebar(
        &mut self,
        layers: &mut crate::quad::TripleLayerQuadAllocator,
    ) -> anyhow::Result<()> {
        let width = self.sidebar_pixel_width();
        if width == 0 {
            return Ok(());
        }
        let width = width as f32;
        let height = self.dimensions.pixel_height as f32;
        let palette = self.palette().clone();
        let colors = self.sidebar_colors(&palette);
        let dpi = self.dimensions.dpi as f32;

        self.filled_rectangle(
            layers,
            1,
            euclid::rect(0.0, 0.0, width, height),
            colors.background,
        )?;
        let divider = points_to_pixels(1.0, dpi).max(1.0).round();
        self.filled_rectangle(
            layers,
            2,
            euclid::rect(width - divider, 0.0, divider, height),
            colors.divider,
        )?;

        if self.sidebar.element.is_none() {
            let built = self.build_sidebar(&palette)?;
            self.sidebar.element.replace(built);
        }

        let header_h = self.sidebar_header_height();
        let handle = points_to_pixels(RESIZE_HANDLE_PT, dpi);
        let mut items = vec![
            UIItem {
                x: 0,
                y: 0,
                width: width as usize,
                height: height as usize,
                item_type: UIItemType::Sidebar(SidebarItem::Background),
            },
            UIItem {
                x: 0,
                y: 0,
                width: width as usize,
                height: header_h as usize,
                item_type: UIItemType::Sidebar(SidebarItem::Header),
            },
        ];
        let elements = self.sidebar.element.take().unwrap_or_default();
        let gl_state = self.render_state.as_ref().unwrap();
        let mut result = Ok(());
        for element in &elements {
            items.extend(element.ui_items());
            if let Err(err) = self.render_element(element, gl_state, None) {
                result = Err(err);
                break;
            }
        }
        self.sidebar.element.replace(elements);
        items.push(UIItem {
            x: (width - handle / 2.0).max(0.0) as usize,
            y: 0,
            width: handle as usize,
            height: height as usize,
            item_type: UIItemType::Sidebar(SidebarItem::Resize),
        });
        self.ui_items.extend(items);
        result
    }

    pub(super) fn mouse_event_sidebar(
        &mut self,
        item: SidebarItem,
        ui_item: UIItem,
        event: MouseEvent,
        context: &dyn WindowOps,
    ) {
        let mut cursor = MouseCursor::Arrow;
        match (item, &event.kind) {
            (SidebarItem::Resize, kind) => {
                cursor = MouseCursor::SizeLeftRight;
                if *kind == WMEK::Press(MousePress::Left) {
                    self.dragging.replace((ui_item, event.clone()));
                }
            }
            (SidebarItem::Header, _) => {
                self.mouse_event_tab_bar(
                    crate::tabbar::TabBarItem::None,
                    ui_item,
                    event.clone(),
                    context,
                );
                return;
            }
            (SidebarItem::Tab { tab_idx, active }, WMEK::Press(MousePress::Left)) => {
                if event.platform_click_count == 2 {
                    if let Err(err) = self.begin_tab_rename(tab_idx, ui_item) {
                        log::debug!("begin_tab_rename({tab_idx}) failed: {err:#}");
                    }
                } else if !active {
                    if let Err(err) = self.activate_tab(tab_idx as isize) {
                        log::debug!("activate_tab({tab_idx}) failed: {err:#}");
                    }
                }
            }
            (SidebarItem::Tab { tab_idx, .. }, WMEK::Press(MousePress::Middle)) => {
                self.close_specific_tab(tab_idx, true);
            }
            (SidebarItem::Tab { tab_idx, .. }, WMEK::Press(MousePress::Right)) => {
                self.show_tab_context_menu(tab_idx, &event, context);
            }
            (SidebarItem::NewTab, WMEK::Press(MousePress::Left)) => {
                self.do_new_tab_button_click(MousePress::Left);
            }
            (_, WMEK::VertWheel(n)) => {
                self.scroll_sidebar(if *n > 0 { -1 } else { 1 });
            }
            _ => {}
        }
        context.set_cursor(Some(cursor));
    }

    fn scroll_sidebar(&mut self, delta: isize) {
        let max = self.sidebar.rows.len().saturating_sub(1);
        let next = (self.sidebar.scroll as isize + delta).clamp(0, max as isize) as usize;
        if next != self.sidebar.scroll {
            self.sidebar.scroll = next;
            self.sidebar.invalidate();
            if let Some(window) = self.window.as_ref() {
                window.invalidate();
            }
        }
    }

    /// Follows a drag of the sidebar's right edge.
    pub(super) fn drag_sidebar_resize(
        &mut self,
        item: UIItem,
        start_event: MouseEvent,
        event: MouseEvent,
    ) {
        let dpi = self.dimensions.dpi as f32;
        let one_point = points_to_pixels(1.0, dpi).max(f32::EPSILON);
        let width = clamp_width(event.coords.x.max(0) as f32 / one_point);
        if (width - self.sidebar.width_pt).abs() >= 1.0 {
            self.sidebar.width_pt = width.round();
            self.sidebar_layout_changed();
        }
        self.dragging.replace((item, start_event));
    }

    pub(super) fn finish_sidebar_resize(&mut self) {
        self.sidebar.persist(&self.config);
        if let Some(window) = self.window.as_ref() {
            window.invalidate();
        }
    }
}

struct SidebarColors {
    background: LinearRgba,
    divider: LinearRgba,
    active: LinearRgba,
    hover: LinearRgba,
    title: LinearRgba,
    dim: LinearRgba,
    status: fn(&ColorPalette, RowStatus) -> Option<LinearRgba>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> PaneSignals {
        PaneSignals::default()
    }

    #[test]
    fn idle_panes_show_no_status() {
        assert_eq!(row_status(&[signals(), signals()]), RowStatus::None);
        assert_eq!(row_status(&[]), RowStatus::None);
    }

    #[test]
    fn program_status_maps_to_each_dot() {
        let with = |s: &str| PaneSignals {
            program_status: Some(s.to_string()),
            ..signals()
        };
        assert_eq!(row_status(&[with("working")]), RowStatus::Working);
        assert_eq!(row_status(&[with("blocked")]), RowStatus::NeedsInput);
        assert_eq!(row_status(&[with("error")]), RowStatus::Error);
        assert_eq!(row_status(&[with("idle")]), RowStatus::None);
        // A finished program stays quiet once its notification was seen.
        assert_eq!(row_status(&[with("done")]), RowStatus::None);
        let unseen_done = PaneSignals {
            unread_notification: true,
            ..with("done")
        };
        assert_eq!(row_status(&[unseen_done]), RowStatus::Done);
    }

    #[test]
    fn progress_and_notifications_drive_status_without_program_status() {
        let progress = |p: Progress| PaneSignals {
            progress: p,
            ..signals()
        };
        assert_eq!(
            row_status(&[progress(Progress::Indeterminate)]),
            RowStatus::Working
        );
        assert_eq!(
            row_status(&[progress(Progress::Percentage(40))]),
            RowStatus::Working
        );
        assert_eq!(
            row_status(&[progress(Progress::Paused)]),
            RowStatus::NeedsInput
        );
        assert_eq!(
            row_status(&[progress(Progress::Error(1))]),
            RowStatus::Error
        );
        let notified = PaneSignals {
            unread_notification: true,
            ..signals()
        };
        assert_eq!(row_status(&[notified]), RowStatus::NeedsInput);
        let bell = PaneSignals {
            unread_bell: true,
            ..signals()
        };
        assert_eq!(row_status(&[bell]), RowStatus::NeedsInput);
        let spinning = PaneSignals {
            agent_spinning: true,
            ..signals()
        };
        assert_eq!(row_status(&[spinning]), RowStatus::Working);
    }

    #[test]
    fn the_most_urgent_pane_wins() {
        let working = PaneSignals {
            program_status: Some("working".into()),
            ..signals()
        };
        let blocked = PaneSignals {
            program_status: Some("blocked".into()),
            ..signals()
        };
        let error = PaneSignals {
            program_status: Some("error".into()),
            ..signals()
        };
        assert_eq!(
            row_status(&[working.clone(), blocked.clone()]),
            RowStatus::NeedsInput
        );
        assert_eq!(row_status(&[blocked, error, working]), RowStatus::Error);
    }

    #[test]
    fn spinner_titles_are_recognized() {
        assert!(title_shows_spinner("⠋ Fix the build"));
        assert!(title_shows_spinner("◐ Refactor"));
        assert!(!title_shows_spinner("✳ Fix the build"));
        assert!(!title_shows_spinner("Fix the build"));
        assert!(!title_shows_spinner(""));
        // U+2800 is the blank braille cell, not a spinner frame.
        assert!(!title_shows_spinner("\u{2800} idle"));
    }

    #[test]
    fn head_files_resolve_to_branch_or_short_commit() {
        assert_eq!(
            branch_from_head("ref: refs/heads/main\n").as_deref(),
            Some("main")
        );
        assert_eq!(
            branch_from_head("ref: refs/heads/feat/x").as_deref(),
            Some("feat/x")
        );
        assert_eq!(
            branch_from_head("0123456789abcdef0123456789abcdef01234567").as_deref(),
            Some("0123456")
        );
        assert_eq!(branch_from_head("garbage"), None);
    }

    #[test]
    fn git_branch_follows_worktree_gitdir_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let nested = repo.join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(read_git_branch(&nested).as_deref(), Some("main"));

        let worktree = tmp.path().join("wt");
        let wt_git = repo.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", wt_git.display()),
        )
        .unwrap();
        assert_eq!(read_git_branch(&worktree).as_deref(), Some("topic"));

        assert_eq!(read_git_branch(&tmp.path().join("elsewhere")), None);
    }

    #[test]
    fn subtitle_modes_pick_their_parts() {
        let d = Some("~/dev/sumi");
        let b = Some("main");
        let c = Some("vim");
        use VerticalTabsSubtitle as M;
        assert_eq!(
            compose_subtitle(M::DirectoryBranch, None, d, b, c).as_deref(),
            Some("~/dev/sumi · main")
        );
        assert_eq!(
            compose_subtitle(M::DirectoryBranch, None, d, None, c).as_deref(),
            Some("~/dev/sumi")
        );
        assert_eq!(
            compose_subtitle(M::Directory, None, d, b, c).as_deref(),
            Some("~/dev/sumi")
        );
        assert_eq!(
            compose_subtitle(M::Branch, None, d, b, c).as_deref(),
            Some("main")
        );
        // Outside a repository the branch mode still says where the tab is.
        assert_eq!(
            compose_subtitle(M::Branch, None, d, None, c).as_deref(),
            Some("~/dev/sumi")
        );
        assert_eq!(
            compose_subtitle(M::Command, None, d, b, c).as_deref(),
            Some("vim")
        );
        assert_eq!(compose_subtitle(M::None, None, d, b, c), None);
        assert_eq!(
            compose_subtitle(M::DirectoryBranch, Some("Claude Code"), d, b, c).as_deref(),
            Some("Claude Code · ~/dev/sumi · main")
        );
        assert_eq!(
            compose_subtitle(M::Command, Some("Codex"), d, b, Some("codex")).as_deref(),
            Some("Codex")
        );
        assert_eq!(compose_subtitle(M::Directory, None, None, None, None), None);
    }

    #[test]
    fn truncation_counts_wide_characters() {
        assert_eq!(truncate_cells("short", 10), "short");
        assert_eq!(truncate_cells("abcdefghij", 5), "abcd…");
        // Each CJK character takes two cells.
        assert_eq!(truncate_cells("中文标签页名称", 6), "中文…");
        assert_eq!(truncate_cells("abc", 0), "…");
    }

    #[test]
    fn saved_state_yields_to_a_changed_config() {
        let saved = SavedSidebar {
            visible: false,
            width: 300.0,
            config_visible: true,
            config_width: 220.0,
        };
        // Same config as when saved: the toggle and the drag are kept.
        assert_eq!(resolve_saved(Some(saved), true, 220.0), (false, 300.0));
        // The config was switched off and back on, or edited: it wins.
        assert_eq!(resolve_saved(Some(saved), false, 220.0), (false, 300.0));
        assert_eq!(resolve_saved(Some(saved), true, 260.0), (false, 260.0));
        let saved_hidden_config = SavedSidebar {
            config_visible: false,
            ..saved
        };
        assert_eq!(
            resolve_saved(Some(saved_hidden_config), true, 220.0),
            (true, 300.0)
        );
        assert_eq!(resolve_saved(None, true, 220.0), (true, 220.0));
        // Widths outside the supported range are clamped.
        assert_eq!(resolve_saved(None, false, 10.0).1, VERTICAL_TABS_MIN_WIDTH);
        assert_eq!(
            resolve_saved(None, false, 9000.0).1,
            VERTICAL_TABS_MAX_WIDTH
        );
    }

    #[test]
    fn paths_under_home_use_a_tilde() {
        let home = "/Users/me";
        assert_eq!(display_path_with_home("/Users/me", home), "~");
        assert_eq!(display_path_with_home("/Users/me/", home), "~");
        assert_eq!(display_path_with_home("/Users/me/dev/x/", home), "~/dev/x");
        assert_eq!(display_path_with_home("/Users/meow", home), "/Users/meow");
        assert_eq!(display_path_with_home("/", home), "/");
        assert_eq!(display_path_with_home("/tmp/", home), "/tmp");
    }

    #[test]
    fn shortcuts_match_cmd_digit_selection() {
        assert_eq!(tab_shortcut(0, 3).as_deref(), Some("⌘1"));
        assert_eq!(tab_shortcut(7, 12).as_deref(), Some("⌘8"));
        assert_eq!(tab_shortcut(8, 12), None);
        assert_eq!(tab_shortcut(11, 12).as_deref(), Some("⌘9"));
        assert_eq!(tab_shortcut(8, 9).as_deref(), Some("⌘9"));
    }

    #[test]
    fn hidden_sidebar_takes_no_width() {
        assert_eq!(sidebar_pixel_width(false, 220.0, 144.0), 0);
        assert_eq!(sidebar_pixel_width(true, 220.0, 72.0), 220);
        assert_eq!(sidebar_pixel_width(true, 220.0, 144.0), 440);
    }
}
