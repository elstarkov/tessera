//! The Tessera application: owns the tabs (each a pane tree), the terminal
//! backends, and the eframe update loop that renders the active tab's panes,
//! handles draggable dividers, routes keyboard shortcuts, and drains PTY events.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui;
use egui::{
    pos2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, KeyboardShortcut, Margin, Modifiers,
    Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder, Vec2,
};
use egui_term::{
    BackendCommand, BackendSettings, FontSettings, PtyEvent, TerminalBackend, TerminalFont,
    TerminalMode, TerminalTheme, TerminalView,
};

use crate::config::{KeySpec, Keybinds, Settings};
use crate::layout::{neighbor, Axis, Dir, PaneId, Tree};

/// Launch configuration (what each pane runs).
pub struct Config {
    pub shell: String,
    pub args: Vec<String>,
}

struct Pane {
    backend: TerminalBackend,
    title: String,
}

/// One tab: an independent pane layout with its own focused pane.
struct Tab {
    tree: Tree,
    focused: PaneId,
    /// Content rect of the last frame this tab was drawn, for spatial navigation.
    last_area: Rect,
    /// Optional user-assigned colour (right-click → tab colour).
    color: Option<Color32>,
    /// Optional custom name (double-click to rename); falls back to pane title.
    name: Option<String>,
}

/// Inline tab-rename state: which tab, the edit buffer, and whether we've focused
/// the text field yet (so we only steal focus once).
struct Editing {
    idx: usize,
    buf: String,
    focused: bool,
}

/// Drag-and-drop payload carried while dragging a tab onto a pane.
#[derive(Clone, Copy)]
struct TabDrag {
    src: usize,
}

/// Scrollback-search state for the focused pane (Cmd+F).
struct Search {
    pane: PaneId,
    query: String,
    /// True once the user clicked into the terminal while search is open, so
    /// keystrokes go to the shell instead of the search field. Click the field
    /// to bring focus back.
    terminal_focused: bool,
    /// (current 1-based match, total matches) for the "3/20" counter.
    matches: (usize, usize),
}

pub struct Tessera {
    tabs: Vec<Tab>,
    active: usize,
    /// All panes across all tabs, keyed by their globally-unique id.
    panes: HashMap<PaneId, Pane>,
    next_id: u64,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    theme: TerminalTheme,
    font: TerminalFont,
    cfg: Config,
    default_title: String,
    editing: Option<Editing>,
    search: Option<Search>,
    /// Chrome surfaces, derived from the theme background at startup.
    term_bg: Color32, // pane card + the padding frame behind the terminal
    window_bg: Color32, // gutter / divider gaps
    bar_bg: Color32,    // tab strip + status bar
    /// Per-pane inner padding (window-padding-x / -y).
    pad: Vec2,
    /// User-rebindable discrete shortcuts.
    keybinds: Keybinds,
    /// The pane whose drag-grip the pointer is currently holding down, if any -
    /// the single source of truth for a pane drag. Tracked in app state (rather
    /// than an egui drag-payload) so it survives the cursor crossing out of the
    /// pane area into the top chrome; re-tiling, tearing into a tab, and the
    /// float card all key off it. Set on press, cleared when the button lifts.
    pane_grip_down: Option<PaneId>,
    /// True while the quit-confirmation dialog is up (Cmd+Q / window close).
    confirm_quit: bool,
    /// Set once the user confirms quitting, so the follow-up close request is
    /// let through instead of being intercepted again.
    quit_confirmed: bool,
}

const ACCENT: Color32 = Color32::from_rgb(102, 161, 255);
/// Destructive-action fill (the quit dialog's confirm button).
const DANGER: Color32 = Color32::from_rgb(210, 85, 85);
const DIV_IDLE: Color32 = Color32::from_rgb(60, 62, 72);
const DIV_HOT: Color32 = Color32::from_rgb(120, 150, 210);

// Surfaces. The pane/gutter/bar colours are derived per-theme at startup (see
// Tessera::new); this is the fallback used when a theme's background can't be
// parsed.
const DEFAULT_TERM_BG: Color32 = Color32::from_rgb(0x18, 0x18, 0x18);
const TAB_SEL: Color32 = Color32::from_rgb(40, 46, 62);
const TAB_IDLE: Color32 = Color32::from_rgb(28, 29, 36);
const TAB_HOVER: Color32 = Color32::from_rgb(36, 38, 47);

/// Preset tab colours offered in the right-click menu (à la iTerm2).
const TAB_PRESETS: &[(&str, Color32)] = &[
    ("Red", Color32::from_rgb(220, 80, 80)),
    ("Orange", Color32::from_rgb(225, 145, 60)),
    ("Yellow", Color32::from_rgb(225, 200, 70)),
    ("Green", Color32::from_rgb(110, 190, 110)),
    ("Teal", Color32::from_rgb(80, 190, 190)),
    ("Blue", Color32::from_rgb(90, 150, 235)),
    ("Purple", Color32::from_rgb(170, 120, 225)),
    ("Pink", Color32::from_rgb(225, 120, 180)),
];

// Geometry.
const PANE_RADIUS: u8 = 10; // rounded pane corners
const GUTTER: i8 = 0; // no outer margin: the focus border sits flush to the edge
const TAB_RADIUS: u8 = 8;
const TAB_MIN_W: f32 = 150.0; // minimum tab width, so tabs feel roomy

impl Tessera {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, settings: Settings) -> Self {
        // Resolve the configured font (by family name) and register it as the
        // primary monospace face. A missing/unknown family falls back to the
        // bundled default, with a note on stderr.
        let user_font = settings.load_font();
        if settings.font_family.is_some() && user_font.is_none() {
            eprintln!(
                "tessera: font-family '{}' not found, using the default font",
                settings.font_family.as_deref().unwrap_or_default()
            );
        }
        let has_bold_face = configure_fonts(&cc.egui_ctx, user_font, settings.load_bold_font());
        let bold_font_type = if has_bold_face {
            FontId::new(
                settings.font_size,
                egui::FontFamily::Name(BOLD_FAMILY.into()),
            )
        } else {
            FontId::monospace(settings.font_size)
        };

        // Build the terminal theme from the chosen palette, and derive the
        // surrounding chrome (pane card, gutter, bars) from its background so a
        // light-on-dark theme stays visually consistent.
        let palette = settings.palette();
        let term_bg = crate::config::parse_hex(&palette.background).unwrap_or(DEFAULT_TERM_BG);
        let theme = TerminalTheme::new(Box::new(palette));

        // Derive the window chrome from the theme background, and theme egui's
        // menus / tooltips to match so popups aren't default-grey boxes.
        let window_bg = darken(term_bg, 0.55);
        let bar_bg = darken(term_bg, 0.82);
        configure_style(&cc.egui_ctx, elevate(bar_bg, 10));

