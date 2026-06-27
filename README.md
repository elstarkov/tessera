<p align="center">
  <img src="assets/icon.svg" width="120" alt="Tessera icon">
</p>

<h1 align="center">Tessera</h1>

<p align="center">
  A GPU-accelerated terminal emulator in Rust with native, draggable pane
  splitting and tabs - the Ghostty look with iTerm2-style splits.
</p>

---

Tessera tiles your shells like a mosaic: split panes by keyboard **or** by
dragging the borders between them, organise them into colourable, renamable,
reorderable tabs, and drag a tab straight onto a pane to merge it in. Each pane
is a real terminal running a real shell (or `tmux`).

## Features

- **Native splits** - a binary tiling tree of panes, no `tmux` required. Each
  pane owns its own PTY + shell.
- **Draggable everything** - drag a border to resize neighbours; drag a tab to
  reorder it, or drop it onto a pane to merge it in as a split (tab tearing),
  iTerm2-style, with live drop previews.
- **Tabs** - open with `Cmd+T`, jump with `Cmd+1..9`; **right-click for a
  colour**, **double-click to rename**, drag to reorder.
- **Keyboard-first** - splits, pane navigation (`Cmd+Alt+Arrow`), and direct
  pane focus (`Opt+1..9`).
- **GPU-accelerated** rendering via `eframe`/`egui`.
- **Real terminal emulation** - full VT/ANSI, colours, scrollback, selection,
  copy/paste - powered by Alacritty's terminal core.
- **Nerd Font ready** - bundles a Nerd Font symbols fallback, so prompt icons
  and powerline glyphs render out of the box.

## Keybindings

| Shortcut | Action |
|---|---|
| `Cmd+T` | New tab |
| `Cmd+1` … `Cmd+9` | Switch to tab N |
| `Opt+1` … `Opt+9` | Focus pane N in the current tab |
| `Cmd+D` | Split right (panes side-by-side) |
| `Cmd+Shift+D` | Split down (panes stacked) |
| `Cmd+W` | Close the focused pane |
| `Cmd+F` | Search the scrollback (Enter / Shift+Enter to step) |
| `Cmd+Alt+←/→/↑/↓` | Move focus between panes |
| drag a border | Resize the two adjacent panes |
| double-click a tab | Rename it · right-click a tab | Set its colour |
| drag a tab | Reorder it in the strip, or drop on a pane to merge |

## Run it

Run Tessera straight from source - all you need is the
[Rust toolchain](https://rustup.rs):

```sh
git clone https://github.com/elstarkov/tessera
cd tessera
cargo run --release            # launches your $SHELL
cargo run --release -- --help  # usage
```

No install step, no app bundle, no Gatekeeper prompts - just clone and run.

## tmux

Tessera gives you native GUI splits without tmux. To drive panes from tmux
instead, run it as the command: `tessera tmux new -A -s main`.

## Configuration

Tessera reads a Ghostty-style `key = value` file at
`~/.config/tessera/config` (a commented template is written there on first
run - or open it from the gear menu in the top-right). Changes apply on the
next launch.

```
font-family      = "JetBrains Mono"
font-size        = 14
theme            = catppuccin-mocha
window-padding-x = 8
window-padding-y = 8
shell            = /bin/zsh
background       = #1e1e2e   # optional, overrides the theme
foreground       = #cdd6f4

# Rebind the discrete shortcuts (modifiers: cmd / ctrl / alt / shift):
keybind-new-tab     = cmd+t
keybind-split-right = cmd+d
keybind-split-down  = cmd+shift+d
keybind-close-pane  = cmd+w
keybind-find        = cmd+f
```

Bundled themes: `default`, `catppuccin-mocha`, `dracula`, `nord`,
`tokyo-night`, `gruvbox-dark`, `solarized-dark`. Unknown keys and bad values
are reported on stderr and skipped - a broken config still gives you a terminal.

## Limitations

- **Fonts:** Nerd Font icons and powerline glyphs render via a bundled symbols
  fallback. CJK and colour emoji still need their own fonts; no ligatures.
- **Some shortcuts are fixed:** the discrete actions are rebindable (see
  [Configuration](#configuration)), but tab/pane switching (Cmd/Opt+1-9) and
  pane navigation (Cmd+Alt+arrows) aren't.
- **No inline images** (Sixel / kitty / iTerm protocols).
- **Not security-audited.** `cargo audit` is clean, but treat it as a v0.1
  hobby project, not hardened software.

## Architecture

```
src/
  main.rs     CLI parsing + window bootstrap (eframe) + config load
  app.rs      Tessera: update loop, tabs, rendering, dividers, shortcuts,
              drag-and-drop (reorder / merge), rename + search popups,
              settings menu, PTY events
  config.rs   Ghostty-style config file: parsing, bundled themes, font lookup
  layout.rs   Arena-based binary split tree + pure geometry pass + merge
vendor/
  egui_term/  Vendored terminal widget (MIT), patched so keyboard input
              follows the focused pane, plus regex scrollback search
```

## Credits & license

Tessera is [MIT-licensed](LICENSE). It vendors and lightly patches
[`egui_term`](https://github.com/Harzu/egui_term) (MIT), bundles
[Symbols Nerd Font](https://www.nerdfonts.com) (MIT), and builds on
[`alacritty_terminal`](https://github.com/alacritty/alacritty),
[`egui`/`eframe`](https://github.com/emilk/egui), and `portable-pty`.

Built in Rust, pair-programmed with [Claude Code](https://claude.com/claude-code).

## Roadmap

- Font fallback (Nerd Fonts / emoji / CJK) and ligatures
- Rebindable tab/pane number and arrow-navigation shortcuts (discrete actions already configurable)
- tmux control-mode (`tmux -CC`) integration
- Zoom a pane to fullscreen
