//! Color themes for the file viewer. `Auto` follows the terminal's own
//! palette; the others paint their own background and pick a matching
//! syntax theme for code. `markdown_theme` in the config picks the one a
//! viewer opens with and Ctrl+T cycles through them.

use std::cell::Cell;
use termwiz::cell::CellAttributes;
use termwiz::color::{ColorAttribute, SrgbaTuple};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    /// An index into the terminal's palette.
    Palette(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    fn attribute(self) -> ColorAttribute {
        match self {
            Color::Palette(idx) => ColorAttribute::PaletteIndex(idx),
            Color::Rgb(r, g, b) => ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
                1.0,
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Theme {
    pub name: &'static str,
    /// None keeps the terminal's own background and text colors.
    pub background: Option<Color>,
    pub text: Option<Color>,
    /// Headings, list markers and prompts.
    pub accent: Color,
    /// Inline code and unhighlighted code blocks.
    pub code: Color,
    /// Rules, gutters and quote bars.
    pub muted: Color,
    /// Status messages and the unsaved mark.
    pub warn: Color,
    pub added: Color,
    pub removed: Color,
    pub hunk: Color,
    /// The syntect theme for code, from syntect's bundled set.
    pub syntax: &'static str,
    /// For `Auto`, the syntax theme on a light terminal.
    pub syntax_light: &'static str,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub(crate) const THEMES: &[Theme] = &[
    Theme {
        name: "Auto",
        background: None,
        text: None,
        accent: Color::Palette(4),
        code: Color::Palette(3),
        muted: Color::Palette(8),
        warn: Color::Palette(3),
        added: Color::Palette(2),
        removed: Color::Palette(1),
        hunk: Color::Palette(6),
        syntax: "base16-ocean.dark",
        syntax_light: "InspiredGitHub",
    },
    Theme {
        name: "GitHub Light",
        background: Some(rgb(0xffffff)),
        text: Some(rgb(0x1f2328)),
        accent: rgb(0x0969da),
        code: rgb(0xcf222e),
        muted: rgb(0x8c959f),
        warn: rgb(0x9a6700),
        added: rgb(0x1a7f37),
        removed: rgb(0xcf222e),
        hunk: rgb(0x8250df),
        syntax: "InspiredGitHub",
        syntax_light: "InspiredGitHub",
    },
    Theme {
        name: "GitHub Dark",
        background: Some(rgb(0x0d1117)),
        text: Some(rgb(0xe6edf3)),
        accent: rgb(0x58a6ff),
        code: rgb(0xff7b72),
        muted: rgb(0x6e7681),
        warn: rgb(0xd29922),
        added: rgb(0x3fb950),
        removed: rgb(0xf85149),
        hunk: rgb(0xbc8cff),
        syntax: "base16-ocean.dark",
        syntax_light: "base16-ocean.dark",
    },
    Theme {
        name: "Solarized Light",
        background: Some(rgb(0xfdf6e3)),
        text: Some(rgb(0x586e75)),
        accent: rgb(0x268bd2),
        code: rgb(0xcb4b16),
        muted: rgb(0x93a1a1),
        warn: rgb(0xb58900),
        added: rgb(0x859900),
        removed: rgb(0xdc322f),
        hunk: rgb(0x6c71c4),
        syntax: "Solarized (light)",
        syntax_light: "Solarized (light)",
    },
    Theme {
        name: "Solarized Dark",
        background: Some(rgb(0x002b36)),
        text: Some(rgb(0x93a1a1)),
        accent: rgb(0x268bd2),
        code: rgb(0xcb4b16),
        muted: rgb(0x586e75),
        warn: rgb(0xb58900),
        added: rgb(0x859900),
        removed: rgb(0xdc322f),
        hunk: rgb(0x6c71c4),
        syntax: "Solarized (dark)",
        syntax_light: "Solarized (dark)",
    },
    Theme {
        name: "Nord",
        background: Some(rgb(0x2e3440)),
        text: Some(rgb(0xd8dee9)),
        accent: rgb(0x88c0d0),
        code: rgb(0xebcb8b),
        muted: rgb(0x616e88),
        warn: rgb(0xebcb8b),
        added: rgb(0xa3be8c),
        removed: rgb(0xbf616a),
        hunk: rgb(0xb48ead),
        syntax: "base16-ocean.dark",
        syntax_light: "base16-ocean.dark",
    },
    Theme {
        name: "Dracula",
        background: Some(rgb(0x282a36)),
        text: Some(rgb(0xf8f8f2)),
        accent: rgb(0xbd93f9),
        code: rgb(0x50fa7b),
        muted: rgb(0x6272a4),
        warn: rgb(0xf1fa8c),
        added: rgb(0x50fa7b),
        removed: rgb(0xff5555),
        hunk: rgb(0xff79c6),
        syntax: "base16-eighties.dark",
        syntax_light: "base16-eighties.dark",
    },
];

thread_local! {
    static CURRENT: Cell<usize> = const { Cell::new(0) };
    static LIGHT_TERMINAL: Cell<bool> = const { Cell::new(false) };
}

/// The theme called `name` (case and spacing ignored), else Auto.
pub(crate) fn index_of(name: &str) -> usize {
    let key = |s: &str| {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let wanted = key(name);
    THEMES
        .iter()
        .position(|t| key(t.name) == wanted)
        .unwrap_or(0)
}

pub(crate) fn set(index: usize, light_terminal: bool) {
    CURRENT.with(|c| c.set(index % THEMES.len()));
    LIGHT_TERMINAL.with(|c| c.set(light_terminal));
}

pub(crate) fn cycle() -> &'static Theme {
    let next = (current_index() + 1) % THEMES.len();
    CURRENT.with(|c| c.set(next));
    current()
}

pub(crate) fn current_index() -> usize {
    CURRENT.with(Cell::get)
}

pub(crate) fn current() -> &'static Theme {
    &THEMES[current_index()]
}

/// The syntect theme name for code in the current theme.
pub(crate) fn syntax_theme() -> &'static str {
    let theme = current();
    if LIGHT_TERMINAL.with(Cell::get) {
        theme.syntax_light
    } else {
        theme.syntax
    }
}

/// Plain text: the theme's text on its background.
pub(crate) fn base() -> CellAttributes {
    let theme = current();
    let mut a = CellAttributes::default();
    if let Some(bg) = theme.background {
        a.set_background(bg.attribute());
    }
    if let Some(fg) = theme.text {
        a.set_foreground(fg.attribute());
    }
    a
}

pub(crate) fn background() -> ColorAttribute {
    current()
        .background
        .map(Color::attribute)
        .unwrap_or_default()
}

/// Text in `color` on the theme's background.
pub(crate) fn colored(color: Color) -> CellAttributes {
    let mut a = base();
    a.set_foreground(color.attribute());
    a
}

pub(crate) fn rgb_text(r: u8, g: u8, b: u8) -> CellAttributes {
    colored(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_offers_exactly_these_themes() {
        let src = include_str!("../../../kaku/src/config_tui/mod.rs");
        let block = src
            .split("lua_key: \"markdown_theme\"")
            .nth(1)
            .and_then(|rest| rest.split("skip_write").next())
            .unwrap();
        for theme in THEMES {
            assert!(
                block.contains(&format!("\"{}\"", theme.name)),
                "{}",
                theme.name
            );
        }
        assert_eq!(block.matches("\",").count(), THEMES.len());
    }

    #[test]
    fn names_match_loosely_and_unknown_names_fall_back_to_auto() {
        assert_eq!(THEMES[index_of("github-dark")].name, "GitHub Dark");
        assert_eq!(THEMES[index_of("Solarized Light")].name, "Solarized Light");
        assert_eq!(index_of("nope"), 0);
    }

    #[test]
    fn every_syntax_theme_ships_with_syntect() {
        let themes = syntect::highlighting::ThemeSet::load_defaults().themes;
        for theme in THEMES {
            assert!(themes.contains_key(theme.syntax), "{}", theme.syntax);
            assert!(
                themes.contains_key(theme.syntax_light),
                "{}",
                theme.syntax_light
            );
        }
    }

    #[test]
    fn themed_text_carries_its_background_and_auto_keeps_the_terminals() {
        set(index_of("Nord"), false);
        assert_ne!(base().background(), ColorAttribute::Default);
        set(0, false);
        assert_eq!(base().background(), ColorAttribute::Default);
        assert_eq!(base().foreground(), ColorAttribute::Default);
    }
}