        let (pty_tx, pty_rx) = channel();
        let default_title = shell_basename(&cfg.shell);
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            panes: HashMap::new(),
            next_id: 0,
            pty_tx,
            pty_rx,
            theme,
            font: TerminalFont::new(FontSettings {
                font_type: FontId::monospace(settings.font_size),
                bold_font_type,
            }),
            cfg,
            default_title,
            editing: None,
            search: None,
            term_bg,
            window_bg,
            bar_bg,
            pad: Vec2::new(settings.padding.0, settings.padding.1),
            keybinds: settings.keybinds.clone(),
            pane_grip_down: None,
            confirm_quit: false,
            quit_confirmed: false,
        };
        // Turn Cmd+Q into a window-close request so the quit confirmation in
        // update() can intercept it (by default the menu's Quit item kills
        // the process before the frame loop ever sees anything).
        #[cfg(target_os = "macos")]
        crate::macos::route_quit_through_close(cc);
        // First tab fills the window. If the shell can't spawn we can't do
        // anything useful, so fail loudly.
        let id = app
            .spawn_pane(&cc.egui_ctx)
            .expect("failed to spawn initial shell");
        app.tabs.push(Tab {
            tree: Tree::new(id),
            focused: id,
            last_area: Rect::ZERO,
            color: None,
            name: None,
        });
        app.active = 0;
        app
    }

    /// Spawn a fresh terminal backend and register it in the global pane map.
    fn spawn_pane(&mut self, ctx: &egui::Context) -> io::Result<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let backend = TerminalBackend::new(
            id,
            ctx.clone(),
            self.pty_tx.clone(),
            BackendSettings {
                shell: self.cfg.shell.clone(),
                args: self.cfg.args.clone(),
                working_directory: default_working_dir(),
            },
        )?;
        self.panes.insert(
            id,
            Pane {
                backend,
                title: self.default_title.clone(),
            },
        );
        Ok(id)
    }

    /// Open a new tab containing a single fresh pane, and focus it.
    fn new_tab(&mut self, ctx: &egui::Context) {
        match self.spawn_pane(ctx) {
            Ok(id) => {
                self.tabs.push(Tab {
                    tree: Tree::new(id),
                    focused: id,
                    last_area: Rect::ZERO,
                    color: None,
                    name: None,
                });
                self.active = self.tabs.len() - 1;
            }
            Err(e) => eprintln!("tessera: failed to open tab: {e}"),
        }
    }

    /// Merge the dragged tab `src` into the tab containing `target_pane`,
    /// splitting that pane along `axis` (the dropped panes go after when `after`).
    /// The source tab's whole layout is spliced in and the source tab removed.
    fn merge_tab(&mut self, src: usize, target_pane: PaneId, axis: Axis, after: bool) {
        if src >= self.tabs.len() {
            return;
        }
        let Some(ti) = self.tabs.iter().position(|t| t.tree.contains(target_pane)) else {
            return;
        };
        if ti == src {
            return; // can't merge a tab into itself
        }
        let src_tab = self.tabs.remove(src);
        // Removing `src` shifts later indices down by one.
        let ti = if src < ti { ti - 1 } else { ti };
        self.tabs[ti]
            .tree
            .attach_subtree(target_pane, &src_tab.tree, axis, after);
        self.tabs[ti].focused = src_tab.focused;
        self.active = ti;
        // src_tab is dropped here; its backends live on in `self.panes`.
    }

    /// Move the tab at `src` so it lands at insertion slot `to` (expressed in
    /// terms of the list *before* removal). The moved tab becomes active.
    fn reorder_tab(&mut self, src: usize, to: usize) {
        match reorder_index(self.tabs.len(), src, to) {
            Some(insert_at) => {
                let tab = self.tabs.remove(src);
                self.tabs.insert(insert_at, tab);
                self.active = insert_at;
            }
            None => {
                if src < self.tabs.len() {
                    self.active = src; // dropped back where it started
                }
            }
        }
    }

    /// Move pane `src` so it sits as a split of `target` on the chosen side,
    /// within the active tab (re-tiling, iTerm2-style). Both panes live in the
    /// active tab; `src` is detached from its current spot (collapsing its old
    /// sibling) and re-inserted next to `target`.
    fn move_pane(&mut self, src: PaneId, target: PaneId, axis: Axis, after: bool) {
        if src == target {
            return;
        }
        let tab = &mut self.tabs[self.active];
        if !tab.tree.contains(src) || !tab.tree.contains(target) {
            return;
        }
        if !tab.tree.close(src) {
            return; // src was the only pane - nothing to move
        }
        tab.tree.split(target, src, axis, after);
        tab.focused = src;
    }

    /// Tear pane `src` out of the active tab into its own new tab, inserted at
    /// strip slot `insert_at`. The new tab becomes active; the source tab keeps
    /// its remaining panes (the grip is only offered when it has 2+).
    fn tear_pane_to_tab(&mut self, src: PaneId, insert_at: usize) {
        let active = self.active;
        if !self.tabs[active].tree.contains(src) {
            return;
        }
        let next = self.tabs[active].tree.focus_after_close(src);
        if !self.tabs[active].tree.close(src) {
            return; // src is the tab's only pane - tearing it out is a no-op
        }
        // Keep the source tab's focus pointing at a pane it still owns.
        if self.tabs[active].focused == src {
            let fallback = self.tabs[active].tree.first_pane();
            self.tabs[active].focused = next
                .filter(|p| self.panes.contains_key(p))
                .unwrap_or(fallback);
        }
        let at = insert_at.min(self.tabs.len());
        self.tabs.insert(
            at,
            Tab {
                tree: Tree::new(src),
                focused: src,
                last_area: Rect::ZERO,
                color: None,
                name: None,
            },
        );
        self.active = at;
    }

    /// Split the active tab's focused pane along `axis`.
    fn split(&mut self, axis: Axis, ctx: &egui::Context) {
        match self.spawn_pane(ctx) {
            Ok(id) => {
                let tab = &mut self.tabs[self.active];
                tab.tree.split(tab.focused, id, axis, true);
                tab.focused = id;
            }
            Err(e) => eprintln!("tessera: failed to spawn pane: {e}"),
        }
    }

    fn close_pane(&mut self, pane: PaneId, ctx: &egui::Context) {
        if !self.panes.contains_key(&pane) {
            return; // already gone (e.g. duplicate Exit + ChildExit)
        }
        if self.search.as_ref().is_some_and(|s| s.pane == pane) {
            self.search = None; // its pane is going away
        }
        let Some(ti) = self.tabs.iter().position(|t| t.tree.contains(pane)) else {
            self.panes.remove(&pane);
            return;
        };

        let next = self.tabs[ti].tree.focus_after_close(pane);
        let removed = self.tabs[ti].tree.close(pane);
        // Dropping the backend sends Shutdown to its PTY loop, killing the shell.
        self.panes.remove(&pane);

        if !removed {
            // That was the tab's last pane - drop the whole tab.
            self.tabs.remove(ti);
            if self.tabs.is_empty() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            if self.active >= self.tabs.len() {
                self.active = self.tabs.len() - 1;
            } else if self.active > ti {
                self.active -= 1;
            }
        } else if self.tabs[ti].focused == pane {
            let fallback = self.tabs[ti].tree.first_pane();
            let nf = next
                .filter(|p| self.panes.contains_key(p))
                .unwrap_or(fallback);
            self.tabs[ti].focused = nf;
        }
    }

    /// Files dropped onto the window (e.g. a screenshot dragged from the
    /// desktop or Finder) paste their shell-quoted paths into the focused
    /// pane, like iTerm2 - TUIs such as Claude Code attach images from a
    /// pasted path. Wrapped in bracketed-paste markers when the app asks for
    /// them (mode 2004).
    fn paste_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let Some(pane) = self.panes.get_mut(&tab.focused) else {
            return;
        };
        let text = dropped
            .iter()
            .filter_map(|f| f.path.as_ref().and_then(|p| p.to_str()))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            return;
        }
        let bracketed = pane
            .backend
            .last_content()
            .terminal_mode
            .contains(TerminalMode::BRACKETED_PASTE);
        let bytes = if bracketed {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        };
        pane.backend.process_command(BackendCommand::Write(bytes));
    }

    /// Close a whole tab: every pane in its tree (dropping a backend kills its
    /// shell), then the tab itself - and the window when it was the last tab.
    fn close_tab(&mut self, ti: usize, ctx: &egui::Context) {
        if ti >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ti);
        for pane in tab.tree.panes_in_order() {
            if self.search.as_ref().is_some_and(|s| s.pane == pane) {
                self.search = None;
            }
            self.panes.remove(&pane);
        }
        if self.tabs.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if self.active > ti {
            self.active -= 1;
        }
    }

    /// Pull terminal output / control events off the PTY channel.
    fn drain_pty_events(&mut self, ctx: &egui::Context) {
        let mut to_close = Vec::new();
        while let Ok((id, event)) = self.pty_rx.try_recv() {
            match event {
                PtyEvent::Title(t) => {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.title = t;
                    }
                }
                PtyEvent::ResetTitle => {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.title = self.default_title.clone();
                    }
                }
                PtyEvent::PtyWrite(text) => {
                    if let Some(p) = self.panes.get_mut(&id) {
                        p.backend
                            .process_command(BackendCommand::Write(text.into_bytes()));
                    }
                }
                PtyEvent::Exit | PtyEvent::ChildExit(_) => to_close.push(id),
                _ => {}
            }
            // Any output for the pane means the shell has redrawn after a Cmd+K
            // (Ctrl+L) - now drop the scrollback that redraw scrolled off, so the
            // cleared lines can't be pulled back by resizing the window.
            if let Some(p) = self.panes.get_mut(&id) {
                p.backend.finish_clear();
            }
        }
        for id in to_close {
            self.close_pane(id, ctx);
        }
    }

    /// Intercept multiplexer shortcuts before terminals see the key events.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let cmd_alt = Modifiers::COMMAND | Modifiers::ALT;
        let kb = self.keybinds.clone();

        let hit = |mods: Modifiers, key: Key| -> bool {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(mods, key)))
        };

        // New tab (rebindable); jump to tab N by number (fixed).
        if consume_keyspec(ctx, kb.new_tab) {
            self.new_tab(ctx);
        }
        const NUM_KEYS: [Key; 9] = [
            Key::Num1,
            Key::Num2,
            Key::Num3,
            Key::Num4,
            Key::Num5,
            Key::Num6,
            Key::Num7,
            Key::Num8,
            Key::Num9,
        ];
        for (i, key) in NUM_KEYS.iter().enumerate() {
            if hit(cmd, *key) && i < self.tabs.len() {
                self.active = i;
            }
        }

        // Option+number: focus pane N within the active tab.
        let mut switched_pane = false;
        for (i, key) in NUM_KEYS.iter().enumerate() {
            if hit(Modifiers::ALT, *key) {
                let order = self.tabs[self.active].tree.panes_in_order();
                if let Some(&pane) = order.get(i) {
                    self.tabs[self.active].focused = pane;
                }
                switched_pane = true;
            }
        }
        if switched_pane {
            // On macOS, Option+digit composes a character (e.g. "¡"). The Key
            // event was consumed above, but egui also queues a Text event for the
            // composed char - drop it so the shortcut doesn't type into the shell.
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(_))));
        }

        // Splits (rebindable). Exact modifier matching keeps split-down
        // (default Cmd+Shift+D) and split-right (default Cmd+D) from clobbering
        // each other the way egui's lenient shortcut matching would.
        if consume_keyspec(ctx, kb.split_down) {
            self.split(Axis::Vertical, ctx); // top / bottom
        }
        if consume_keyspec(ctx, kb.split_right) {
            self.split(Axis::Horizontal, ctx); // side by side
        }
        // Close focused pane (collapses/removes its tab when it was the last).
        if consume_keyspec(ctx, kb.close_pane) {
            let pane = self.tabs[self.active].focused;
            self.close_pane(pane, ctx);
        }
        // Clear the focused pane's scrollback + screen (iTerm2-style Cmd+K).
        if consume_keyspec(ctx, kb.clear) {
            let pane = self.tabs[self.active].focused;
            if let Some(p) = self.panes.get_mut(&pane) {
                p.backend.clear();
            }
        }
        // Open scrollback search on the focused pane.
        if consume_keyspec(ctx, kb.find) {
            let pane = self.tabs[self.active].focused;
            self.search = Some(Search {
                pane,
                query: String::new(),
                terminal_focused: false,
                matches: (0, 0),
            });
        }

        // Directional navigation within the active tab.
        let nav = [
            (Key::ArrowLeft, Dir::Left),
            (Key::ArrowRight, Dir::Right),
            (Key::ArrowUp, Dir::Up),
            (Key::ArrowDown, Dir::Down),
        ];
        for (key, dir) in nav {
            if hit(cmd_alt, key) {
                let a = self.active;
                let (leaves, _) = self.tabs[a].tree.geometry(self.tabs[a].last_area);
                if let Some(p) = neighbor(&leaves, self.tabs[a].focused, dir) {
                    self.tabs[a].focused = p;
                }
            }
        }
    }

    fn draw_tab_strip(&mut self, ctx: &egui::Context) {
        let mut switch_to: Option<usize> = None;
        let mut open_new = false;
        let mut open_settings = false;
        let mut set_color: Option<(usize, Option<Color32>)> = None;
        let mut start_edit: Option<(usize, String)> = None;
        let mut close_tab_click: Option<usize> = None;
        let mut pending_reorder: Option<(usize, usize)> = None;
        // A pane torn out of its tab and dropped on the strip → (pane, slot).
        let mut pending_tear: Option<(PaneId, usize)> = None;
        // Top of the pane area from last frame = bottom of the top chrome (tab
        // strip + status bar). A torn-out pane dropped anywhere in that chrome
        // becomes a tab, so the gap between the two bars isn't a dead zone you
        // can release into and have nothing happen.
        let chrome_bottom = self
            .tabs
            .get(self.active)
            .map(|t| t.last_area.top())
            .unwrap_or(0.0);
        // The pane being dragged by its grip, tracked in app state. Unlike the
        // egui drag-and-drop payload, this reliably survives the cursor crossing
        // from the pane area up into the top chrome - so the tear keys off it, not
        // the payload (which gets dropped at the panel boundary).
        let grip_pane = self.pane_grip_down;
        let strip = egui::Frame::default()
            .fill(self.bar_bg)
            .inner_margin(Margin::symmetric(8, 7));
        egui::TopBottomPanel::top("tabs")
            .frame(strip)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let radius = CornerRadius::same(TAB_RADIUS);
                    // Rects of each tab in order, for the reorder insertion indicator.
                    let mut tab_rects: Vec<Rect> = Vec::new();
                    // The tab currently being torn out (if any) - its slot is drawn as
                    // a faint gap, since the tab itself floats under the cursor.
                    let dragged_src =
                        egui::DragAndDrop::payload::<TabDrag>(ui.ctx()).map(|d| d.src);

                    for (i, tab) in self.tabs.iter().enumerate() {
                        let selected = i == self.active;
                        // Default label is the shell name (e.g. "zsh"); a custom name
                        // from the rename popup overrides it. A fresh tab always shows
                        // "N  zsh" - names never carry over to a new tab.
                        let display = tab.name.as_deref().unwrap_or(self.default_title.as_str());

                        let text_color = if selected {
                            Color32::WHITE
                        } else {
                            Color32::from_gray(165)
                        };
                        // Custom-drawn so one widget can click (switch), double-click
                        // (rename) and drag (tear out), while keeping the rounded look.
                        let galley = ui.painter().layout_no_wrap(
                            format!("{}  {}", i + 1, truncate(display, 24)),
                            FontId::proportional(14.0),
                            text_color,
                        );
                        let width = (galley.size().x + 36.0).max(TAB_MIN_W);
                        let (rect, resp) = ui
                            .allocate_exact_size(egui::vec2(width, 30.0), Sense::click_and_drag());
                        tab_rects.push(rect);
                        if dragged_src == Some(i) {
                            // This tab is lifted out and floating under the cursor;
                            // leave a faint recessed gap where it normally sits.
                            ui.painter()
                                .rect_filled(rect, radius, Color32::from_black_alpha(45));
                            ui.painter().rect_stroke(
                                rect,
                                radius,
                                Stroke::new(1.0, Color32::from_white_alpha(16)),
                                StrokeKind::Inside,
                            );
                        } else {
                            // Raw rect hit-test, not resp.hovered(): the close
                            // button sits on top and would un-hover the tab.
                            let tab_hovered = ui.rect_contains_pointer(rect);
                            let fill = if selected {
                                TAB_SEL
                            } else if tab_hovered {
                                TAB_HOVER
                            } else {
                                TAB_IDLE
                            };
                            ui.painter().rect_filled(rect, radius, fill);
                            if let Some(c) = tab.color {
                                // Tint the whole tab and add a solid colour bar along the
                                // bottom, so the colour reads whether or not it's active.
                                let a = if selected { 120 } else { 70 };
                                ui.painter().rect_filled(
                                    rect,
                                    radius,
                                    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a),
                                );
                                let bar = Rect::from_min_max(
                                    pos2(rect.left() + 8.0, rect.bottom() - 4.0),
                                    pos2(rect.right() - 8.0, rect.bottom() - 2.0),
                                );
                                ui.painter().rect_filled(bar, CornerRadius::same(1), c);
                            }
                            ui.painter().galley(
                                rect.center() - galley.size() * 0.5,
                                galley,
                                text_color,
                            );

                            // iTerm2-style hover close: an × on the tab's left
                            // edge while the pointer is over the tab. Registered
                            // after the tab response, so it wins the click.
                            if tab_hovered
                                && dragged_src.is_none()
                                && grip_pane.is_none()
                                && self.editing.is_none()
                            {
                                let close_rect = Rect::from_center_size(
                                    pos2(rect.left() + 15.0, rect.center().y),
                                    egui::vec2(16.0, 16.0),
                                );
                                let cr =
                                    ui.interact(close_rect, resp.id.with("close"), Sense::click());
                                let fg = if cr.hovered() {
                                    ui.painter().rect_filled(
                                        close_rect,
                                        CornerRadius::same(4),
                                        Color32::from_white_alpha(32),
                                    );
                                    Color32::WHITE
                                } else {
                                    Color32::from_gray(150)
                                };
                                let x = ui.painter().layout_no_wrap(
                                    "×".to_string(),
                                    FontId::monospace(13.0),
                                    fg,
                                );
                                ui.painter()
                                    .galley(close_rect.center() - x.size() * 0.5, x, fg);
                                if cr.clicked() {
                                    close_tab_click = Some(i);
                                }
                            }
                        }

                        if resp.double_clicked() {
                            start_edit = Some((i, display.to_string()));
                        } else if resp.clicked() {
                            switch_to = Some(i);
                        }
                        resp.dnd_set_drag_payload(TabDrag { src: i });
                        // Spring-loaded tabs (iTerm2-style): while dragging a tab,
                        // hovering a *different* tab activates it, so its panes become
                        // the drop targets and you can drop the dragged tab in there.
                        if let Some(drag) = resp.dnd_hover_payload::<TabDrag>() {
                            if drag.src != i {
                                switch_to = Some(i);
                            }
                        }
                        // Right-click → tab colour (presets, custom picker, or clear).
                        resp.context_menu(|ui| {
                            ui.label("Tab colour");
                            ui.horizontal(|ui| {
                                for (name, col) in TAB_PRESETS {
                                    let (sw, r) = ui.allocate_exact_size(
                                        egui::vec2(22.0, 22.0),
                                        Sense::click(),
                                    );
                                    ui.painter().rect_filled(sw, CornerRadius::same(5), *col);
                                    if r.hovered() {
                                        ui.painter().rect_stroke(
                                            sw,
                                            CornerRadius::same(5),
                                            Stroke::new(1.5, Color32::WHITE),
                                            StrokeKind::Inside,
                                        );
                                    }
                                    if r.on_hover_text(*name).clicked() {
                                        set_color = Some((i, Some(*col)));
                                        ui.close_menu();
                                    }
                                }
                            });
                            let mut custom = tab.color.unwrap_or(Color32::from_rgb(90, 150, 235));
                            ui.horizontal(|ui| {
                                ui.label("Custom:");
                                if ui.color_edit_button_srgba(&mut custom).changed() {
                                    set_color = Some((i, Some(custom)));
                                }
                            });
                            if ui.button("Clear colour").clicked() {
                                set_color = Some((i, None));
                                ui.close_menu();
                            }
                        });
                    }

                    // "+" new-tab button.
                    let plus = ui.painter().layout_no_wrap(
                        "+".to_string(),
                        FontId::proportional(18.0),
                        Color32::from_gray(200),
                    );
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(34.0, 30.0), Sense::click());
                    let fill = if resp.hovered() { TAB_HOVER } else { TAB_IDLE };
                    ui.painter().rect_filled(rect, radius, fill);
                    ui.painter().galley(
                        rect.center() - plus.size() * 0.5,
                        plus,
                        Color32::from_gray(200),
                    );
                    if resp.clicked() {
                        open_new = true;
                    }
                    resp.on_hover_text("New tab (Cmd+T)");

                    // Gear menu, pinned to the far right. The popup is themed
                    // globally (see configure_style) so it matches the terminal.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("⚙", |ui| {
                            ui.set_min_width(150.0);
                            if ui.button("Settings").clicked() {
                                open_settings = true;
                                ui.close_menu();
                            }
                        })
                        .response
                        .on_hover_text("Settings");
                    });

                    // While dragging a tab *or* a torn-out pane over the strip: show
                    // where it would land, and on release drop it there. Dropping
                    // anywhere in the strip works - between tabs, before the first, or
                    // after the last. A tab reorders; a pane becomes a new tab.
                    //
                    // A tab drag is tracked by the egui drag-payload; a pane drag is
                    // tracked by `grip_pane` (app state), which - unlike a drag-payload
                    // - survives the cursor crossing out of the central panel into this
                    // chrome, so a release up here actually registers.
                    let tab_payload = egui::DragAndDrop::has_payload_of_type::<TabDrag>(ui.ctx());
                    if tab_payload || grip_pane.is_some() {
                        if let (Some(p), Some(first)) =
                            (ui.ctx().pointer_latest_pos(), tab_rects.first().copied())
                        {
                            // A dragged tab only reorders when released on the strip
                            // itself; a torn-out pane can land anywhere in the top
                            // chrome (strip *and* the status bar below it), so the gap
                            // between the bars isn't a dead zone.
                            let in_strip = p.y >= first.top() - 8.0 && p.y <= first.bottom() + 8.0;
                            let in_chrome =
                                grip_pane.is_some() && chrome_bottom > 1.0 && p.y < chrome_bottom;
                            if in_strip || in_chrome {
                                // Insertion slot = number of tabs whose centre is left
                                // of the cursor (0 = before the first tab).
                                let insert = tab_rects
                                    .iter()
                                    .position(|r| p.x < r.center().x)
                                    .unwrap_or(tab_rects.len());
                                let x = if insert == 0 {
                                    tab_rects[0].left() - 3.0
                                } else if insert < tab_rects.len() {
                                    (tab_rects[insert - 1].right() + tab_rects[insert].left()) * 0.5
                                } else {
                                    tab_rects[tab_rects.len() - 1].right() + 3.0
                                };
                                ui.painter().line_segment(
                                    [pos2(x, first.top()), pos2(x, first.bottom())],
                                    Stroke::new(2.5, ACCENT),
                                );
                                // Released over the strip → reorder the dragged tab, or
                                // tear the dragged pane out into a new tab here.
                                if ui.input(|i| i.pointer.any_released()) {
                                    if let Some(drag) =
                                        egui::DragAndDrop::take_payload::<TabDrag>(ui.ctx())
                                    {
                                        pending_reorder = Some((drag.src, insert));
                                    } else if let Some(pane) = grip_pane {
                                        pending_tear = Some((pane, insert));
                                    }
                                }
                            }
                        }
                    }
                });
            });
        if open_new {
            self.new_tab(ctx);
        }
        if open_settings {
            open_config();
        }
        if let Some((i, c)) = set_color {
            if let Some(tab) = self.tabs.get_mut(i) {
                tab.color = c;
            }
        }
        // Reorder/tear win over spring-load's transient active change; their
        // indices are still valid because colour above ran before the list changed.
        // A close can't coincide with a drag release or another click.
        if let Some(ti) = close_tab_click {
            self.close_tab(ti, ctx);
        } else if let Some((pane, insert)) = pending_tear {
            self.tear_pane_to_tab(pane, insert);
        } else if let Some((src, to)) = pending_reorder {
            self.reorder_tab(src, to);
        } else if let Some(i) = switch_to {
            self.active = i;
        }
        if let Some((idx, buf)) = start_edit {
            self.editing = Some(Editing {
                idx,
                buf,
                focused: false,
            });
        }
    }

    /// The rename popup. While it's open the panes are drawn unfocused, so the
    /// terminal can't grab keyboard focus and typing goes to the text field.
    fn draw_rename_modal(&mut self, ctx: &egui::Context) {
        let mut captured: Option<(usize, String)> = None;
        let mut result: Option<bool> = None; // Some(true)=confirm, Some(false)=cancel
                                             // Tie the popup's accent to the tab being edited (its colour, if set).
        let accent = self
            .editing
            .as_ref()
            .and_then(|e| self.tabs.get(e.idx))
            .and_then(|t| t.color)
            .unwrap_or(ACCENT);
        let card = elevate(self.bar_bg, 10);
        if let Some(ed) = &mut self.editing {
            let resp = egui::Modal::new(Id::new("tessera_rename"))
                .backdrop_color(Color32::from_black_alpha(130))
                .frame(card_frame(card))
                .show(ctx, |ui| {
                    ui.set_width(300.0);
                    ui.label(
                        egui::RichText::new("Rename tab")
                            .strong()
                            .size(15.0)
                            .color(Color32::from_gray(236)),
                    );
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new("Leave empty to restore the default name")
                            .size(11.5)
                            .color(Color32::from_gray(140)),
                    );
                    ui.add_space(12.0);
                    let te = styled_field(
                        ui,
                        &mut ed.buf,
                        "Tab name",
                        accent,
                        f32::INFINITY,
                        Color32::from_gray(236),
                    );
                    if !ed.focused {
                        te.request_focus();
                        ed.focused = true;
                    }
                    let submit = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.add_space(16.0);
                    let mut action: Option<bool> = None;
                    // Right-aligned, primary ("Rename") on the right per macOS.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 8.0;
                        if pill_button(ui, "Rename", accent, Color32::WHITE).clicked() {
                            action = Some(true);
                        }
                        if pill_button(
                            ui,
                            "Cancel",
                            Color32::from_white_alpha(14),
                            Color32::from_gray(215),
                        )
                        .clicked()
                        {
                            action = Some(false);
                        }
                    });
                    if submit {
                        action = Some(true);
                    }
                    action
                });
            captured = Some((ed.idx, ed.buf.clone()));
            // Backdrop click or Escape cancels.
            result = if resp.should_close() {
                Some(false)
            } else {
                resp.inner
            };
        }
        if let Some(confirm) = result {
            if confirm {
                if let Some((idx, buf)) = captured {
                    let name = buf.trim().to_string();
                    if let Some(tab) = self.tabs.get_mut(idx) {
                        tab.name = (!name.is_empty()).then_some(name);
                    }
                }
            }
            self.editing = None;
        }
    }

    /// The quit confirmation (Cmd+Q / the window's close button). While it's
    /// open the panes are drawn unfocused, so Return and Escape land in the
    /// dialog instead of a shell.
    fn draw_quit_modal(&mut self, ctx: &egui::Context) {
        if !self.confirm_quit {
            return;
        }
        let shells = self.panes.len();
        let tabs = self.tabs.len();
        let detail = if shells == 1 {
            "The open shell will be terminated".to_string()
        } else if tabs == 1 {
            format!("All {shells} open shells will be terminated")
        } else {
            format!("All {shells} open shells across {tabs} tabs will be terminated")
        };
        let resp = egui::Modal::new(Id::new("tessera_quit"))
            .backdrop_color(Color32::from_black_alpha(130))
            .frame(card_frame(elevate(self.bar_bg, 10)))
            .show(ctx, |ui| {
                ui.set_width(300.0);
                ui.label(
                    egui::RichText::new("Quit Tessera?")
                        .strong()
                        .size(15.0)
                        .color(Color32::from_gray(236)),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(detail)
                        .size(11.5)
                        .color(Color32::from_gray(140)),
                );
                ui.add_space(16.0);
                let mut action: Option<bool> = None;
                // Right-aligned, primary ("Quit") on the right per macOS.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    if pill_button(ui, "Quit", DANGER, Color32::WHITE).clicked() {
                        action = Some(true);
                    }
                    if pill_button(
                        ui,
                        "Cancel",
                        Color32::from_white_alpha(14),
                        Color32::from_gray(215),
                    )
                    .clicked()
                    {
                        action = Some(false);
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    action = Some(true);
                }
                action
            });
        // Backdrop click or Escape cancels.
        let action = if resp.should_close() {
            Some(false)
        } else {
            resp.inner
        };
        match action {
            Some(true) => {
                // Let the follow-up close request through; the dialog stays up
                // for the teardown frames so it doesn't flicker away first.
                self.quit_confirmed = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(false) => self.confirm_quit = false,
            None => {}
        }
    }

    /// Cmd+F scrollback search bar, floating at the top-right over the pane.
    fn draw_search_bar(&mut self, ctx: &egui::Context) {
        let Some(pane) = self.search.as_ref().map(|s| s.pane) else {
            return;
        };
        // Close if the searched pane went away or isn't in the visible tab.
        if !self.panes.contains_key(&pane) || !self.tabs[self.active].tree.contains(pane) {
            if let Some(p) = self.panes.get_mut(&pane) {
                p.backend.clear_search();
            }
            self.search = None;
            return;
        }

        let mut action: Option<(bool, bool)> = None; // (forward, reset)
        let mut close = false;
        let card = elevate(self.bar_bg, 8);
        // Tie the focus ring / counter highlight to the active tab's accent.
        let accent = self
            .tabs
            .get(self.active)
            .and_then(|t| t.color)
            .unwrap_or(ACCENT);
        {
            let search = self.search.as_mut().unwrap();
            egui::Area::new(Id::new("tessera_search"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-14.0, 84.0))
                .show(ctx, |ui| {
                    card_frame(card).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let no_matches = search.matches.1 == 0 && !search.query.is_empty();
                            let field_color = if no_matches {
                                Color32::from_rgb(235, 130, 130)
                            } else {
                                Color32::from_gray(235)
                            };
                            let te = styled_field(
                                ui,
                                &mut search.query,
                                "Find in scrollback",
                                accent,
                                190.0,
                                field_color,
                            );
                            // The field owns focus unless the user clicked the
                            // terminal; clicking back into the field reclaims it.
                            if !search.terminal_focused && !te.has_focus() {
                                te.request_focus();
                            }
                            if te.gained_focus() {
                                search.terminal_focused = false;
                            }
                            if te.changed() {
                                action = Some((false, true)); // new query → recollect matches
                            }
                            let (enter, shift, esc) = ui.input(|i| {
                                (
                                    i.key_pressed(Key::Enter),
                                    i.modifiers.shift,
                                    i.key_pressed(Key::Escape),
                                )
                            });
                            // Step matches on Enter (older) / Shift+Enter (newer)
                            // while the search field owns the keyboard. Gating on
                            // `te.lost_focus()` doesn't work here: a singleline
                            // TextEdit only surrenders focus on *plain* Enter (so
                            // Shift+Enter would never register), and even that is
                            // immediately undone by the auto-refocus above, so the
                            // field never reports losing focus.
                            if enter && !search.terminal_focused {
                                action = Some((shift, false));
                                te.request_focus();
                            }
                            // "3 / 20" style match counter.
                            let (cur, total) = search.matches;
                            let counter = if search.query.is_empty() {
                                String::new()
                            } else {
                                format!("{cur} / {total}")
                            };
                            ui.add_space(2.0);
                            ui.label(egui::RichText::new(counter).size(12.5).color(
                                if no_matches {
                                    Color32::from_rgb(235, 130, 130)
                                } else {
                                    Color32::from_gray(165)
                                },
                            ));
                            ui.add_space(2.0);
                            // Step through matches / close, as compact icon buttons.
                            if icon_button(ui, "▲", "Previous match (older) - Enter").clicked() {
                                action = Some((false, false));
                            }
                            if icon_button(ui, "▼", "Next match (newer) - Shift+Enter").clicked()
                            {
                                action = Some((true, false));
                            }
                            // Esc closes only when the field has focus; when the
                            // terminal is focused, Esc belongs to the shell (vim etc).
                            if icon_button(ui, "×", "Close - Esc").clicked()
                                || (esc && !search.terminal_focused)
                            {
                                close = true;
                            }
                        });
                    });
                });
        }

        if close {
            if let Some(p) = self.panes.get_mut(&pane) {
                p.backend.clear_search();
            }
            self.search = None;
            ctx.request_repaint();
            return;
        }
        if let Some((forward, reset)) = action {
            let query = self
                .search
                .as_ref()
                .map(|s| s.query.clone())
                .unwrap_or_default();
            let counts = self
                .panes
                .get_mut(&pane)
                .map(|p| p.backend.search(&query, forward, reset))
                .unwrap_or((0, 0));
            if let Some(s) = self.search.as_mut() {
                s.matches = counts;
            }
            ctx.request_repaint();
        }
    }
}

