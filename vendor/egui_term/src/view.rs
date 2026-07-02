use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Point as TerminalGridPoint;
use alacritty_terminal::term::cell;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use egui::epaint::RectShape;
use egui::Modifiers;
use egui::MouseWheelUnit;
use egui::Shape;
use egui::Widget;
use egui::{Align2, Painter, Pos2, Rect, Response, Stroke, Vec2};
use egui::{CornerRadius, Key};
use egui::{Id, PointerButton};

use crate::backend::BackendCommand;
use crate::backend::TerminalBackend;
use crate::backend::{LinkAction, MouseButton, SelectionType};
use crate::bindings::Binding;
use crate::bindings::{BindingAction, BindingsLayout, InputKind};
use crate::font::TerminalFont;
use crate::theme::TerminalTheme;
use crate::types::Size;

const EGUI_TERM_WIDGET_ID_PREFIX: &str = "egui_term::instance::";

#[derive(Debug, Clone)]
enum InputAction {
    BackendCall(BackendCommand),
    WriteToClipboard(String),
    Ignore,
}

#[derive(Clone, Default)]
pub struct TerminalViewState {
    is_dragged: bool,
    scroll_pixels: f32,
    current_mouse_position_on_grid: TerminalGridPoint,
    /// `ctx.input().time` of the most recent scroll gesture, used to fade the
    /// auto-hiding scrollbars in and out (tessera). Defaults to 0.0, which reads
    /// as "scrolled long ago" so the bars start hidden.
    last_scroll_time: f64,
}

pub struct TerminalView<'a> {
    widget_id: Id,
    has_focus: bool,
    /// When false, the terminal ignores pointer input (clicks, selection, wheel)
    /// but still takes keyboard input if focused. Tessera uses this so a pane's
    /// hover grip can be grabbed without the terminal underneath starting a
    /// text selection at the same time.
    pointer_input: bool,
    size: Vec2,
    backend: &'a mut TerminalBackend,
    font: TerminalFont,
    theme: TerminalTheme,
    bindings_layout: BindingsLayout,
}

impl Widget for TerminalView<'_> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let (layout, painter) = ui.allocate_painter(self.size, egui::Sense::click());

        let widget_id = self.widget_id;
        let mut state = ui.memory(|m| {
            m.data
                .get_temp::<TerminalViewState>(widget_id)
                .unwrap_or_default()
        });

        self.focus(&layout)
            .resize(&layout)
            .process_input(&layout, &mut state)
            .show(&mut state, &layout, &painter);

        ui.memory_mut(|m| m.data.insert_temp(widget_id, state));
        layout
    }
}

impl<'a> TerminalView<'a> {
    pub fn new(ui: &mut egui::Ui, backend: &'a mut TerminalBackend) -> Self {
        let widget_id =
            ui.make_persistent_id(format!("{}{}", EGUI_TERM_WIDGET_ID_PREFIX, backend.id));

        Self {
            widget_id,
            has_focus: false,
            pointer_input: true,
            size: ui.available_size(),
            backend,
            font: TerminalFont::default(),
            theme: TerminalTheme::default(),
            bindings_layout: BindingsLayout::new(),
        }
    }

    #[inline]
    pub fn set_pointer_input(mut self, enabled: bool) -> Self {
        self.pointer_input = enabled;
        self
    }

    #[inline]
    pub fn set_theme(mut self, theme: TerminalTheme) -> Self {
        self.theme = theme;
        self
    }

    #[inline]
    pub fn set_font(mut self, font: TerminalFont) -> Self {
        self.font = font;
        self
    }

    #[inline]
    pub fn set_focus(mut self, has_focus: bool) -> Self {
        self.has_focus = has_focus;
        self
    }

    #[inline]
    pub fn set_size(mut self, size: Vec2) -> Self {
        self.size = size;
        self
    }

    #[inline]
    pub fn add_bindings(mut self, bindings: Vec<(Binding<InputKind>, BindingAction)>) -> Self {
        self.bindings_layout.add_bindings(bindings);
        self
    }

