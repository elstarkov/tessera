# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Tessera is a GPU-accelerated terminal emulator in Rust (egui/eframe) with native,
draggable pane splitting and tabs — the Ghostty look with iTerm2-style splits. Each
pane is a real PTY + shell; splitting is a native tiling tree, not tmux.

## Commands

```sh
cargo run --release              # build + launch (runs $SHELL in the first pane)
cargo run --release -- --help    # CLI usage
cargo run --release -- tmux new -A -s main   # any COMMAND/args run in every pane
cargo build                      # dev build (debug; release is much faster for actual use)
cargo test                       # run all tests (workspace: root crate + vendor/egui_term)
cargo test attach_merges_two_single_pane_tabs   # run one test by name
cargo clippy --workspace         # lint
cargo fmt                        # format
scripts/package.sh [--dmg]       # build dist/Tessera.app (macOS bundle, ad-hoc signed)
```

Tests live in `src/layout.rs` (split-tree behaviour), the tail of `src/app.rs`
(`reorder_index`), and the vendor crate. The `vendor/egui_term/examples/*` files
(`churn_stress`, `close_pane_leak`, `sync_bench`) are stress/leak harnesses — run with
`cargo run -p egui_term --example churn_stress`.

Profiling output (`flamegraph.svg`, `perf.data`) and the packaging output (`dist/`,
which contains stale `mockterm*` artifacts from before the rename) are git-ignored.

## Architecture

Cargo workspace: the root crate `tessera` (the binary) depends on the vendored,
locally-patched `vendor/egui_term` library crate.

**`src/main.rs`** — CLI parsing, config load, eframe window bootstrap. Shell
precedence: explicit CLI command > config `shell` key > `$SHELL` > `/bin/zsh`.

**`src/app.rs`** — `Tessera` is the `eframe::App`; `update()` (≈line 872) is the whole
per-frame loop: drain PTY events → handle shortcuts → draw tab strip → render the
active tab's panes/dividers → draw rename/search/settings popups. Key ownership model:

- Panes are **global**, keyed by a globally-unique `PaneId` (`panes: HashMap<PaneId,
  Pane>`), and outlive the tab structure. Each `Tab` holds a `Tree` (its split layout)
  plus its `focused` pane id; a pane belongs to exactly one tab's tree at a time.
- Each `Pane` owns a `TerminalBackend` = its own PTY + shell + a dedicated PTY event
  subscription thread that sends `(PaneId, PtyEvent)` down a single mpsc channel,
  drained in `drain_pty_events`.
- Tab tearing/merging is real subtree surgery: dragging a tab onto a pane calls
  `Tree::attach_subtree` (imports the dragged tab's *entire* layout as a split);
  `reorder_tab` just moves the tab in the strip.
- Chrome colours (pane card, gutter, bars) are derived from the theme background at
  startup in `Tessera::new`, not hardcoded.

**`src/layout.rs`** — the binary split tree. Stored in an **arena** (`Vec<Entry>` with
parent indices + a free list) so nodes reference each other by index without borrow-
checker fights. `geometry()` is a **pure** pass producing pane rects + divider handles,
decoupled from rendering so the same data drives both drawing and spatial keyboard
navigation (`neighbor()`). `close()` collapses a split into its surviving sibling.

**`src/config.rs`** — Ghostty-style `key = value` file at `~/.config/tessera/config`
(`$XDG_CONFIG_HOME` respected). A commented template is written on first run. Parsing
is deliberately tolerant: unknown keys / bad values are reported to stderr and skipped,
**never** aborting — a broken config still yields a working terminal. Config changes
apply on the **next launch** only (no live reload). Font families resolve via `fontdb`
(pure-Rust, no native deps). Discrete shortcuts (`Keybinds`) are rebindable; numbered
tab/pane switches (Cmd/Opt+1-9) and arrow navigation (Cmd+Alt+arrows) are fixed.

**`vendor/egui_term/`** — terminal widget over `alacritty_terminal`, forked from
upstream Harzu/egui_term (MIT) and patched for Tessera. Search for the `tessera patch`
comments before changing behaviour here. The notable local changes:

- Keyboard input follows the *focused* pane regardless of egui focus (`view.rs:146`).
- Regex scrollback search (`backend/mod.rs` `search`/`clear_search`).
- A `dirty` `AtomicBool` shared with the PTY thread so `sync()` skips re-cloning the
  whole grid on frames where nothing changed (hover, idle, a sibling's output).
- The PTY subscription thread `break`s when `recv()` returns `Err` (every sender
  dropped = pane closed) instead of busy-looping — a closed pane used to peg a core.

## Notes

- macOS-first (Cmd-key shortcuts, `package.sh`), though nothing is hard-bound to it.
- No live config reload, no inline images (Sixel/kitty), no ligatures/CJK/emoji fonts
  beyond the bundled Symbols Nerd Font fallback. Treat as a v0.1 hobby project.