impl eframe::App for Tessera {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_pty_events(ctx);
        // A close request (Cmd+Q via the rerouted Quit menu item, or the
        // window's close button) would quit the whole app: hold it and ask
        // first. It passes through once the dialog confirms, or when the last
        // pane already closed itself (`tabs` empty = the shells are gone, so
        // there's nothing to protect).
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && !self.tabs.is_empty()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.confirm_quit = true;
        }
        // While the rename or quit popup is open, shortcuts are frozen too.
        if self.editing.is_none() && !self.confirm_quit {
            self.handle_shortcuts(ctx);
        }
        // Closing the last pane (Cmd+W, or `exit` / Ctrl-D in the last shell)
        // empties `tabs` and queues ViewportCommand::Close. That Close is only
        // processed *after* update() returns, so bail out of the rest of this
        // frame now - otherwise the code below would index self.tabs[active] on
        // an empty Vec and panic on the way out.
        if self.tabs.is_empty() {
            return;
        }
        self.paste_dropped_files(ctx);
        self.draw_tab_strip(ctx);

        // The active tab's colour (if set) tints the accent UI; else default blue.
        let accent = self
            .tabs
            .get(self.active)
            .and_then(|t| t.color)
            .unwrap_or(ACCENT);

        // Theme-derived surfaces, copied out before the panel closures borrow self.
        let bar_bg = self.bar_bg;
        let window_bg = self.window_bg;
        let term_bg = self.term_bg;
        let pad = self.pad;

        // Status / hint bar (active tab's focused pane + shortcut hints).
        let status = egui::Frame::default()
            .fill(bar_bg)
            .inner_margin(Margin::symmetric(10, 5));
        egui::TopBottomPanel::top("status").frame(status).show(ctx, |ui| {
            ui.horizontal(|ui| {
                let title = self
                    .tabs
                    .get(self.active)
                    .and_then(|t| t.name.clone())
                    .unwrap_or_else(|| self.default_title.clone());
                ui.label(
                    egui::RichText::new(format!("▌ {title}"))
                        .color(accent)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            "Cmd+D split right   Cmd+Shift+D split down   Cmd+T tab   Cmd+1-9 tab   Opt+1-9 pane   Cmd+W close   Cmd+F find   Cmd+Opt+arrows move",
                        )
                        .color(Color32::from_gray(120))
                        .size(12.0),
                    );
                });
            });
        });

        let active = self.active;
        let focused = self.tabs[active].focused;
        let theme = self.theme.clone();
        let font = self.font.clone();
        // The rename and quit dialogs fully freeze pane focus. Search shares focus: by
        // default the search field owns it, but once you click a pane the
        // terminal takes over so you can keep typing while search stays open.
        let modal_open = self.editing.is_some() || self.confirm_quit;
        let searching = self.search.is_some();
        let search_on_terminal = self.search.as_ref().is_some_and(|s| s.terminal_focused);
        let panes_focusable = !modal_open && (!searching || search_on_terminal);

        let frame = egui::Frame::default()
            .fill(window_bg)
            .inner_margin(Margin::same(GUTTER));

        let mut clicked: Option<PaneId> = None;
        let mut close_pane_click: Option<PaneId> = None;
        let mut ratio_updates: Vec<(usize, f32)> = Vec::new();
        let mut pending_drop: Option<(usize, PaneId, Axis, bool)> = None;
        // A pane dragged onto another pane in this tab → (src, target, axis, after).
        let mut pending_pane_drop: Option<(PaneId, PaneId, Axis, bool)> = None;
        // Which grip the pointer is holding (carried over from last frame). Kept
        // alive across the whole gesture so the drag survives the cursor leaving
        // the pane; written back to self after the panel closes.
        let mut grip_down = self.pane_grip_down;

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let area = ui.max_rect();
            self.tabs[active].last_area = area;
            let (leaves, dividers) = self.tabs[active].tree.geometry(area);
            let radius = CornerRadius::same(PANE_RADIUS);
            // The pane currently being dragged out by its grip, if any (this
            // frame's snapshot of the app-state signal - see `pane_grip_down`).
            // Its source terminal must not also start a text selection meanwhile.
            let pane_drag_src = grip_down;
            let multi = leaves.len() > 1;

            // 1) Draw each pane as a rounded card with padded text inside, plus a
            //    hover grip you can drag to re-tile the pane or tear it into a tab.
            for (pane_id, rect) in &leaves {
                // Rounded card background; the terminal (same bg colour) sits inset
                // by PANE_PAD so glyphs aren't flush against the edges.
                ui.painter().rect_filled(*rect, radius, term_bg);
                let inner = rect.shrink2(pad);

                // Hover-grip hit area, hugging the pane's top edge. The visible
                // handle is much slimmer (drawn below); this stays a bit larger so
                // it's still easy to grab.
                let grip_rect = Rect::from_center_size(
                    pos2(rect.center().x, rect.top() + 8.0),
                    egui::vec2(40.0, 14.0),
                );
                // Hover-close hit area in the pane's top-right corner (iTerm2-
                // style: hidden until the pane is hovered).
                let close_rect = Rect::from_center_size(
                    pos2(rect.right() - 15.0, rect.top() + 13.0),
                    egui::vec2(18.0, 18.0),
                );
                let ptr = ui.ctx().pointer_latest_pos();
                let pane_hovered = ptr.is_some_and(|p| rect.contains(p));
                let dragging_this = pane_drag_src == Some(*pane_id);
                let over_grip = multi && ptr.is_some_and(|p| grip_rect.contains(p));
                let over_close = ptr.is_some_and(|p| close_rect.contains(p));
                let suppress_mouse = over_grip || over_close || dragging_this;

                let Some(pane) = self.panes.get_mut(pane_id) else {
                    continue;
                };
                let resp = ui
                    .allocate_new_ui(UiBuilder::new().max_rect(inner), |ui| {
                        let view = TerminalView::new(ui, &mut pane.backend)
                            .set_focus(*pane_id == focused && panes_focusable)
                            .set_pointer_input(!suppress_mouse)
                            .set_theme(theme.clone())
                            .set_font(font.clone())
                            .set_size(inner.size());
                        ui.add(view)
                    })
                    .inner;
                if resp.clicked() {
                    clicked = Some(*pane_id);
                }

                // The grip: only when the tab has 2+ panes (otherwise there's
                // nothing to re-tile, and the whole tab is already draggable). It
                // fades in on hover and stays live for the whole drag - once the
                // press is recorded, `dragging_this` (driven by app state) stays
                // true even after the cursor leaves the pane, so the grip doesn't
                // drop mid-gesture.
                let want_grip =
                    multi && ((pane_hovered && pane_drag_src.is_none()) || dragging_this);
                let vis = ui.ctx().animate_bool_with_time(
                    Id::new(("tessera_grip_vis", active, pane_id)),
                    want_grip,
                    0.12,
                );
                if want_grip {
                    let g = ui.interact(
                        grip_rect,
                        Id::new(("tessera_pane_grip", active, pane_id)),
                        Sense::click_and_drag(),
                    );
                    // Record the held grip from the press onward (true while the
                    // button is down on it, even when dragging outside the pane).
                    // This is the one signal the whole pane drag keys off.
                    if g.is_pointer_button_down_on() {
                        grip_down = Some(*pane_id);
                    }
                    if g.clicked() {
                        clicked = Some(*pane_id);
                    }
                    if g.dragged() || g.drag_started() {
                        ui.ctx().set_cursor_icon(CursorIcon::Grabbing);
                    } else if g.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::Grab);
                    }
                }
                if vis > 0.003 {
                    // Just a slim handle bar hugging the top edge - no plate.
                    let bar = Rect::from_center_size(
                        pos2(rect.center().x, rect.top() + 6.0),
                        egui::vec2(26.0, 4.0),
                    );
                    ui.painter().rect_filled(
                        bar,
                        CornerRadius::same(2),
                        Color32::from_white_alpha((vis * 160.0) as u8),
                    );
                }

                // Hover ×: closes this pane, same as Cmd+W on it. Fades in with
                // the pane hover like the grip; hidden during any drag.
                let tab_dragging = egui::DragAndDrop::has_payload_of_type::<TabDrag>(ui.ctx());
                let want_close = pane_hovered
                    && pane_drag_src.is_none()
                    && !tab_dragging
                    && self.editing.is_none();
                let close_vis = ui.ctx().animate_bool_with_time(
                    Id::new(("tessera_close_vis", active, pane_id)),
                    want_close,
                    0.12,
                );
                if want_close {
                    let c = ui.interact(
                        close_rect,
                        Id::new(("tessera_pane_close", active, pane_id)),
                        Sense::click(),
                    );
                    if c.clicked() {
                        close_pane_click = Some(*pane_id);
                    }
                    if c.hovered() {
                        ui.ctx().set_cursor_icon(CursorIcon::Default);
                    }
                }
                if close_vis > 0.003 {
                    if over_close && pane_drag_src.is_none() {
                        ui.painter().rect_filled(
                            close_rect,
                            CornerRadius::same(5),
                            Color32::from_white_alpha(28),
                        );
                    }
                    let alpha = (close_vis * if over_close { 230.0 } else { 150.0 }) as u8;
                    let fg = Color32::from_white_alpha(alpha);
                    let x =
                        ui.painter()
                            .layout_no_wrap("×".to_string(), FontId::monospace(14.0), fg);
                    ui.painter()
                        .galley(close_rect.center() - x.size() * 0.5, x, fg);
                }
            }

            // 2) Draggable dividers: the gap shows the window background, with a
            //    subtle rounded grip in the centre that lights up on hover/drag.
            for div in &dividers {
                let id = Id::new(("tessera_divider", active, div.node));
                let resp = ui.interact(div.rect, id, Sense::drag());
                let hot = resp.hovered() || resp.dragged();
                if hot {
                    ctx.set_cursor_icon(match div.axis {
                        Axis::Horizontal => CursorIcon::ResizeHorizontal,
                        Axis::Vertical => CursorIcon::ResizeVertical,
                    });
                }
                let grip = match div.axis {
                    Axis::Horizontal => Rect::from_center_size(
                        div.rect.center(),
                        egui::vec2(4.0, (div.rect.height() - 24.0).max(12.0)),
                    ),
                    Axis::Vertical => Rect::from_center_size(
                        div.rect.center(),
                        egui::vec2((div.rect.width() - 24.0).max(12.0), 4.0),
                    ),
                };
                ui.painter().rect_filled(
                    grip,
                    CornerRadius::same(2),
                    if hot { DIV_HOT } else { DIV_IDLE },
                );
                if resp.dragged() && div.avail > 1.0 {
                    let delta = resp.drag_delta();
                    let along = match div.axis {
                        Axis::Horizontal => delta.x,
                        Axis::Vertical => delta.y,
                    };
                    ratio_updates.push((div.node, div.ratio + along / div.avail));
                }
            }

            // 3) Rounded accent border around the focused pane, painted on top.
            if let Some((_, rect)) = leaves.iter().find(|(p, _)| *p == focused) {
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    radius,
                    Stroke::new(1.5, accent),
                    StrokeKind::Inside,
                );
            }

            // 4) Tab drag-and-drop: while a tab is being dragged, every pane is a
            //    drop target. The half of the pane nearest the cursor previews
            //    where the dropped tab will land, and a drop merges it in there.
            if egui::DragAndDrop::has_payload_of_type::<TabDrag>(ui.ctx()) {
                for (pane_id, rect) in &leaves {
                    let dz = ui.interact(
                        *rect,
                        Id::new(("tessera_drop", active, pane_id)),
                        Sense::hover(),
                    );
                    let hovering = dz.dnd_hover_payload::<TabDrag>().is_some();
                    let released = dz.dnd_release_payload::<TabDrag>();
                    if hovering || released.is_some() {
                        let pos = ui.ctx().pointer_interact_pos().unwrap_or(rect.center());
                        let (axis, after) = drop_side(*rect, pos);
                        ui.painter().rect_filled(
                            drop_half(*rect, axis, after),
                            radius,
                            Color32::from_rgba_unmultiplied(102, 161, 255, 70),
                        );
                        if let Some(drag) = released {
                            pending_drop = Some((drag.src, *pane_id, axis, after));
                        }
                    }
                }
            }

            // 4b) Pane drag-and-drop within the tab: dragging a pane by its grip
            //     and dropping it on another pane re-tiles it onto the chosen half.
            //     A plain hit-test on the grip-press state (mirroring the tear in
            //     draw_tab_strip), so it can't desync from it. Source isn't a target.
            if let Some(src) = pane_drag_src {
                let ptr = ui.ctx().pointer_latest_pos();
                let released = ui.input(|i| i.pointer.any_released());
                for (pane_id, rect) in &leaves {
                    if *pane_id == src {
                        continue;
                    }
                    if ptr.is_some_and(|p| rect.contains(p)) {
                        let pos = ptr.unwrap_or_else(|| rect.center());
                        let (axis, after) = drop_side(*rect, pos);
                        ui.painter().rect_filled(
                            drop_half(*rect, axis, after),
                            radius,
                            Color32::from_rgba_unmultiplied(102, 161, 255, 70),
                        );
                        if released {
                            pending_pane_drop = Some((src, *pane_id, axis, after));
                        }
                    }
                }
            }
        });

        // The grip drag ends the moment no pointer button is held - clear it then
        // (covers normal release and any release we might not have observed, e.g.
        // the window losing focus mid-drag).
        if !ctx.input(|i| i.pointer.any_down()) {
            grip_down = None;
        }
        self.pane_grip_down = grip_down;

        if let Some(p) = clicked {
            self.tabs[active].focused = p;
            // Clicking a pane during search hands keyboard focus to the terminal.
            if let Some(s) = self.search.as_mut() {
                s.terminal_focused = true;
            }
        }
        for (node, ratio) in ratio_updates {
            self.tabs[active].tree.set_ratio(node, ratio);
        }
        if let Some((src, target_pane, axis, after)) = pending_drop {
            self.merge_tab(src, target_pane, axis, after);
        }
        if let Some((src, tgt, axis, after)) = pending_pane_drop {
            self.move_pane(src, tgt, axis, after);
        }
        // A pane's hover × closes it; a click can't coincide with the drag/drop
        // actions above. Closing the last pane queues the window close - bail
        // out of the frame then, like the Cmd+W path does.
        if let Some(p) = close_pane_click {
            self.close_pane(p, ctx);
            if self.tabs.is_empty() {
                return;
            }
        }

        // Floating, translucent copy of the dragged tab that follows the cursor -
        // iTerm2-style "lift", so it's obvious the tab is grabbed and droppable.
        // On pick-up it eases up (grows slightly + casts a shadow); the slot it
        // came from shows a faint gap (see draw_tab_strip). The body is kept
        // semi-transparent so the content behind it shows through.
        let dragging = egui::DragAndDrop::payload::<TabDrag>(ctx).map(|d| d.src);
        let lift =
            ctx.animate_bool_with_time(Id::new("tessera_tab_lift"), dragging.is_some(), 0.12);
        if let (Some(src), Some(pos)) = (dragging, ctx.pointer_latest_pos()) {
            if let Some(tab) = self.tabs.get(src) {
                let raw = tab.name.as_deref().unwrap_or(self.default_title.as_str());
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    Id::new("tessera_tab_drag_preview"),
                ));
                // Rebuild the tab at its real size, so it reads as the same tab
                // lifted off the strip rather than a generic chip.
                let galley = painter.layout_no_wrap(
                    format!("{}  {}", src + 1, truncate(raw, 24)),
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );
                let width = (galley.size().x + 36.0).max(TAB_MIN_W);
                let size = Vec2::new(width, 30.0) * (1.0 + 0.04 * lift);
                let rect = Rect::from_center_size(pos, size);
                let radius = CornerRadius::same(TAB_RADIUS);

                // Soft drop shadow, fading in as the tab lifts.
                if lift > 0.0 {
                    painter.rect_filled(
                        rect.translate(Vec2::new(0.0, 4.0 * lift)).expand(1.0),
                        radius,
                        Color32::from_black_alpha((70.0 * lift) as u8),
                    );
                }
                // Translucent body - you can see what's behind it.
                painter.rect_filled(
                    rect,
                    radius,
                    Color32::from_rgba_unmultiplied(TAB_SEL.r(), TAB_SEL.g(), TAB_SEL.b(), 170),
                );
                if let Some(c) = tab.color {
                    painter.rect_filled(
                        rect,
                        radius,
                        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 110),
                    );
                    let bar = Rect::from_min_max(
                        pos2(rect.left() + 8.0, rect.bottom() - 4.0),
                        pos2(rect.right() - 8.0, rect.bottom() - 2.0),
                    );
                    painter.rect_filled(bar, CornerRadius::same(1), c);
                }
                // Thin highlight outline so it stands off the background.
                painter.rect_stroke(
                    rect,
                    radius,
                    Stroke::new(1.0, Color32::from_white_alpha(40)),
                    StrokeKind::Inside,
                );
                painter.galley(
                    rect.center() - galley.size() * 0.5,
                    galley,
                    Color32::from_white_alpha(235),
                );
            }
        }

        // Floating, translucent card while a pane is dragged out by its grip -
        // same "lift" treatment as a dragged tab, labelled with the pane's title.
        // Keyed off the grip-press state so it tracks the cursor the whole way,
        // including up onto the tab bar; gated on a real drag so a plain grip
        // click doesn't flash a card.
        let pane_dragging = self
            .pane_grip_down
            .filter(|_| ctx.input(|i| i.pointer.is_decidedly_dragging()));
        let plift =
            ctx.animate_bool_with_time(Id::new("tessera_pane_lift"), pane_dragging.is_some(), 0.12);
        if let (Some(pid), Some(pos)) = (pane_dragging, ctx.pointer_latest_pos()) {
            let title = self
                .panes
                .get(&pid)
                .map(|p| p.title.clone())
                .unwrap_or_else(|| self.default_title.clone());
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                Id::new("tessera_pane_drag_preview"),
            ));
            let galley = painter.layout_no_wrap(
                truncate(&title, 24),
                FontId::proportional(13.5),
                Color32::WHITE,
            );
            let size = (galley.size() + Vec2::new(28.0, 16.0)) * (1.0 + 0.04 * plift);
            let rect = Rect::from_center_size(pos, size);
            let radius = CornerRadius::same(8);
            if plift > 0.0 {
                painter.rect_filled(
                    rect.translate(Vec2::new(0.0, 4.0 * plift)).expand(1.0),
                    radius,
                    Color32::from_black_alpha((70.0 * plift) as u8),
                );
            }
            painter.rect_filled(
                rect,
                radius,
                Color32::from_rgba_unmultiplied(TAB_SEL.r(), TAB_SEL.g(), TAB_SEL.b(), 180),
            );
            painter.rect_stroke(
                rect,
                radius,
                Stroke::new(1.0, Color32::from_white_alpha(45)),
                StrokeKind::Inside,
            );
            painter.galley(
                rect.center() - galley.size() * 0.5,
                galley,
                Color32::from_white_alpha(235),
            );
        }

        // Scrollback search bar (floats over the focused pane).
        self.draw_search_bar(ctx);
        // Rename popup on top of everything (panes are unfocused while it's open).
        self.draw_rename_modal(ctx);
        // Quit confirmation, same treatment.
        self.draw_quit_modal(ctx);
    }
}