    fn focus(self, layout: &Response) -> Self {
        if self.has_focus {
            layout.request_focus();
        } else {
            layout.surrender_focus();
        }

        self
    }

    fn resize(self, layout: &Response) -> Self {
        self.backend.process_command(BackendCommand::Resize(
            Size::from(layout.rect.size()),
            self.font.font_measure(&layout.ctx),
        ));

        self
    }

    fn process_input(self, layout: &Response, state: &mut TerminalViewState) -> Self {
        // tessera patch: keyboard input follows the *focused* pane regardless
        // of pointer position, but mouse interaction (clicks, selection, wheel)
        // is still gated on the pointer being over this pane - otherwise a click
        // in another split would drive a selection in the focused one.
        let has_focus = layout.has_focus();
        let hovered = layout.contains_pointer();
        if !has_focus && !hovered {
            return self;
        }
        // Pointer interaction (selection, clicks, wheel) is additionally gated so
        // the pane's drag grip can be grabbed without selecting text underneath.
        let mouse_ok = hovered && self.pointer_input;

        let modifiers = layout.ctx.input(|i| i.modifiers);
        let events = layout.ctx.input(|i| i.events.clone());
        for event in events {
            let mut input_actions = vec![];

            match event {
                egui::Event::Text(_)
                | egui::Event::Key { .. }
                | egui::Event::Copy
                | egui::Event::Paste(_) => {
                    if has_focus {
                        input_actions.push(process_keyboard_event(
                            event,
                            self.backend,
                            &self.bindings_layout,
                            modifiers,
                        ))
                    }
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                } if mouse_ok => {
                    // A wheel gesture over this pane lights up the auto-hiding
                    // scrollbar - even if the scroll is clamped at the top/bottom,
                    // so it behaves the same as iTerm2 (tessera).
                    state.last_scroll_time = layout.ctx.input(|i| i.time);
                    input_actions.extend(process_mouse_wheel(
                        state,
                        self.backend,
                        self.font.font_type().size,
                        unit,
                        delta,
                        &modifiers,
                    ))
                }
                egui::Event::PointerButton {
                    button,
                    pressed,
                    modifiers,
                    pos,
                    ..
                } if mouse_ok => input_actions.push(process_button_click(
                    state,
                    layout,
                    self.backend,
                    &self.bindings_layout,
                    button,
                    pos,
                    &modifiers,
                    pressed,
                )),
                egui::Event::PointerMoved(pos) if mouse_ok => {
                    input_actions = process_mouse_move(state, layout, self.backend, pos, &modifiers)
                }
                _ => {}
            };

            for action in input_actions {
                match action {
                    InputAction::BackendCall(cmd) => {
                        self.backend.process_command(cmd);
                    }
                    InputAction::WriteToClipboard(data) => {
                        layout.ctx.copy_text(data);
                    }
                    InputAction::Ignore => {}
                }
            }
        }

        self
    }

