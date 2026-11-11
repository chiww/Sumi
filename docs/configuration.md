# Configuration

## Config File

Sumi creates `~/.config/sumi/sumi.lua` with a commented template on first launch, and `sumi init` or `sumi config` recreate it if it is missing. Open it with `sumi config` or `Cmd + ,`.

The file loads the bundled Sumi defaults first, then applies your overrides on top:

```lua
local wezterm = require 'wezterm'

local function resolve_bundled_config()
  local resource_dir = wezterm.executable_dir:gsub('MacOS/?$', 'Resources')
  local bundled = resource_dir .. '/sumi.lua'
  local f = io.open(bundled, 'r')
  if f then f:close(); return bundled end
  return '/Applications/Sumi.app/Contents/Resources/sumi.lua'
end

local config = {}
local bundled = resolve_bundled_config()
if bundled then
  local ok, loaded = pcall(dofile, bundled)
  if ok and type(loaded) == 'table' then config = loaded end
end

-- User overrides:
config.font_size = 16
config.window_background_opacity = 0.95

return config
```

The generated file is a longer version of this, with commented examples below `-- User overrides:`; most users only need to uncomment the lines they want to change.

## Common Overrides

Add only the settings you want to change:

```lua
config.font_size = 16
config.window_background_opacity = 0.95

-- New installs start with config.color_scheme = "Sumi Dark". Change it to:
-- config.color_scheme = "Sumi Light"
-- config.color_scheme = "Auto"  -- follow macOS appearance
```

Keep terminal behavior, appearance, key bindings, launch behavior, and other WezTerm-compatible settings in `sumi.lua`.

Manage Sumi Assistant settings with `sumi ai`. That command writes `~/.config/sumi/assistant.toml` for model, base URL, auth, API keys, and tool settings. The Lua/TOML split is intentional: `sumi.lua` stays compatible with WezTerm-style terminal configuration, while `assistant.toml` is managed by the AI setup flow.

## Language

Sumi ships built-in English and Simplified Chinese (`zh-CN`) translations
for its menus, command palette, AI overlay, and CLI TUIs. The selected
language also asks the AI Assistant to reply in Simplified Chinese by
default; in English the Assistant follows the language you write in.

```lua
config.language = "auto"   -- default: detect from $LC_ALL / $LANG; fall back to English
config.language = "zh-CN"  -- force Simplified Chinese
config.language = "en"     -- force English
```

Resolution order when `"auto"` is set:

1. `$LC_ALL`
2. `$LC_MESSAGES`
3. `$LANG`
4. English (fallback)

When the desktop app starts from the Dock or Finder, `$LANG` is usually
unset; Sumi then derives it from the macOS **region** (System Settings >
General > Language & Region), not the preferred language. If the two
differ, set `config.language` explicitly.

Unsupported values fall back to English and emit a warning in the log.
The macOS system menu bar (File / Edit / Window) is owned by the OS;
it follows your system language and is **not** affected by
`config.language`. Re-launch Sumi after changing the value.

> **Note:** `sumi.lua` is loaded by the desktop app and the `sumi` CLI.
> The standalone `k` chat CLI resolves its locale from the environment
> only (`LC_ALL` / `LC_MESSAGES` / `LANG`); set those variables in your
> shell rc to control `k`.

---

## Appearance

**Theme**

New installs start in Sumi Dark, because the generated `sumi.lua` sets `config.color_scheme = "Sumi Dark"`. Installs whose config has no `color_scheme` line keep following macOS appearance, switching between Sumi Dark and Sumi Light automatically. To follow macOS appearance, choose **Auto** in `sumi config` or remove the `config.color_scheme` line. To force one theme:

```lua
config.color_scheme = "Sumi Dark"   -- always dark
config.color_scheme = "Sumi Light"  -- always light
```

**Custom themes**

Sumi reads WezTerm-compatible color scheme files from the `colors` directory next to your config: `~/.config/sumi/colors/*.toml` by default, plus any directory listed in `config.color_scheme_dirs`. Everything `sumi config` needs to list a theme comes from those files, so no registration step is required.

```toml
# ~/.config/sumi/colors/example-dark.toml
[colors]
background = "#101014"
foreground = "#e6e2d8"
cursor_bg = "#e6e2d8"
cursor_fg = "#101014"
selection_bg = "#2c2a36"
selection_fg = "#e6e2d8"
ansi = ["#3a3646", "#e1505d", "#2bae85", "#e2c027", "#619ac3", "#c08eaf", "#57c3c2", "#cfc8bb"]
brights = ["#6b6478", "#ea586c", "#45b787", "#eed045", "#8fb2c9", "#d1c2d3", "#93d5dc", "#f4efe4"]

[metadata]
name = "Example Dark"
```