fn shell_basename(shell: &str) -> String {
    shell
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(shell)
        .to_string()
}

/// Name of the egui font family holding the bold terminal face. Only
/// registered when a true bold face exists (see `configure_fonts`).
pub const BOLD_FAMILY: &str = "mono-bold";

/// Register fonts: the user's `font-family` (if any) as the primary monospace
/// face, plus a Nerd Font symbols fallback so prompt icons and powerline glyphs
/// (which most monospace fonts lack) still render. Also registers a bold
/// monospace family for cells with the bold attribute - the user font's bold
/// face when it ships one, or the bundled Hack Bold to match the default Hack.
/// Returns whether that bold family was registered; when it wasn't (a user
/// font without a true bold), the terminal synthesises bold instead.
fn configure_fonts(
    ctx: &egui::Context,
    user_font: Option<(Vec<u8>, u32)>,
    user_bold_font: Option<(Vec<u8>, u32)>,
) -> bool {
    use egui::{FontData, FontFamily};
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "nerd_symbols".to_owned(),
        std::sync::Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/SymbolsNerdFontMono-Regular.ttf"
        ))),
    );
    // A configured font is loaded from disk and put *first* in the Monospace
    // family, so the terminal (which renders with FontId::monospace) uses it.
    let has_user_font = user_font.is_some();
    if let Some((bytes, index)) = user_font {
        let mut data = FontData::from_owned(bytes);
        data.index = index; // pick the right face out of a .ttc collection
        fonts
            .font_data
            .insert("user_mono".to_owned(), std::sync::Arc::new(data));
        fonts
            .families
            .entry(FontFamily::Monospace)
            .or_default()
            .insert(0, "user_mono".to_owned());
    }
    let bold_data = match (has_user_font, user_bold_font) {
        // The configured font's own bold face.
        (true, Some((bytes, index))) => {
            let mut data = FontData::from_owned(bytes);
            data.index = index;
            Some(data)
        }
        // Configured font without a true bold: leave the family unregistered
        // (mixing another typeface's bold in would look worse than synthesis).
        (true, None) => None,
        // Default font: bundle the matching Hack Bold.
        (false, _) => Some(FontData::from_static(include_bytes!(
            "../assets/fonts/Hack-Bold.ttf"
        ))),
    };
    let has_bold = bold_data.is_some();
    if let Some(data) = bold_data {
        fonts
            .font_data
            .insert("mono_bold".to_owned(), std::sync::Arc::new(data));
        fonts.families.insert(
            FontFamily::Name(BOLD_FAMILY.into()),
            vec!["mono_bold".to_owned(), "nerd_symbols".to_owned()],
        );
    }
    // Append the symbols font as a last-resort fallback in both families (the
    // primary font is tried first; missing glyphs fall through to the symbols).
    for family in [FontFamily::Monospace, FontFamily::Proportional] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("nerd_symbols".to_owned());
    }
    ctx.set_fonts(fonts);
    has_bold
}