    fn show(self, state: &mut TerminalViewState, layout: &Response, painter: &Painter) {
        let content = self.backend.sync();
        let layout_min = layout.rect.min;
        let layout_max = layout.rect.max;
        let cell_height = content.terminal_size.cell_height as f32;
        let cell_width = content.terminal_size.cell_width as f32;
        let global_bg = self.theme.get_color(Color::Named(NamedColor::Background));

        let mut shapes = vec![Shape::Rect(RectShape::filled(
            Rect::from_min_max(layout_min, layout_max),
            CornerRadius::ZERO,
            global_bg,
        ))];

        for indexed in content.grid.display_iter() {
            let flags = indexed.cell.flags;
            let is_wide_char_spacer = flags.contains(cell::Flags::WIDE_CHAR_SPACER);
            if is_wide_char_spacer {
                continue;
            }

            let is_app_cursor_mode = content.terminal_mode.contains(TermMode::APP_CURSOR);
            let is_wide_char = flags.contains(cell::Flags::WIDE_CHAR);
            let is_inverse = flags.contains(cell::Flags::INVERSE);
            // tessera patch: render the SGR style attributes TUIs build their
            // visual hierarchy from. DIM_BOLD is the composite DIM|BOLD, so
            // testing it with `intersects` would catch plain bold cells and dim
            // them - `contains(DIM)` alone covers both DIM and DIM_BOLD.
            let is_dim = flags.contains(cell::Flags::DIM);
            let is_bold = flags.contains(cell::Flags::BOLD);
            let is_italic = flags.contains(cell::Flags::ITALIC);
            let is_underline = flags.intersects(cell::Flags::ALL_UNDERLINES);
            let is_strikeout = flags.contains(cell::Flags::STRIKEOUT);
            let is_hidden = flags.contains(cell::Flags::HIDDEN);
            let is_selected = content
                .selectable_range
                .is_some_and(|r| r.contains(indexed.point));
            let is_hovered_hyperling = content.hovered_hyperlink.as_ref().is_some_and(|r| {
                r.contains(&indexed.point) && r.contains(&state.current_mouse_position_on_grid)
            });

            let x = layout_min.x + (cell_width * indexed.point.column.0 as f32);
            let line_num = indexed.point.line.0 + content.grid.display_offset() as i32;
            let y = layout_min.y + (cell_height * line_num as f32);

            // Bold promotes the base ANSI colours to their bright variants
            // ("bold-as-bright"), so bold text reads as such even in colours.
            let fg_color = if is_bold {
                match indexed.fg {
                    Color::Named(name) => Color::Named(name.to_bright()),
                    Color::Indexed(idx @ 0..=7) => Color::Indexed(idx + 8),
                    other => other,
                }
            } else {
                indexed.fg
            };
            let mut fg = self.theme.get_color(fg_color);
            let mut bg = self.theme.get_color(indexed.bg);
            let cell_width = if is_wide_char {
                cell_width * 2.0
            } else {
                cell_width
            };

            if is_dim {
                fg = fg.linear_multiply(0.7);
            }

            if is_inverse || is_selected {
                std::mem::swap(&mut fg, &mut bg);
            }

            if global_bg != bg {
                shapes.push(Shape::Rect(RectShape::filled(
                    Rect::from_min_size(
                        Pos2::new(x, y),
                        // + 1.0 is to fill grid border
                        Vec2::new(cell_width + 1., cell_height + 1.),
                    ),
                    CornerRadius::ZERO,
                    bg,
                )));
            }

            // Handle hovered hyperlink underline
            if is_hovered_hyperling {
                let underline_height = y + cell_height;
                shapes.push(Shape::LineSegment {
                    points: [
                        Pos2::new(x, underline_height),
                        Pos2::new(x + cell_width, underline_height),
                    ],
                    stroke: Stroke::new(cell_height * 0.15, fg).into(),
                });
            }

            // Underline / strikethrough attributes, drawn as segments (like the
            // hyperlink underline) so they also span blank cells.
            if is_underline && !is_hidden {
                let underline_y = y + cell_height * 0.92;
                shapes.push(Shape::LineSegment {
                    points: [
                        Pos2::new(x, underline_y),
                        Pos2::new(x + cell_width, underline_y),
                    ],
                    stroke: Stroke::new((cell_height * 0.06).max(1.0), fg).into(),
                });
            }
            if is_strikeout && !is_hidden {
                let strike_y = y + cell_height * 0.5;
                shapes.push(Shape::LineSegment {
                    points: [Pos2::new(x, strike_y), Pos2::new(x + cell_width, strike_y)],
                    stroke: Stroke::new((cell_height * 0.06).max(1.0), fg).into(),
                });
            }

            // Handle cursor rendering
            if content.grid.cursor.point == indexed.point {
                let cursor_color = self.theme.get_color(content.cursor.fg);
                shapes.push(Shape::Rect(RectShape::filled(
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(cell_width, cell_height)),
                    CornerRadius::default(),
                    cursor_color,
                )));
            }

            // Draw text content
            if !is_hidden && indexed.c != ' ' && indexed.c != '\t' {
                if content.grid.cursor.point == indexed.point && is_app_cursor_mode {
                    std::mem::swap(&mut fg, &mut bg);
                }

                let font_id = if is_bold {
                    self.font.bold_font_type()
                } else {
                    self.font.font_type()
                };

                if is_italic {
                    // No italic terminal face is registered; egui's faux
                    // italics (glyph skew) keeps the cell metrics intact.
                    let mut format = egui::TextFormat::simple(font_id, fg);
                    format.italics = true;
                    let mut job = egui::text::LayoutJob::default();
                    job.append(&indexed.c.to_string(), 0.0, format);
                    let galley = painter.fonts(|f| f.layout_job(job));
                    let glyph_width = galley.size().x;
                    shapes.push(Shape::galley(
                        Pos2::new(x + (cell_width - glyph_width) / 2.0, y),
                        galley,
                        fg,
                    ));
                } else {
                    let pos = Pos2 {
                        x: x + (cell_width / 2.0),
                        y,
                    };
                    shapes.push(Shape::text(
                        &painter.fonts(|c| c.clone()),
                        pos,
                        Align2::CENTER_TOP,
                        indexed.c,
                        font_id.clone(),
                        fg,
                    ));
                    // No dedicated bold face available: synthesise bold by
                    // double-striking the glyph half a pixel to the right.
                    if is_bold && font_id == self.font.font_type() {
                        shapes.push(Shape::text(
                            &painter.fonts(|c| c.clone()),
                            Pos2 { x: pos.x + 0.5, y },
                            Align2::CENTER_TOP,
                            indexed.c,
                            font_id,
                            fg,
                        ));
                    }
                }
            }
        }