The selected value is the scheme's `[metadata].name`, or the file name when that field is absent. Keep the file name equal to `name` so the scheme loads without scanning the whole directory. A scheme your `sumi.lua` defines directly (`config.color_schemes["Name"] = { ... }`) is listed too.

After the file is in place, open `sumi config` and pick it from **Theme**; the command writes `config.color_scheme = "Example Dark"` for you. **Auto** still switches only between Sumi Dark and Sumi Light, so a custom theme is a manual choice. With a custom theme, the title bar, the Yazi theme and the Claude Code theme Sumi keeps in sync stay on Sumi Dark colors, even when the theme itself is light.

**Color overrides**

Remap specific hex colors to keep theme consistency with apps that output their own colors. `color_overrides` applies to rendered backgrounds, including palette-backed ANSI backgrounds and truecolor backgrounds. `foreground_color_overrides` applies only to truecolor text:

```lua
config.color_overrides = {
  ['#6E6E6E'] = '#3A3942',
}

config.foreground_color_overrides = {
  ['#FFFFDB'] = '#575653',
}
```

**Font**

Sumi defaults to JetBrains Mono with PingFang SC as CJK fallback. Change font:

```lua
config.font = wezterm.font("Fira Code")
```

Sumi disables ligatures by default. Re-enable:

```lua
config.harfbuzz_features = {}
```

**Font size**

Sumi auto-selects 15px (low-res) or 17px (high-res) based on your display. Override:

```lua
config.font_size = 16
```

**Line height**

```lua
config.line_height = 1.28  -- default
```