/// Theme egui's popups (the gear "Settings" menu, the right-click tab menu,
/// tooltips) to match the terminal, instead of the default near-black grey.
/// egui builds the root menu frame from the *global* style, so this has to be
/// set on the context, not on a local Ui.
fn configure_style(ctx: &egui::Context, card: Color32) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    // Surfaces shared by menus and tooltips.
    v.window_fill = card;
    v.window_stroke = Stroke::new(1.0, Color32::from_white_alpha(22));
    v.menu_corner_radius = CornerRadius::same(12);
    v.popup_shadow = egui::epaint::Shadow {
        offset: [0, 10],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(140),
    };
    // Menu rows: frameless at rest, a soft rounded highlight on hover.
    v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.inactive.fg_stroke.color = Color32::from_gray(205);
    v.widgets.hovered.weak_bg_fill = Color32::from_white_alpha(18);
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.hovered.fg_stroke.color = Color32::from_gray(240);
    v.widgets.active.weak_bg_fill = Color32::from_white_alpha(26);
    v.widgets.active.bg_stroke = Stroke::NONE;
    v.widgets.inactive.corner_radius = CornerRadius::same(7);
    v.widgets.hovered.corner_radius = CornerRadius::same(7);
    v.widgets.active.corner_radius = CornerRadius::same(7);
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    // Drop the gear menu a touch lower so its body floats clearly over the pane
    // instead of grazing the focused-pane border right below the bars.
    style.spacing.menu_spacing = 16.0;
    ctx.set_style(style);
}