        painter.extend(shapes);

        // tessera: iTerm2-style auto-hiding scrollbars, painted over the content.
        let fg = self.theme.get_color(Color::Named(NamedColor::Foreground));
        draw_scrollbars(
            state.last_scroll_time,
            layout,
            painter,
            fg,
            content.grid.total_lines(),
            content.grid.screen_lines(),
            content.grid.display_offset(),
            content.grid.columns(),
            cell_width,
        );
    }
}

/// iTerm2-style overlay scrollbars: a slim thumb that shows up only while you're
/// scrolling a scrollable pane and fades out shortly after you stop. Drawn on top
/// of the terminal content and non-interactive - a pure position indicator.
#[allow(clippy::too_many_arguments)]
fn draw_scrollbars(
    last_scroll_time: f64,
    layout: &Response,
    painter: &Painter,
    fg: egui::Color32,
    total_lines: usize,
    visible_lines: usize,
    v_offset: usize,
    columns: usize,
    cell_width: f32,
) {
    // Fade out almost as soon as scrolling stops. HOLD is kept just long enough
    // to bridge the gap between discrete wheel notches so the bar doesn't flicker
    // mid-scroll; FADE is a quick dissolve once you actually stop.
    const HOLD: f64 = 0.1; // stay fully opaque this long after the last scroll
    const FADE: f64 = 0.18; // then dissolve over this long
    const THICKNESS: f32 = 6.0;
    const MARGIN: f32 = 3.0;
    const MIN_THUMB: f32 = 24.0;

    let ctx = &layout.ctx;
    let now = ctx.input(|i| i.time);
    let since = now - last_scroll_time;
    let alpha = if since <= HOLD {
        1.0
    } else {
        (1.0 - (since - HOLD) / FADE).clamp(0.0, 1.0)
    };
    if alpha <= 0.0 {
        return;
    }
    // Drive the fade. The app only repaints on input, so without this the bar
    // would freeze at full opacity: during the hold we wake exactly when the fade
    // is due to start, and during the fade we repaint every frame for smoothness.
    if since <= HOLD {
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(HOLD - since + 1e-3));
    } else {
        ctx.request_repaint();
    }

    let color =
        egui::Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), (alpha * 140.0) as u8);
    let rect = layout.rect;
    let pill = CornerRadius::same((THICKNESS / 2.0) as u8);

    // Vertical: only when there's scrollback above/below the viewport.
    if total_lines > visible_lines {
        let track = rect.height() - 2.0 * MARGIN;
        if track > MIN_THUMB {
            let (pos, size_frac) = v_thumb(total_lines, visible_lines, v_offset);
            let thumb = (track * size_frac).max(MIN_THUMB).min(track);
            let y = rect.top() + MARGIN + pos * (track - thumb);
            let x = rect.right() - MARGIN - THICKNESS;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x, y), Vec2::new(THICKNESS, thumb)),
                pill,
                color,
            );
        }
    }

    // Horizontal: terminals reflow text to the pane width, so content fits
    // horizontally by construction and this normally stays hidden. We still draw
    // it symmetrically if the grid ever reports more columns than fit (e.g. for a
    // frame mid-resize, before the PTY catches up); the +1-cell threshold keeps
    // sub-cell rounding from flickering it on.
    let content_w = columns as f32 * cell_width;
    if content_w > rect.width() + cell_width {
        let track = rect.width() - 2.0 * MARGIN;
        if track > MIN_THUMB {
            let size_frac = (rect.width() / content_w).clamp(0.0, 1.0);
            let thumb = (track * size_frac).max(MIN_THUMB).min(track);
            // The grid has no horizontal scroll offset, so the view is pinned left.
            let x = rect.left() + MARGIN;
            let y = rect.bottom() - MARGIN - THICKNESS;
            painter.rect_filled(
                Rect::from_min_size(Pos2::new(x, y), Vec2::new(thumb, THICKNESS)),
                pill,
                color,
            );
        }
    }
}

