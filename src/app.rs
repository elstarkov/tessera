//! The mockterm application: owns the tabs (each a pane tree), the terminal
//! backends, and the eframe update loop that renders the active tab's panes,
//! handles draggable dividers, routes keyboard shortcuts, and drains PTY events.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui;
use egui::{
    pos2, Color32, CornerRadius, CursorIcon, FontId, Id, Key, KeyboardShortcut, Margin,
    Modifiers, Pos2, Rect, Sense, Stroke, StrokeKind, UiBuilder,
};
use egui_term::{
    BackendCommand, BackendSettings, FontSettings, PtyEvent, TerminalBackend,
    TerminalFont, TerminalTheme, TerminalView,
};

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

pub struct MockTerm {
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
}

const ACCENT: Color32 = Color32::from_rgb(102, 161, 255);
const DIV_IDLE: Color32 = Color32::from_rgb(60, 62, 72);
const DIV_HOT: Color32 = Color32::from_rgb(120, 150, 210);

// Surfaces.
const TERM_BG: Color32 = Color32::from_rgb(0x18, 0x18, 0x18); // matches egui_term theme bg
const WINDOW_BG: Color32 = Color32::from_rgb(12, 12, 14); // gutter behind the panes
const BAR_BG: Color32 = Color32::from_rgb(20, 21, 26); // tab strip + status bar
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
const PANE_PAD: f32 = 8.0; // breathing room between the card edge and the text
const GUTTER: i8 = 8; // outer margin between the window edge and the panes
const TAB_RADIUS: u8 = 8;
const TAB_MIN_W: f32 = 150.0; // minimum tab width, so tabs feel roomy

impl MockTerm {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        let (pty_tx, pty_rx) = channel();
        let default_title = shell_basename(&cfg.shell);
        let mut app = Self {
            tabs: Vec::new(),
            active: 0,
            panes: HashMap::new(),
            next_id: 0,
            pty_tx,
            pty_rx,
            theme: TerminalTheme::default(),
            font: TerminalFont::new(FontSettings {
                font_type: FontId::monospace(14.0),
            }),
            cfg,
            default_title,
            editing: None,
        };
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
            Err(e) => eprintln!("mockterm: failed to open tab: {e}"),
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

    /// Split the active tab's focused pane along `axis`.
    fn split(&mut self, axis: Axis, ctx: &egui::Context) {
        match self.spawn_pane(ctx) {
            Ok(id) => {
                let tab = &mut self.tabs[self.active];
                tab.tree.split(tab.focused, id, axis, true);
                tab.focused = id;
            }
            Err(e) => eprintln!("mockterm: failed to spawn pane: {e}"),
        }
    }

    fn close_pane(&mut self, pane: PaneId, ctx: &egui::Context) {
        if !self.panes.contains_key(&pane) {
            return; // already gone (e.g. duplicate Exit + ChildExit)
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
            // That was the tab's last pane — drop the whole tab.
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
        }
        for id in to_close {
            self.close_pane(id, ctx);
        }
    }

    /// Intercept multiplexer shortcuts before terminals see the key events.
    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let cmd_alt = Modifiers::COMMAND | Modifiers::ALT;