/// Scale a colour toward black by `factor` (0 = black, 1 = unchanged). Used to
/// derive the gutter and bar surfaces from the theme's background.
fn darken(c: Color32, factor: f32) -> Color32 {
    let f = |v: u8| (v as f32 * factor).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(f(c.r()), f(c.g()), f(c.b()))
}

/// Consume a key event that *exactly* matches `spec` (modifiers included), so it
/// triggers the action and stays hidden from the terminal. Exact matching -
/// unlike egui's lenient shortcut matching - lets Cmd+D and Cmd+Shift+D coexist.
fn consume_keyspec(ctx: &egui::Context, spec: KeySpec) -> bool {
    ctx.input_mut(|i| {
        let mut hit = false;
        i.events.retain(|e| match e {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } if *key == spec.key && modifiers.matches_exact(spec.mods) => {
                hit = true;
                false // consume it
            }
            _ => true,
        });
        // On macOS an Alt-modified shortcut also queues a composed Text event
        // (e.g. Alt+T -> "†"); drop it so it doesn't leak into the shell.
        if hit && spec.mods.alt {
            i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        }
        hit
    })
}

/// Nudge a colour a few steps lighter, to lift a surface above the bars.
fn elevate(c: Color32, amt: u8) -> Color32 {
    Color32::from_rgb(
        c.r().saturating_add(amt),
        c.g().saturating_add(amt),
        c.b().saturating_add(amt),
    )
}