fn process_keyboard_event(
    event: egui::Event,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    modifiers: Modifiers,
) -> InputAction {
    match event {
        egui::Event::Text(text) => process_text_event(&text, modifiers, backend, bindings_layout),
        egui::Event::Paste(text) => InputAction::BackendCall(
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            if modifiers.contains(Modifiers::COMMAND | Modifiers::SHIFT) {
                BackendCommand::Write(text.as_bytes().to_vec())
            } else {
                // Plain Ctrl+V belongs to the shell: send the literal ^V
                // byte (terminal paste is Ctrl+Shift+V).
                BackendCommand::Write([0x16].to_vec())
            },
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                BackendCommand::Write(text.as_bytes().to_vec())
            },
        ),
        egui::Event::Copy => {
            #[cfg(not(any(target_os = "ios", target_os = "macos")))]
            if modifiers.contains(Modifiers::COMMAND | Modifiers::SHIFT) {
                let content = backend.selectable_content();
                InputAction::WriteToClipboard(content)
            } else {
                // Plain Ctrl+C belongs to the shell (interrupt): send the
                // literal ^C byte (terminal copy is Ctrl+Shift+C).
                InputAction::BackendCall(BackendCommand::Write([0x3].to_vec()))
            }
            #[cfg(any(target_os = "ios", target_os = "macos"))]
            {
                let content = backend.selectable_content();
                InputAction::WriteToClipboard(content)
            }
        }
        egui::Event::Key {
            key,
            pressed,
            modifiers,
            ..
        } => process_keyboard_key(backend, bindings_layout, key, modifiers, pressed),
        _ => InputAction::Ignore,
    }
}

fn process_text_event(
    text: &str,
    modifiers: Modifiers,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
) -> InputAction {
    if let Some(key) = Key::from_name(text) {
        if bindings_layout.get_action(
            InputKind::KeyCode(key),
            modifiers,
            backend.last_content().terminal_mode,
        ) == BindingAction::Ignore
        {
            InputAction::BackendCall(BackendCommand::Write(text.as_bytes().to_vec()))
        } else {
            InputAction::Ignore
        }
    } else {
        InputAction::BackendCall(BackendCommand::Write(text.as_bytes().to_vec()))
    }
}

fn process_keyboard_key(
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    key: Key,
    modifiers: Modifiers,
    pressed: bool,
) -> InputAction {
    if !pressed {
        return InputAction::Ignore;
    }

    let terminal_mode = backend.last_content().terminal_mode;
    let binding_action =
        bindings_layout.get_action(InputKind::KeyCode(key), modifiers, terminal_mode);

    match binding_action {
        BindingAction::Char(c) => {
            let mut buf = [0, 0, 0, 0];
            let str = c.encode_utf8(&mut buf);
            InputAction::BackendCall(BackendCommand::Write(str.as_bytes().to_vec()))
        }
        BindingAction::Esc(seq) => {
            InputAction::BackendCall(BackendCommand::Write(seq.as_bytes().to_vec()))
        }
        _ => InputAction::Ignore,
    }
}

