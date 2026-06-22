//! The mockterm application: owns the pane tree, the terminal backends, and the
//! eframe update loop that renders panes, handles draggable dividers, routes
//! keyboard shortcuts, and drains PTY events.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui;
use egui::{
    Color32, CornerRadius, CursorIcon, FontId, Id, Key, KeyboardShortcut, Margin,
    Modifiers, Rect, Sense, Stroke, StrokeKind, UiBuilder,
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

pub struct MockTerm {
    tree: Tree,
    panes: HashMap<PaneId, Pane>,
    focused: PaneId,
    next_id: u64,
    pty_tx: Sender<(u64, PtyEvent)>,
    pty_rx: Receiver<(u64, PtyEvent)>,
    theme: TerminalTheme,
    font: TerminalFont,
    cfg: Config,
    default_title: String,
    /// Content rect of the last frame, used for spatial keyboard navigation.
    last_area: Rect,
}

const ACCENT: Color32 = Color32::from_rgb(102, 161, 255);
const DIV_IDLE: Color32 = Color32::from_rgb(38, 40, 48);
const DIV_HOT: Color32 = Color32::from_rgb(90, 120, 180);

impl MockTerm {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        let (pty_tx, pty_rx) = channel();
        let default_title = shell_basename(&cfg.shell);
        let mut app = Self {
            tree: Tree::new(0),
            panes: HashMap::new(),
            focused: 0,
            next_id: 0,
            pty_tx,
            pty_rx,
            theme: TerminalTheme::default(),
            font: TerminalFont::new(FontSettings {
                font_type: FontId::monospace(14.0),
            }),
            cfg,
            default_title,
            last_area: Rect::ZERO,
        };
        // First pane fills the window. If the shell can't spawn we can't do
        // anything useful, so fail loudly.
        let id = app
            .spawn_pane(&cc.egui_ctx)
            .expect("failed to spawn initial shell");
        app.tree = Tree::new(id);
        app.focused = id;
        app
    }

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
                working_directory: None,
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

    fn split(&mut self, axis: Axis, ctx: &egui::Context) {
        match self.spawn_pane(ctx) {
            Ok(id) => {
                self.tree.split(self.focused, id, axis, true);
                self.focused = id;
            }
            Err(e) => eprintln!("mockterm: failed to spawn pane: {e}"),
        }
    }

    fn close_pane(&mut self, pane: PaneId, ctx: &egui::Context) {
        if !self.panes.contains_key(&pane) {
            return; // already gone (e.g. duplicate Exit + ChildExit)
        }
        let next = self.tree.focus_after_close(pane);
        self.tree.close(pane);
        // Dropping the backend sends Shutdown to its PTY loop, killing the shell.
        self.panes.remove(&pane);

        if self.panes.is_empty() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.focused == pane {
            self.focused = next
                .filter(|p| self.panes.contains_key(p))
                .unwrap_or_else(|| *self.panes.keys().next().unwrap());
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

        // Split right (side-by-side) / split down (stacked) — iTerm2 semantics.
        if hit(cmd, Key::D) {
            self.split(Axis::Horizontal, ctx);
        }
        if hit(cmd_shift, Key::D) {
            self.split(Axis::Vertical, ctx);
        }
        // Close focused pane.
        if hit(cmd, Key::W) {
            self.close_pane(self.focused, ctx);
        }
        // Directional navigation (Cmd+Alt+Arrows).
        let nav = [
            (Key::ArrowLeft, Dir::Left),
            (Key::ArrowRight, Dir::Right),
            (Key::ArrowUp, Dir::Up),
            (Key::ArrowDown, Dir::Down),
        ];
        for (key, dir) in nav {
            if hit(cmd_alt, key) {
                let (leaves, _) = self.tree.geometry(self.last_area);
                if let Some(p) = neighbor(&leaves, self.focused, dir) {
                    self.focused = p;
                }
            }
        }
    }
}

impl eframe::App for MockTerm {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_pty_events(ctx);
        self.handle_shortcuts(ctx);

        // Status / hint bar.
        egui::TopBottomPanel::top("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let title = self
                    .panes
                    .get(&self.focused)
                    .map(|p| p.title.as_str())
                    .unwrap_or("mockterm");
                ui.label(
                    egui::RichText::new(format!("▌ {title}"))
                        .color(ACCENT)
                        .strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            "⌘D split right   ⌘⇧D split down   ⌘W close   ⌘⌥←→↑↓ move   drag borders to resize",
                        )
                        .color(Color32::from_gray(120))
                        .size(12.0),
                    );
                });
            });
        });

        let focused = self.focused;
        let theme = self.theme.clone();
        let font = self.font.clone();

        let frame = egui::Frame::default()
            .fill(Color32::from_rgb(16, 17, 21))
            .inner_margin(Margin::ZERO);

        let mut clicked: Option<PaneId> = None;
        let mut ratio_updates: Vec<(usize, f32)> = Vec::new();

        egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
            let area = ui.max_rect();
            self.last_area = area;
            let (leaves, dividers) = self.tree.geometry(area);

            // 1) Draw each pane's terminal.
            for (pane_id, rect) in &leaves {
                let Some(pane) = self.panes.get_mut(pane_id) else {
                    continue;
                };
                let resp = ui
                    .allocate_new_ui(UiBuilder::new().max_rect(*rect), |ui| {
                        let view = TerminalView::new(ui, &mut pane.backend)
                            .set_focus(*pane_id == focused)
                            .set_theme(theme.clone())
                            .set_font(font.clone())
                            .set_size(rect.size());
                        ui.add(view)
                    })
                    .inner;
                if resp.clicked() {
                    clicked = Some(*pane_id);
                }
            }

            // 2) Draggable dividers on top of the panes.
            for div in &dividers {
                let id = Id::new(("mockterm_divider", div.node));
                let resp = ui.interact(div.rect, id, Sense::drag());
                let hot = resp.hovered() || resp.dragged();
                if hot {
                    ctx.set_cursor_icon(match div.axis {
                        Axis::Horizontal => CursorIcon::ResizeHorizontal,
                        Axis::Vertical => CursorIcon::ResizeVertical,
                    });
                }
                ui.painter().rect_filled(
                    div.rect,
                    CornerRadius::ZERO,
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

            // 3) Accent border around the focused pane, painted last (on top).
            if let Some((_, rect)) = leaves.iter().find(|(p, _)| *p == focused) {
                ui.painter().rect_stroke(
                    rect.shrink(0.5),
                    CornerRadius::ZERO,
                    Stroke::new(1.5, ACCENT),
                    StrokeKind::Inside,
                );
            }
        });

        if let Some(p) = clicked {
            self.focused = p;
        }
        for (node, ratio) in ratio_updates {
            self.tree.set_ratio(node, ratio);
        }
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