/// A floating-card frame: elevated fill, hairline border, rounded corners,
/// generous padding, and a soft drop shadow - so the popups read as real
/// surfaces instead of flat default boxes.
fn card_frame(fill: Color32) -> egui::Frame {
    egui::Frame::default()
        .fill(fill)
        .stroke(Stroke::new(1.0, Color32::from_white_alpha(22)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .shadow(egui::epaint::Shadow {
            offset: [0, 10],
            blur: 28,
            spread: 0,
            color: Color32::from_black_alpha(140),
        })
}

/// A compact square icon button with a rounded hover/press highlight. The glyph
/// is drawn from the monospace family (Hack), which carries the arrow and ×
/// glyphs that the proportional UI font lacks.
fn icon_button(ui: &mut egui::Ui, glyph: &str, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), Sense::click());
    let bg = if resp.is_pointer_button_down_on() {
        Color32::from_white_alpha(28)
    } else if resp.hovered() {
        Color32::from_white_alpha(16)
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(7), bg);
    let fg = if resp.hovered() {
        Color32::from_gray(240)
    } else {
        Color32::from_gray(170)
    };
    let galley = ui
        .painter()
        .layout_no_wrap(glyph.to_owned(), FontId::monospace(14.0), fg);
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, fg);
    if tip.is_empty() {
        resp
    } else {
        resp.on_hover_text(tip)
    }
}