fn process_mouse_wheel(
    state: &mut TerminalViewState,
    backend: &TerminalBackend,
    font_size: f32,
    unit: MouseWheelUnit,
    delta: Vec2,
    modifiers: &Modifiers,
) -> Vec<InputAction> {
    // Normalise the gesture to whole lines (mice report lines, trackpads
    // points); positive = toward older output, Scroll's convention.
    let lines = match unit {
        MouseWheelUnit::Line => (delta.y.signum() * delta.y.abs().ceil()) as i32,
        MouseWheelUnit::Point => {
            state.scroll_pixels -= delta.y;
            let lines = (state.scroll_pixels / font_size).trunc();
            state.scroll_pixels %= font_size;
            -(lines as i32)
        }
        MouseWheelUnit::Page => 0,
    };
    if lines == 0 {
        return vec![];
    }

    // tessera patch: when the application has mouse reporting enabled (a TUI
    // with its own scroll handling), the wheel belongs to it - report scroll-
    // button presses (64/65), one per line. Plain Scroll would degrade to
    // arrow keys on the alternate screen, which such apps don't want. Shift
    // keeps the wheel for the emulator's own scrollback, like Alacritty.
    let terminal_mode = backend.last_content().terminal_mode;
    if terminal_mode.intersects(TermMode::MOUSE_MODE) && !modifiers.shift {
        let button = if lines > 0 {
            MouseButton::ScrollUp
        } else {
            MouseButton::ScrollDown
        };
        (0..lines.unsigned_abs())
            .map(|_| {
                InputAction::BackendCall(BackendCommand::MouseReport(
                    button.clone(),
                    *modifiers,
                    state.current_mouse_position_on_grid,
                    true,
                ))
            })
            .collect()
    } else {
        vec![InputAction::BackendCall(BackendCommand::Scroll(lines))]
    }
}

fn process_button_click(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    button: PointerButton,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
) -> InputAction {
    match button {
        PointerButton::Primary => process_left_button(
            state,
            layout,
            backend,
            bindings_layout,
            position,
            modifiers,
            pressed,
        ),
        _ => InputAction::Ignore,
    }
}

fn process_left_button(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
    pressed: bool,
) -> InputAction {
    let terminal_mode = backend.last_content().terminal_mode;
    if terminal_mode.intersects(TermMode::MOUSE_MODE) {
        InputAction::BackendCall(BackendCommand::MouseReport(
            MouseButton::LeftButton,
            *modifiers,
            state.current_mouse_position_on_grid,
            pressed,
        ))
    } else if pressed {
        process_left_button_pressed(state, layout, position)
    } else {
        process_left_button_released(state, layout, backend, bindings_layout, position, modifiers)
    }
}

fn process_left_button_pressed(
    state: &mut TerminalViewState,
    layout: &Response,
    position: Pos2,
) -> InputAction {
    state.is_dragged = true;
    InputAction::BackendCall(build_start_select_command(layout, position))
}

fn process_left_button_released(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    bindings_layout: &BindingsLayout,
    position: Pos2,
    modifiers: &Modifiers,
) -> InputAction {
    state.is_dragged = false;
    if layout.double_clicked() || layout.triple_clicked() {
        InputAction::BackendCall(build_start_select_command(layout, position))
    } else {
        let terminal_content = backend.last_content();
        let binding_action = bindings_layout.get_action(
            InputKind::Mouse(PointerButton::Primary),
            *modifiers,
            terminal_content.terminal_mode,
        );

        if binding_action == BindingAction::LinkOpen {
            InputAction::BackendCall(BackendCommand::ProcessLink(
                LinkAction::Open,
                state.current_mouse_position_on_grid,
            ))
        } else {
            InputAction::Ignore
        }
    }
}