        let hit = |mods: Modifiers, key: Key| -> bool {
            ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(mods, key)))
        };

        // Tabs: new tab, and jump to tab N by number.
        if hit(cmd, Key::T) {
            self.new_tab(ctx);
        }
        const NUM_KEYS: [Key; 9] = [
            Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6,
            Key::Num7, Key::Num8, Key::Num9,
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
            // composed char — drop it so the shortcut doesn't type into the shell.
            ctx.input_mut(|i| i.events.retain(|e| !matches!(e, egui::Event::Text(_))));
        }

        // Splits.
        if hit(cmd, Key::D) {
            self.split(Axis::Horizontal, ctx);
        }
        if hit(cmd_shift, Key::D) {
            self.split(Axis::Vertical, ctx);
        }
        // Close focused pane (collapses/removes its tab when it was the last).
        if hit(cmd, Key::W) {
            let pane = self.tabs[self.active].focused;
            self.close_pane(pane, ctx);
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
        let mut set_color: Option<(usize, Option<Color32>)> = None;
        let mut start_edit: Option<(usize, String)> = None;
        let mut pending_reorder: Option<(usize, usize)> = None;
        let strip = egui::Frame::default()
            .fill(BAR_BG)
            .inner_margin(Margin::symmetric(8, 7));
        egui::TopBottomPanel::top("tabs").frame(strip).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let radius = CornerRadius::same(TAB_RADIUS);
                // Rects of each tab in order, for the reorder insertion indicator.
                let mut tab_rects: Vec<Rect> = Vec::new();

                for (i, tab) in self.tabs.iter().enumerate() {
                    let selected = i == self.active;
                    // Default label is the shell name (e.g. "zsh"); a custom name
                    // from the rename popup overrides it. A fresh tab always shows
                    // "N  zsh" — names never carry over to a new tab.
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
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(width, 30.0), Sense::click_and_drag());
                    tab_rects.push(rect);
                    let fill = if selected {
                        TAB_SEL
                    } else if resp.hovered() {
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
                    ui.painter()
                        .galley(rect.center() - galley.size() * 0.5, galley, text_color);

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
                                let (sw, r) = ui
                                    .allocate_exact_size(egui::vec2(22.0, 22.0), Sense::click());
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
                ui.painter()
                    .galley(rect.center() - plus.size() * 0.5, plus, Color32::from_gray(200));
                if resp.clicked() {
                    open_new = true;
                }
                resp.on_hover_text("New tab (Cmd+T)");

                // While dragging a tab over the strip: show where it would land,
                // and on release reorder it there. Dropping anywhere in the strip
                // works — between tabs, before the first, or after the last.
                if egui::DragAndDrop::has_payload_of_type::<TabDrag>(ui.ctx()) {
                    if let (Some(p), Some(first)) =
                        (ui.ctx().pointer_latest_pos(), tab_rects.first().copied())
                    {
                        if p.y >= first.top() - 8.0 && p.y <= first.bottom() + 8.0 {
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
                            // Released over the strip → take the payload (so the
                            // content-area merge handler won't also fire) and reorder.
                            if ui.input(|i| i.pointer.any_released()) {
                                if let Some(drag) =
                                    egui::DragAndDrop::take_payload::<TabDrag>(ui.ctx())
                                {
                                    pending_reorder = Some((drag.src, insert));
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
        if let Some((i, c)) = set_color {
            if let Some(tab) = self.tabs.get_mut(i) {
                tab.color = c;
            }
        }
        // Reorder wins over spring-load's transient active change; its indices are
        // still valid because colour above ran before the list changed.
        if let Some((src, to)) = pending_reorder {
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
        if let Some(ed) = &mut self.editing {
            let resp = egui::Modal::new(Id::new("mockterm_rename")).show(ctx, |ui| {
                ui.set_width(280.0);
                ui.label(egui::RichText::new("Rename tab").strong().size(15.0));
                ui.add_space(8.0);
                let te = ui.add(
                    egui::TextEdit::singleline(&mut ed.buf)
                        .desired_width(f32::INFINITY)
                        .font(FontId::proportional(15.0))
                        .hint_text("Tab name (empty = default)"),
                );
                if !ed.focused {
                    te.request_focus();
                    ed.focused = true;
                }
                let submit = te.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(12.0);
                let mut action: Option<bool> = None;
                ui.horizontal(|ui| {
                    if ui.button("Confirm").clicked() {
                        action = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
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
}

impl eframe::App for MockTerm {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_pty_events(ctx);
        // While the rename popup is open, shortcuts are frozen too.
        if self.editing.is_none() {
            self.handle_shortcuts(ctx);
        }
        self.draw_tab_strip(ctx);

        // The active tab's colour (if set) tints the accent UI; else default blue.
        let accent = self
            .tabs
            .get(self.active)
            .and_then(|t| t.color)
            .unwrap_or(ACCENT);

        // Status / hint bar (active tab's focused pane + shortcut hints).
        let status = egui::Frame::default()
            .fill(BAR_BG)
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
                            "⌘T tab  ⌘1-9 tab  ⌥1-9 pane  ⌘D split→  ⌘⇧D split↓  ⌘W close  ⌘⌥←→↑↓ move  drag borders",
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
        // While renaming, no pane is focused so the terminal can't take keyboard.
        let renaming = self.editing.is_some();

        let frame = egui::Frame::default()
            .fill(WINDOW_BG)
            .inner_margin(Margin::same(GUTTER));

        let mut clicked: Option<PaneId> = None;
        let mut ratio_updates: Vec<(usize, f32)> = Vec::new();
        let mut pending_drop: Option<(usize, PaneId, Axis, bool)> = None;

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let area = ui.max_rect();
            self.tabs[active].last_area = area;
            let (leaves, dividers) = self.tabs[active].tree.geometry(area);
            let radius = CornerRadius::same(PANE_RADIUS);

            // 1) Draw each pane as a rounded card with padded text inside.
            for (pane_id, rect) in &leaves {
                // Rounded card background; the terminal (same bg colour) sits inset
                // by PANE_PAD so glyphs aren't flush against the edges.
                ui.painter().rect_filled(*rect, radius, TERM_BG);
                let inner = rect.shrink(PANE_PAD);
                let Some(pane) = self.panes.get_mut(pane_id) else {
                    continue;
                };
                let resp = ui
                    .allocate_new_ui(UiBuilder::new().max_rect(inner), |ui| {
                        let view = TerminalView::new(ui, &mut pane.backend)
                            .set_focus(*pane_id == focused && !renaming)
                            .set_theme(theme.clone())
                            .set_font(font.clone())
                            .set_size(inner.size());
                        ui.add(view)
                    })
                    .inner;
                if resp.clicked() {
                    clicked = Some(*pane_id);
                }
            }

            // 2) Draggable dividers: the gap shows the window background, with a
            //    subtle rounded grip in the centre that lights up on hover/drag.
            for div in &dividers {
                let id = Id::new(("mockterm_divider", active, div.node));
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
                        Id::new(("mockterm_drop", active, pane_id)),
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
        });

        if let Some(p) = clicked {
            self.tabs[active].focused = p;
        }
        for (node, ratio) in ratio_updates {
            self.tabs[active].tree.set_ratio(node, ratio);
        }
        if let Some((src, target_pane, axis, after)) = pending_drop {
            self.merge_tab(src, target_pane, axis, after);
        }

        // Floating chip that follows the cursor while dragging a tab.
        if let Some(drag) = egui::DragAndDrop::payload::<TabDrag>(ctx) {
            if let (Some(pos), Some(tab)) = (ctx.pointer_latest_pos(), self.tabs.get(drag.src)) {
                let raw = tab.name.as_deref().unwrap_or(self.default_title.as_str());
                let painter = ctx.layer_painter(egui::LayerId::new(
                    egui::Order::Tooltip,
                    Id::new("mockterm_tab_drag_preview"),
                ));
                let galley = painter.layout_no_wrap(
                    format!("{}  {}", drag.src + 1, truncate(raw, 24)),
                    FontId::proportional(14.0),
                    Color32::WHITE,
                );
                let rect = Rect::from_min_size(
                    pos + egui::vec2(12.0, 10.0),
                    galley.size() + egui::vec2(20.0, 12.0),
                );
                painter.rect_filled(rect, CornerRadius::same(TAB_RADIUS), TAB_SEL);
                painter.galley(rect.center() - galley.size() * 0.5, galley, Color32::WHITE);
            }
        }

        // Rename popup on top of everything (panes are unfocused while it's open).
        self.draw_rename_modal(ctx);
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
    use super::reorder_index;

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