/// A rounded "pill" text button. `fill` / `text_color` pick a primary (accent)
/// or ghost (subtle) variant.
fn pill_button(
    ui: &mut egui::Ui,
    text: &str,
    fill: Color32,
    text_color: Color32,
) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(text).color(text_color).size(14.0))
            .fill(fill)
            .corner_radius(CornerRadius::same(8))
            .min_size(Vec2::new(92.0, 32.0)),
    )
}

/// A rounded, padded single-line text field with an accent focus ring. Visual
/// tweaks are scoped so they don't leak to sibling widgets.
fn styled_field(
    ui: &mut egui::Ui,
    buf: &mut String,
    hint: &str,
    accent: Color32,
    width: f32,
    text_color: Color32,
) -> egui::Response {
    ui.scope(|ui| {
        let v = ui.visuals_mut();
        v.extreme_bg_color = Color32::from_black_alpha(110);
        v.selection.bg_fill =
            Color32::from_rgba_unmultiplied(accent.r(), accent.g(), accent.b(), 90);
        v.selection.stroke = Stroke::new(1.0, accent);
        v.widgets.inactive.corner_radius = CornerRadius::same(8);
        v.widgets.hovered.corner_radius = CornerRadius::same(8);
        v.widgets.active.corner_radius = CornerRadius::same(8);
        v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_white_alpha(20));
        v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_white_alpha(35));
        v.widgets.active.bg_stroke = Stroke::new(1.5, accent);
        ui.add(
            egui::TextEdit::singleline(buf)
                .desired_width(width)
                .font(FontId::proportional(14.5))
                .margin(Margin::symmetric(10, 7))
                .hint_text(hint)
                .text_color(text_color),
        )
    })
    .inner
}

/// Open the user's config file in the default text editor, writing a commented
/// template there first if it doesn't exist yet. Changes apply on next launch.
fn open_config() {
    let Some(path) = crate::config::ensure_file() else {
        eprintln!("tessera: couldn't locate a config directory to open");
        return;
    };
    match std::process::Command::new("open")
        .arg("-t") // open in the default text editor, not "run" it
        .arg(&path)
        .spawn()
    {
        // Reap the short-lived `open` helper on a detached thread so it doesn't
        // linger as a zombie process for the rest of the session.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("tessera: couldn't open {}: {e}", path.display()),
    }
}

/// Directory new shells start in. Launched from a terminal we inherit the launch
/// directory; launched from the Dock/Finder the process cwd is "/", which is a
/// poor place to start a shell, so fall back to $HOME.
fn default_working_dir() -> Option<std::path::PathBuf> {
    use std::path::{Path, PathBuf};
    match std::env::current_dir() {
        Ok(cwd) if cwd != Path::new("/") => Some(cwd),
        _ => std::env::var_os("HOME").map(PathBuf::from),
    }
}

/// Decide which edge of `rect` the cursor is nearest, and translate that into a
/// split axis + whether the dropped pane goes after (right/below). Dropping near
/// the left edge puts it on the left, near the bottom puts it below, etc.
fn drop_side(rect: Rect, pos: Pos2) -> (Axis, bool) {
    let fx = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
    let fy = ((pos.y - rect.top()) / rect.height().max(1.0)).clamp(0.0, 1.0);
    let (left, right, top, bottom) = (fx, 1.0 - fx, fy, 1.0 - fy);
    let nearest = left.min(right).min(top).min(bottom);
    if nearest == left {
        (Axis::Horizontal, false)
    } else if nearest == right {
        (Axis::Horizontal, true)
    } else if nearest == top {
        (Axis::Vertical, false)
    } else {
        (Axis::Vertical, true)
    }
}

/// The half of `rect` a drop would occupy, for the preview highlight.
fn drop_half(rect: Rect, axis: Axis, after: bool) -> Rect {
    let c = rect.center();
    match (axis, after) {
        (Axis::Horizontal, false) => Rect::from_min_max(rect.min, pos2(c.x, rect.max.y)),
        (Axis::Horizontal, true) => Rect::from_min_max(pos2(c.x, rect.min.y), rect.max),
        (Axis::Vertical, false) => Rect::from_min_max(rect.min, pos2(rect.max.x, c.y)),
        (Axis::Vertical, true) => Rect::from_min_max(pos2(rect.min.x, c.y), rect.max),
    }
}

/// Shorten a title for the tab strip, appending an ellipsis when clipped.
/// Quote a filesystem path for the shell: single-quoted, with any embedded
/// single quotes closed, escaped, and reopened.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let kept: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{kept}…")
    }
}

/// Where a tab dragged from `src` should be re-inserted given an insertion slot
/// `to` (in terms of the list before removal). Returns `None` for a no-op move
/// (dropped back in its own slot). Pure so it can be unit-tested.
fn reorder_index(len: usize, src: usize, to: usize) -> Option<usize> {
    if src >= len {
        return None;
    }
    let to = to.min(len);
    let insert_at = if to > src { to - 1 } else { to };
    (insert_at != src).then_some(insert_at)
}

#[cfg(test)]
mod tests {
    use super::{reorder_index, shell_quote};

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("/tmp/plain.png"), "'/tmp/plain.png'");
        assert_eq!(
            shell_quote("/tmp/with space/img.png"),
            "'/tmp/with space/img.png'"
        );
        assert_eq!(shell_quote("/tmp/it's.png"), r"'/tmp/it'\''s.png'");
    }

    fn apply(list: &[u64], src: usize, to: usize) -> Vec<u64> {
        let mut v = list.to_vec();
        if let Some(at) = reorder_index(v.len(), src, to) {
            let x = v.remove(src);
            v.insert(at, x);
        }
        v
    }

    #[test]
    fn drag_tab3_to_front_makes_it_first() {
        // [1,2,3], drag tab 3 (idx 2) before tab 1 (slot 0) -> [3,1,2]
        assert_eq!(apply(&[1, 2, 3], 2, 0), vec![3, 1, 2]);
    }

    #[test]
    fn drag_tab2_to_end_makes_it_last() {
        // [1,2,3], drag tab 2 (idx 1) after tab 3 (slot 3) -> [1,3,2]
        assert_eq!(apply(&[1, 2, 3], 1, 3), vec![1, 3, 2]);
    }

    #[test]
    fn drop_in_same_slot_is_a_noop() {
        assert_eq!(reorder_index(3, 1, 1), None); // before itself
        assert_eq!(reorder_index(3, 1, 2), None); // after itself
        assert_eq!(apply(&[1, 2, 3], 1, 1), vec![1, 2, 3]);
    }
}