fn build_start_select_command(layout: &Response, cursor_position: Pos2) -> BackendCommand {
    let selection_type = if layout.double_clicked() {
        SelectionType::Semantic
    } else if layout.triple_clicked() {
        SelectionType::Lines
    } else {
        SelectionType::Simple
    };

    BackendCommand::SelectStart(
        selection_type,
        cursor_position.x - layout.rect.min.x,
        cursor_position.y - layout.rect.min.y,
    )
}

fn process_mouse_move(
    state: &mut TerminalViewState,
    layout: &Response,
    backend: &TerminalBackend,
    position: Pos2,
    modifiers: &Modifiers,
) -> Vec<InputAction> {
    let terminal_content = backend.last_content();
    let cursor_x = position.x - layout.rect.min.x;
    let cursor_y = position.y - layout.rect.min.y;
    state.current_mouse_position_on_grid = TerminalBackend::selection_point(
        cursor_x,
        cursor_y,
        &terminal_content.terminal_size,
        terminal_content.grid.display_offset(),
    );

    let mut actions = vec![];
    // Handle command or selection update based on terminal mode and modifiers
    if state.is_dragged {
        let terminal_mode = terminal_content.terminal_mode;
        let cmd = if terminal_mode.contains(TermMode::MOUSE_MOTION) && modifiers.is_none() {
            InputAction::BackendCall(BackendCommand::MouseReport(
                MouseButton::LeftMove,
                *modifiers,
                state.current_mouse_position_on_grid,
                true,
            ))
        } else {
            InputAction::BackendCall(BackendCommand::SelectUpdate(cursor_x, cursor_y))
        };

        actions.push(cmd);
    }

    // Handle link hover if applicable
    if modifiers.command_only() {
        actions.push(InputAction::BackendCall(BackendCommand::ProcessLink(
            LinkAction::Hover,
            state.current_mouse_position_on_grid,
        )));
    }

    actions
}

/// Vertical scrollbar geometry from the grid's scroll state, decoupled from
/// painting so the (easy-to-invert) `display_offset` math can be unit-tested.
///
/// `total` is every line in the buffer (scrollback + viewport), `visible` is the
/// viewport height, and `offset` is alacritty's `display_offset` (0 = pinned to
/// the newest line, growing as you scroll back). Returns `(pos, size_frac)` where
/// `size_frac` is the thumb length as a fraction of the track and `pos` is its
/// travel along the track: 0.0 at the oldest line (top), 1.0 at the newest
/// (bottom).
fn v_thumb(total: usize, visible: usize, offset: usize) -> (f32, f32) {
    let size_frac = (visible as f32 / total as f32).clamp(0.0, 1.0);
    // Lines hidden above the viewport, as a fraction of the whole buffer.
    let top_frac = total.saturating_sub(visible).saturating_sub(offset) as f32 / total as f32;
    let pos = if size_frac < 1.0 {
        (top_frac / (1.0 - size_frac)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (pos, size_frac)
}

#[cfg(test)]
mod tests {
    use super::v_thumb;

    #[test]
    fn thumb_size_is_visible_over_total() {
        let (_, size) = v_thumb(400, 100, 0);
        assert!((size - 0.25).abs() < 1e-6);
    }

    #[test]
    fn at_bottom_offset_zero_thumb_is_at_the_end() {
        // display_offset 0 means we're pinned to the newest output.
        let (pos, _) = v_thumb(400, 100, 0);
        assert!((pos - 1.0).abs() < 1e-6, "pos = {pos}");
    }

    #[test]
    fn scrolled_to_the_top_thumb_is_at_the_start() {
        // Max offset = history = total - visible: the oldest line is in view.
        let (pos, _) = v_thumb(400, 100, 300);
        assert!(pos.abs() < 1e-6, "pos = {pos}");
    }

    #[test]
    fn halfway_scrolled_thumb_is_centered() {
        // Half of the 300 lines of history scrolled back.
        let (pos, _) = v_thumb(400, 100, 150);
        assert!((pos - 0.5).abs() < 1e-6, "pos = {pos}");
    }
}