The default favors readable text spacing. Character-cell graphics (QR codes, `neofetch` logos, TUI charts) stretch with the row height; set `1.0`–`1.1` if you want them near-square. See the [FAQ](faq.md#qr-codes-and-terminal-graphics-look-vertically-stretched) for details.

**Window transparency**

```lua
config.window_background_opacity = 0.92
config.macos_window_background_blur = 20  -- optional blur (0–100)
```

**Traffic lights (macOS)**

By default, Sumi embeds the macOS traffic light buttons into the tab bar area using `INTEGRATED_BUTTONS|RESIZE`. To hide the traffic lights while keeping resize edges and tab-bar dragging:

```lua
config.window_decorations = "RESIZE"
```

`RESIZE` preserves the ability to resize the window from its edges and drag it by the tab bar; it only removes the close/minimize/zoom buttons.

**Agent title**

While Claude Code, Codex or Kimi runs in the active pane, its mark and current topic show at the top right of the window, and at the right end of the remote access page's tab bar. When the agent already shows the same topic on its input box border, only the mark stays. To turn it off, choose Off for Agent Title in `sumi config`, or:

```lua
config.show_agent_title = false
```

**Padding**

```lua
config.window_padding = { left = '24px', right = '24px', top = '40px', bottom = '20px' }
```

Sizes accept `px`, `pt`, `cell`, and `%`. `px` is a physical pixel value and does not scale with display DPI, so the same value can look smaller on a high-density display. Use `pt` for DPI-scaled spacing or `cell` for spacing relative to the terminal cell size, for example `top = '15pt'`.

## Terminal Behavior

**Cursor**

```lua
config.default_cursor_style = "BlinkingBar"
config.cursor_thickness = "2px"
config.cursor_blink_rate = 500
```

**Scrollback**

```lua
config.scrollback_lines = 10000  -- default
```

**File link editor**

Set an editor command for local file links printed in the terminal. Sumi appends the resolved `path`, or `path:line:column` when the link includes a location. Shell-style quoting is supported for command arguments:

```lua
config.file_link_editor = "zed"
-- config.file_link_editor = "cursor --goto"
```

This setting takes priority over Sumi's automatic VS Code detection. When it is unset, Sumi keeps the existing VS Code and `$VISUAL` / `$EDITOR` fallback behavior. Directory links always open in Finder, whether or not this setting is set.

**Copy on select**

Enabled by default. Disable:

```lua
config.copy_on_select = false
```

**Strip leading whitespace on copy**

When copying indented multi-line text (e.g. from a code block), remove the shared leading whitespace so the pasted result starts at column 0:

```lua
config.copy_strip_leading_whitespace = true  -- default: false
```

**Restore previous session**

Re-open the tabs and panes from your last session on launch. Enabled by default; set it to `false` to disable saving and restoring the session:

```lua
config.restore_previous_session = false  -- default: true
```

**Working directory inheritance**

```lua
config.window_inherit_working_directory = true   -- new windows
config.tab_inherit_working_directory = true       -- new tabs
config.split_pane_inherit_working_directory = true -- new splits
```

**Focused pane emphasis**

To make the focused pane easier to spot, adjust the colors of inactive panes:

```lua
config.inactive_pane_hsb = {
  hue = 0.95,
  saturation = 1.0,
  brightness = 0.9,
}
```

Each value multiplies the pane's existing HSV component, so `1.0` leaves that component unchanged. `hue` is not a color value or a degree-based rotation; small adjustments work best. The focused pane itself is not transformed.

**Tab bar**

Hidden when only one tab is open. Auto-generated tab titles show the current directory by default. You can change the position, shorten path titles, or opt into showing the foreground command alongside the path:

```lua
config.tab_bar_at_bottom = false                   -- move to top
config.tab_title_show_basename_only = true         -- show "dirname" instead of "parent/dirname"
config.tab_title_show_foreground_process = true    -- show "dirname·codex" while commands run
config.tab_title_use_pane_title = true             -- show application titles set via OSC 0/2
config.show_tab_index_in_tab_bar = true            -- number tabs: "1:dirname", "2:dirname"
```

`show_tab_index_in_tab_bar` (Settings: Tab Numbers) prefixes each tab with its position, so `Cmd + 1` through `Cmd + 8` select the tabs numbered 1 to 8. `Cmd + 9` always selects the last tab, whatever its number.

With `tab_title_use_pane_title`, the active pane's title also supplies the window title and the title of a split tab. Explicit tab renames still take precedence. Shell prompt hooks may overwrite application titles after a command exits; this is independent of OSC support.

The trailing cell stays blank until a pane wants your attention, then shows an amber dot. Background tabs that emit BEL use it, so does any program that reports an error or paused state through the OSC 9;4 progress sequence, and so does Claude Code when it waits on you, finishes or fails (OSC 7501). Disable only the BEL part if you do not want tab-level bell notifications:

```lua
config.bell_tab_indicator = false
```

**Left panel**

Sumi has a tool panel at the window's left edge, in the spirit of Warp's: a project file tree, search in files, saved command snippets and the list of AI chats. Tabs stay in the top tab bar. Toggle it with View > Left Panel (`Cmd + \`), jump to a tool with `Ctrl + Cmd + 1` to `4` (pressing a tool's shortcut again hides the panel), or turn it on in `sumi config` or Lua:

```lua
config.left_panel = true
config.left_panel_width = 260                    -- points, 180 to 560
config.left_panel_font = wezterm.font('Menlo')   -- unset: the window title font
config.left_panel_font_size = 13                 -- unset: window_frame.font_size, else 12
config.left_panel_line_height = 1.2              -- row spacing, 0.8 to 2.5
```

The same three are in `sumi config` under Window (Panel Font, Panel Font Size, Panel Line Height); Font and Panel Font list the installed fonts, and typing jumps through the list.

- **Files** follows the active pane's directory up to its Git root and hides `.gitignore`d entries. Click a folder to expand it and a file to open it beside the terminal; right-click for open in a new tab or the default app, reveal in Finder, `cd` here, new tab here, new file or folder, rename, move to Trash, copy path and set as root.
- **Search** looks through the project's text files as you type (match case and regex toggles), honouring `.gitignore`. Click a match to open the file at that line.
- **Snippets** live in `~/.config/sumi/snippets.toml`. Click one to type it into the active pane; right-click to run it. Text in `{{double braces}}` marks a placeholder, so such a snippet is typed without Enter.
- **AI chats** lists saved Sumi Assistant conversations with a filter; click one to reopen it in the chat overlay.

Files open in Sumi's viewer pane, split to the right; later files reuse it. Markdown opens rendered, `Cmd + P` switches to the editable source, and other text files open in the editor with syntax highlighting. `Cmd + S` saves, `Cmd + Z` / `Cmd + Shift + Z` undo and redo, `Cmd + F` finds, and `Ctrl + Q` closes the viewer (it asks again when there are unsaved edits). A file changed on disk reloads when there are no unsaved edits. `Ctrl + T` cycles the viewer's color theme; `config.markdown_theme` (or Markdown Theme in `sumi config`) picks the one it opens with: `'Auto'` follows the terminal's colors, and `'GitHub Light'`, `'GitHub Dark'`, `'Solarized Light'`, `'Solarized Dark'`, `'Nord'` and `'Dracula'` bring their own background and code colors. Binary files such as images and PDFs open in their default app. You can also run the viewer yourself with `Sumi.app/Contents/MacOS/kaku-gui --sumi-view <file>`.

The panel's visibility, tool and width are remembered across restarts until you change `left_panel` or `left_panel_width` in the config.

**Scrollbar**

Disabled by default. Enable via `sumi config` (toggle the scrollbar style option) or in Lua:

```lua
config.enable_scroll_bar = true
```

If you want the mouse wheel to scroll inside alternate-screen apps such as nano and vim, instead of peeking into Sumi's primary scrollback, enable:

```lua
config.alternate_screen_wheel_scrolls_terminal = true
```

**Selection drag + mouse wheel**

Controls what the mouse wheel does while you are dragging out a selection with the left mouse button held down. Defaults to `"Extend"`, which matches macOS `NSTextView` apps such as Safari, TextEdit, VS Code, iTerm2 and `Terminal.app`: the wheel scrolls the scrollback and the selection grows to follow the cursor across screens.

```lua
-- Default (recommended): scroll AND extend the selection so you can grab
-- text that spans more than one screen of output.
config.selection_wheel_scroll_behavior = "Extend"

-- Scroll the scrollback but leave the selection range untouched.
config.selection_wheel_scroll_behavior = "ScrollOnly"

-- Drop the wheel event entirely (the old behavior);
-- selecting text that does not fit on one screen requires releasing the
-- mouse, scrolling, and re-selecting.
config.selection_wheel_scroll_behavior = "Ignore"
```

**macOS Option key**

Left Option sends Meta (useful for Vim/Neovim word navigation). Right Option sends compose characters.

```lua
config.send_composed_key_when_left_alt_is_pressed = false  -- default: left = Meta
config.send_composed_key_when_right_alt_is_pressed = true  -- default: right = Compose
```

## Updates

Sumi checks GitHub for new releases in the background (enabled by default) and quietly downloads a newer version when it finds one. It never installs on its own: it shows a notification, and clicking it asks you to confirm first, since applying an update closes every window and stops running tasks.

Turn off background checks entirely:

```lua
config.check_for_updates = false
```

Change how often it checks (default `10800`, i.e. every 3 hours):

```lua
config.check_for_updates_interval_seconds = 86400  -- once a day
```

You can always update manually with `sumi update` or from the app menu, regardless of these settings.

## Custom Keybindings

Always **insert** into `config.keys`, never replace it. Replacing erases all Sumi defaults.

```lua
-- Navigate pane right
table.insert(config.keys, {
  key = 'RightArrow',
  mods = 'CMD|SHIFT',
  action = wezterm.action.ActivatePaneDirection('Right'),
})

-- Split pane horizontally
table.insert(config.keys, {
  key = 'Enter',
  mods = 'CMD|OPT',
  action = wezterm.action.SplitHorizontal({ domain = 'CurrentPaneDomain' }),
})
```

Full list of available actions: [WezTerm KeyAssignment reference](https://wezfurlong.org/wezterm/config/lua/keyassignment/).

## Advanced

**Enterprise proxy headers**

Add custom HTTP headers to Sumi Assistant API requests (for corporate proxies or API gateways):

```toml
# ~/.config/sumi/assistant.toml
custom_headers = ["X-Customer-ID: your-id", "X-Org: your-org"]
```

Note: `Authorization` and `Content-Type` are reserved and cannot be overridden.

**Extend Command Palette**

Add a custom command to Command Palette (`Cmd + Shift + P`) via `sumi.lua`:

```lua
wezterm.on('augment-command-palette', function(window, pane)
  if not pane then return {} end

  local cwd_obj = pane:get_current_working_dir()
  if not cwd_obj then return {} end

  -- Finder can only reveal local paths. file_path is already URL-decoded,
  -- so directories containing spaces or non-ASCII characters work too.
  local host = cwd_obj.host
  if cwd_obj.scheme ~= 'file'
      or (host and host ~= '' and host ~= 'localhost' and host ~= wezterm.hostname():lower()) then
    return {}
  end
  local cwd = cwd_obj.file_path
  if not cwd then return {} end

  return {
    {
      brief = 'Reveal in Finder',
      doc = 'Reveal current directory in Finder',
      action = wezterm.action_callback(function()
        wezterm.run_child_process({ 'open', '-R', cwd })
      end),
    },
  }
end)
```

**Full WezTerm Lua API**

Sumi uses WezTerm's configuration system. Any WezTerm config option works in `sumi.lua`. For the complete reference, see:

- [WezTerm config options](https://wezfurlong.org/wezterm/config/)
- [WezTerm Lua API](https://wezfurlong.org/wezterm/config/lua/)

### IME composition

To keep the character after the cursor visible when composing Korean, Japanese or Chinese text in the middle of a line:

```lua
config.ime_preedit_rendering = 'BuiltinInsert'
```

This shifts the remaining cells of the current row for display only and clips at the pane edge. The terminal buffer and bytes sent to the application stay unchanged. The default `Builtin` mode overlays the preedit; `System` does not select native preedit rendering on macOS.
