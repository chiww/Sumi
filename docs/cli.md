# CLI Reference

Run `sumi` in your terminal to see all available commands.

## sumi ai

Open the AI settings panel inside Sumi. Configure external coding tools (Claude Code, Codex, Gemini CLI, Copilot CLI, Kimi Code, etc.) and Sumi Assistant.

```bash
sumi ai
```

## sumi chat

Start Sumi's standalone AI chat from any shell. This is a discoverable alias for the bundled `k` helper, so it works even when `k` is not on your PATH.

```bash
sumi chat                 # open interactive chat
sumi chat "explain this"  # one-shot prompt
```

The chat uses `~/.config/sumi/assistant.toml`, shares the same conversation and memory files as the `Cmd + L` overlay, and supports `/new`, `/resume`, `/clear`, `/status`, `/memory`, and `/exit` in interactive mode.

## sumi config

Open the Sumi configuration TUI for common settings and Lua overrides. It creates `~/.config/sumi/sumi.lua` if it is missing. Inside Sumi, `Cmd + ,` opens the same screen.

```bash
sumi config
```

## sumi doctor

Check the Sumi app bundle, PATH entries, and zsh or fish shell integration. Use this first if something feels broken.

Every run also writes a diagnostic bundle to `~/.local/share/sumi/diagnostics/Sumi-Diagnose.zip`: the report, logs, recent crash reports, a short sample of a running Sumi, and any GUI stalls Sumi recorded on its own. Home paths, the host name, MAC addresses, serial numbers and credential-looking values are replaced first, `assistant.toml` is never read, and nothing leaves your Mac. Attach the zip when you report an issue. If Sumi itself is frozen, run `sumi doctor` from Terminal.app.

```bash
sumi doctor
sumi doctor --shell fish       # check fish even when $SHELL points to zsh
sumi doctor --shell fish --fix # repair the selected integration
```

## sumi update

Check for and install the latest Sumi release.

```bash
sumi update
```

## sumi reset

Remove Sumi-managed shell and tmux integration, Sumi-managed git delta defaults, selected Sumi state, and managed theme blocks in `~/.config/sumi/sumi.lua`. It also deletes Sumi AI memory (`soul/` and `ai_chat_memory.md`), the AI chat onboarding flag, and the `backups/` folder in `~/.config/sumi`. User-authored Lua outside managed blocks is preserved. Use with caution and run `sumi init` again if you want shell integration back.

```bash
sumi reset
sumi reset --shell fish # use fish for restart and restore guidance
```

## sumi remote

Use this Mac's terminals from a browser on another device, through Sumi's relay. Remote access is off until you turn it on.

```bash
sumi remote               # turn remote access on (asks first) and print a pairing link
sumi remote devices       # list paired browsers
sumi remote revoke <id>   # unpair a browser; its open sessions end within seconds
sumi remote off           # turn remote access off
sumi remote --reset       # new identity, every browser unpaired
```

The page is made for desktop browsers for now. Open the link in the browser you want to pair, check that the browser and `sumi remote` show the same six-digit code, and press Enter in the terminal to allow it, or Ctrl+C to refuse. Someone who gets hold of the link cannot pair without that Enter. A link works once, for 10 minutes, so run `sumi remote` again for each device. After pairing, bookmark the page: the browser keeps its own key and comes back without a new link. What you type and see is encrypted between the browser and Sumi, and the relay only forwards it. Sumi posts a notification when a paired browser returns after a while. `sumi doctor` shows whether Sumi is connected to the relay.

## sumi init

Set up Sumi's shell integration for zsh or fish. When both shells are installed, an interactive run asks which one to configure. Use `--shell` to make the choice explicit in scripts or when `$SHELL` does not match your daily shell. Also installs optional CLI tools (Starship, Delta, Lazygit, Yazi) via Homebrew.

```bash
sumi init
sumi init --shell fish
sumi init --shell zsh --update-only
```

If the `sumi` command goes missing from your shell, restore it with:

```bash
/Applications/Sumi.app/Contents/MacOS/sumi init --update-only
exec zsh -l
```

## sumi cli

Interact with the Sumi multiplexer from scripts and external tools.

```bash
sumi cli split-pane                          # split current pane
sumi cli split-pane -- bash -c "echo hello"  # split and run a command
sumi cli --help                              # list all subcommands
sumi cli split-pane --help                   # help for a specific subcommand
```

Useful for integrating Sumi with AI tools or shell scripts that need to open panes or tabs programmatically.

`wait` and `events` read the state Claude Code and Codex report over OSC 7501 (`working`, `blocked`, `done`, `error`, `idle`), which `list --format json` shows as `program_status`:

```bash
sumi cli wait --pane-id 5 --for done,blocked --next --timeout 20m   # until that agent finishes or needs you
sumi cli events --for blocked                                       # a line each time a pane starts waiting on you
```

[agent-skill.md](agent-skill.md) teaches a coding agent to use these commands. It is a skill file; to give it to Claude Code:

```bash
mkdir -p ~/.claude/skills/kaku
curl -fsSL https://raw.githubusercontent.com/tw93/Kaku/main/docs/agent-skill.md -o ~/.claude/skills/kaku/SKILL.md
```
