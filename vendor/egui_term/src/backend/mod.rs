pub mod settings;

use crate::types::Size;
use alacritty_terminal::event::{
    Event, EventListener, Notify, OnResize, WindowSize,
};
use alacritty_terminal::event_loop::{EventLoop, Msg, Notifier};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{
    Selection, SelectionRange, SelectionType as AlacrittySelectionType,
};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::{
    self, cell::Cell, test::TermSize, viewport_to_point, Term, TermMode,
};
use alacritty_terminal::{tty, Grid};
use egui::Modifiers;
use settings::BackendSettings;
use std::borrow::Cow;
use std::cmp::min;
use std::io::Result;
use std::ops::{Index, RangeInclusive};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc};

pub type TerminalMode = TermMode;
pub type PtyEvent = Event;
pub type SelectionType = AlacrittySelectionType;

#[derive(Debug, Clone)]
pub enum BackendCommand {
    Write(Vec<u8>),
    Scroll(i32),
    Resize(Size, Size),
    SelectStart(SelectionType, f32, f32),
    SelectUpdate(f32, f32),
    ProcessLink(LinkAction, Point),
    MouseReport(MouseButton, Modifiers, Point, bool),
}

#[derive(Debug, Clone)]
pub enum MouseMode {
    Sgr,
    Normal(bool),
}

impl From<TermMode> for MouseMode {
    fn from(term_mode: TermMode) -> Self {
        if term_mode.contains(TermMode::SGR_MOUSE) {
            MouseMode::Sgr
        } else if term_mode.contains(TermMode::UTF8_MOUSE) {
            MouseMode::Normal(true)
        } else {
            MouseMode::Normal(false)
        }
    }
}

#[derive(Debug, Clone)]
pub enum MouseButton {
    LeftButton = 0,
    MiddleButton = 1,
    RightButton = 2,
    LeftMove = 32,
    MiddleMove = 33,
    RightMove = 34,
    NoneMove = 35,
    ScrollUp = 64,
    ScrollDown = 65,
    Other = 99,
}

#[derive(Debug, Clone)]
pub enum LinkAction {
    Clear,
    Hover,
    Open,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalSize {
    pub cell_width: u16,
    pub cell_height: u16,
    num_cols: u16,
    num_lines: u16,
    layout_size: Size,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cell_width: 1,
            cell_height: 1,
            num_cols: 80,
            num_lines: 50,
            layout_size: Size::default(),
        }
    }
}

impl Dimensions for TerminalSize {
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }

    fn screen_lines(&self) -> usize {
        self.num_lines as usize
    }

    fn columns(&self) -> usize {
        self.num_cols as usize
    }

    fn last_column(&self) -> Column {
        Column(self.num_cols as usize - 1)
    }

    fn bottommost_line(&self) -> Line {
        Line(self.num_lines as i32 - 1)
    }
}

impl From<TerminalSize> for WindowSize {
    fn from(size: TerminalSize) -> Self {
        Self {
            num_lines: size.num_lines,
            num_cols: size.num_cols,
            cell_width: size.cell_width,
            cell_height: size.cell_height,
        }
    }
}

pub struct TerminalBackend {
    pub id: u64,
    pub url_regex: RegexSearch,
    term: Arc<FairMutex<Term<EventProxy>>>,
    size: TerminalSize,
    notifier: Notifier,
    last_content: RenderableContent,
    /// Set whenever the terminal grid (cells or scroll position) actually changes,
    /// so sync() can skip re-cloning the whole grid on repaints where nothing
    /// changed (mouse move, hover, idle, a sibling pane's output). Set by the PTY
    /// event thread and by the scroll/resize/search paths; cleared by sync().
    dirty: Arc<AtomicBool>,
    /// All matches for the current scrollback search, most-recent first, plus the
    /// index of the focused one (tessera patch).
    search_matches: Vec<Match>,
    search_index: usize,
    /// True between a Cmd+K clear (which sends Ctrl+L) and the shell's redraw
    /// landing, so we can then drop the scrollback that redraw scrolled off.
    clear_pending: bool,
}

impl TerminalBackend {
    pub fn new(
        id: u64,
        app_context: egui::Context,
        pty_event_proxy_sender: Sender<(u64, PtyEvent)>,
        settings: BackendSettings,
    ) -> Result<Self> {
        // tessera patch: guarantee the child shell a usable TERM. GUI launches
        // (macOS Dock/Finder) provide no TERM at all, which leaves zsh/ncurses
        // without terminfo and breaks line editing; keep a valid inherited
        // value, otherwise fall back to the ubiquitous xterm-256color.
        // COLORTERM advertises 24-bit colour support.
        let mut env = std::collections::HashMap::new();
        let term = std::env::var("TERM")
            .ok()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "xterm-256color".to_string());
        env.insert("TERM".to_string(), term);
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(settings.shell, settings.args)),
            working_directory: settings.working_directory,
            env,
            ..tty::Options::default()
        };
        let config = term::Config::default();
        let terminal_size = TerminalSize::default();
        let pty = tty::new(&pty_config, terminal_size.into(), id)?;
        let (event_sender, event_receiver) = mpsc::channel();
        let event_proxy = EventProxy(event_sender);
        let mut term = Term::new(config, &terminal_size, event_proxy.clone());
        let initial_content = RenderableContent {
            grid: term.grid().clone(),
            selectable_range: None,
            terminal_mode: *term.mode(),
            terminal_size,
            cursor: term.grid_mut().cursor_cell().clone(),
            hovered_hyperlink: None,
        };
        let term = Arc::new(FairMutex::new(term));
        let pty_event_loop =
            EventLoop::new(term.clone(), event_proxy, pty, false, false)?;
        let notifier = Notifier(pty_event_loop.channel());
        let url_regex = RegexSearch::new(r#"(ipfs:|ipns:|magnet:|mailto:|gemini://|gopher://|https://|http://|news:|file://|git://|ssh:|ftp://)[^\u{0000}-\u{001F}\u{007F}-\u{009F}<>"\s{-}\^⟨⟩`]+"#).unwrap();
        let _pty_event_loop_thread = pty_event_loop.spawn();
        // Shared with the PTY event thread; every event means the grid changed, so
        // the next sync() must re-clone it. Start dirty so the first sync captures
        // the shell's startup output.
        let dirty = Arc::new(AtomicBool::new(true));
        let thread_dirty = dirty.clone();
        let subscription = std::thread::Builder::new()
            .name(format!("pty_event_subscription_{}", id))
            .spawn(move || loop {
                // `recv()` returns Err once every sender is dropped, which happens
                // when this pane's PTY event loop shuts down (i.e. the pane was
                // closed). The old code ignored that and re-looped, so `recv()`
                // returned Err immediately forever - a busy-spin that pegged a
                // core for every closed pane. Break out of the loop instead.
                let Ok(event) = event_receiver.recv() else {
                    break;
                };
                thread_dirty.store(true, Ordering::Relaxed);
                // If the app side is gone (window closing) there's nobody left to
                // forward to; stop the thread rather than panicking.
                if pty_event_proxy_sender.send((id, event.clone())).is_err() {
                    break;
                }
                app_context.clone().request_repaint();
                if let Event::Exit = event {
                    break;
                }
            });
        // If the forwarder thread couldn't be spawned, the PTY event loop above
        // is already running; tell it to shut down so its thread (and the PTY
        // child) don't leak when we bail out of the constructor here.
        let _pty_event_subscription = match subscription {
            Ok(handle) => handle,
            Err(e) => {
                let _ = notifier.0.send(Msg::Shutdown);
                return Err(e);
            }
        };

        Ok(Self {
            id,
            url_regex,
            term: term.clone(),
            size: terminal_size,
            notifier,
            last_content: initial_content,
            dirty,
            search_matches: Vec::new(),
            search_index: 0,
            clear_pending: false,
        })
    }

    pub fn process_command(&mut self, cmd: BackendCommand) {
        let term = self.term.clone();
        let mut term = term.lock();
        match cmd {
            BackendCommand::Write(input) => {
                self.write(input);
                term.scroll_display(Scroll::Bottom);
                self.dirty.store(true, Ordering::Relaxed);
            },
            BackendCommand::Scroll(delta) => {
                self.scroll(&mut term, delta);
                self.dirty.store(true, Ordering::Relaxed);
            },
            BackendCommand::Resize(layout_size, font_size) => {
                // resize() sets `dirty` itself, but only when the size actually
                // changes - this command is issued every frame, so we must not
                // mark dirty unconditionally here.
                self.resize(&mut term, layout_size, font_size);
            },
            BackendCommand::SelectStart(selection_type, x, y) => {
                self.start_selection(&mut term, selection_type, x, y);
                self.dirty.store(true, Ordering::Relaxed);
            },
            BackendCommand::SelectUpdate(x, y) => {
                self.update_selection(&mut term, x, y);
                self.dirty.store(true, Ordering::Relaxed);
            },
            // Link hover/clear only touch the overlay (recomputed each sync) and
            // mouse reports are echoed back by the app as PTY output (a Wakeup),
            // so neither needs to force a grid re-clone.
            BackendCommand::ProcessLink(link_action, point) => {
                self.process_link_action(&term, link_action, point);
            },
            BackendCommand::MouseReport(button, modifiers, point, pressed) => {
                self.process_mouse_report(button, modifiers, point, pressed);
            },
        };
    }

    pub fn selection_point(
        x: f32,
        y: f32,
        terminal_size: &TerminalSize,
        display_offset: usize,
    ) -> Point {
        let col = (x as usize) / (terminal_size.cell_width as usize);
        let col = min(Column(col), Column(terminal_size.num_cols as usize - 1));

        let line = (y as usize) / (terminal_size.cell_height as usize);
        let line = min(line, terminal_size.num_lines as usize - 1);

        viewport_to_point(display_offset, Point::new(line, col))
    }

    pub fn selectable_content(&self) -> String {
        let content = self.last_content();
        let mut result = String::new();
        if let Some(range) = content.selectable_range {
            for indexed in content.grid.display_iter() {
                if range.contains(indexed.point) {
                    result.push(indexed.c);
                }
            }
        }
        result
    }

    pub fn sync(&mut self) -> &RenderableContent {
        let term = self.term.clone();
        let mut terminal = term.lock();
        let selectable_range = match &terminal.selection {
            Some(s) => s.to_range(&terminal),
            None => None,
        };

        let cursor = terminal.grid_mut().cursor_cell().clone();
        // The full grid clone (up to ~10k scrollback rows) is the expensive part,
        // so only pay it when the grid actually changed since the last sync. The
        // cheap fields below are refreshed every time so selection/cursor stay live.
        if self.dirty.swap(false, Ordering::Relaxed) {
            self.last_content.grid = terminal.grid().clone();
        }
        self.last_content.selectable_range = selectable_range;
        self.last_content.cursor = cursor.clone();
        self.last_content.terminal_mode = *terminal.mode();
        self.last_content.terminal_size = self.size;
        self.last_content()
    }

    pub fn last_content(&self) -> &RenderableContent {
        &self.last_content
    }

    /// Find the next scrollback match of `query` (tessera patch). `forward`
    /// searches toward newer output (down), else toward older (up). `reset`
    /// restarts from the bottom - pass it when the query text changes. Scrolls
    /// the viewport to the match and selects it (so it highlights via the normal
    /// selection rendering). Returns whether a match was found.
    pub fn search(&mut self, query: &str, forward: bool, reset: bool) -> (usize, usize) {
        if query.is_empty() {
            self.clear_search();
            return (0, 0);
        }
        if reset {
            self.rebuild_matches(query);
        } else if !self.search_matches.is_empty() {
            let n = self.search_matches.len();
            self.search_index = if forward {
                (self.search_index + n - 1) % n // toward newer (index 0)
            } else {
                (self.search_index + 1) % n // toward older
            };
        }
        if self.search_matches.is_empty() {
            return (0, 0);
        }
        self.jump_to(self.search_matches[self.search_index].clone());
        (self.search_index + 1, self.search_matches.len())
    }

    /// Collect every match of `query` across the scrollback, most-recent first.
    fn rebuild_matches(&mut self, query: &str) {
        self.search_matches.clear();
        self.search_index = 0;
        let mut regex = match RegexSearch::new(query) {
            Ok(r) => r,
            Err(_) => return,
        };
        let term = self.term.lock();
        let (start, end) = {
            let g = term.grid();
            (
                Point::new(g.topmost_line(), Column(0)),
                Point::new(g.bottommost_line(), g.last_column()),
            )
        };
        let mut matches: Vec<Match> =
            RegexIter::new(start, end, Direction::Right, &term, &mut regex).collect();
        drop(term);
        matches.reverse(); // bottommost (most recent) first
        self.search_matches = matches;
    }

    /// Scroll the viewport to a match and select it (so it highlights).
    fn jump_to(&mut self, m: Match) {
        let (start, end) = (*m.start(), *m.end());
        let mut term = self.term.lock();
        term.scroll_to_point(start);
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
        self.dirty.store(true, Ordering::Relaxed); // scrolled the viewport
    }

    /// Clear the search highlight and return to the bottom of the scrollback.
    pub fn clear_search(&mut self) {
        self.search_matches.clear();
        self.search_index = 0;
        let mut term = self.term.lock();
        term.selection = None;
        term.scroll_display(Scroll::Bottom);
        self.dirty.store(true, Ordering::Relaxed); // scrolled to the bottom
    }

    /// Clear the terminal the way iTerm2's Cmd+K does (tessera patch): send the
    /// shell's own "clear screen" line-editor command (Ctrl+L, `0x0c`). The shell
    /// clears the screen, redraws the prompt at the top, and keeps any typed
    /// input, so the cursor stays in sync. A pure emulator-side wipe can't manage
    /// that: moving the grid behind the shell's back leaves the redrawn prompt and
    /// the shell's cursor disconnected (text ends up floating mid-screen).
    pub fn clear(&mut self) {
        self.write(vec![0x0c]);
        self.clear_pending = true;
    }

    /// Finish a pending Cmd+K clear once the shell's Ctrl+L redraw has been
    /// applied (call this when PTY output for the pane arrives). The redraw's
    /// `\e[2J` scrolls the old screen into scrollback rather than discarding it,
    /// so without this a resize taller would pull the "cleared" lines back into
    /// view. Wiping the history here makes the clear actually stick.
    pub fn finish_clear(&mut self) {
        if !self.clear_pending {
            return;
        }
        self.clear_pending = false;
        let mut term = self.term.lock();
        term.grid_mut().clear_history();
        term.scroll_display(Scroll::Bottom);
        drop(term);
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn process_link_action(
        &mut self,
        terminal: &Term<EventProxy>,
        link_action: LinkAction,
        point: Point,
    ) {
        match link_action {
            LinkAction::Hover => {
                self.last_content.hovered_hyperlink = self.regex_match_at(
                    terminal,
                    point,
                    &mut self.url_regex.clone(),
                );
            },
            LinkAction::Clear => {
                self.last_content.hovered_hyperlink = None;
            },
            LinkAction::Open => {
                self.open_link();
            },
        };
    }

    fn open_link(&self) {
        if let Some(range) = &self.last_content.hovered_hyperlink {
            let start = range.start();
            let end = range.end();

            let mut url = String::from(self.last_content.grid.index(*start).c);
            for indexed in self.last_content.grid.iter_from(*start) {
                url.push(indexed.c);
                if indexed.point == *end {
                    break;
                }
            }

            // A bad/unhandled URL must not take the whole terminal down; just
            // report it and carry on.
            if let Err(e) = open::that(url) {
                eprintln!("tessera: couldn't open link: {e}");
            }
        }
    }

    fn process_mouse_report(
        &self,
        button: MouseButton,
        modifiers: Modifiers,
        point: Point,
        pressed: bool,
    ) {
        let mut mods = 0;
        if modifiers.contains(Modifiers::SHIFT) {
            mods += 4;
        }
        if modifiers.contains(Modifiers::ALT) {
            mods += 8;
        }
        if modifiers.contains(Modifiers::COMMAND) {
            mods += 16;
        }

        match MouseMode::from(self.last_content().terminal_mode) {
            MouseMode::Sgr => {
                self.sgr_mouse_report(point, button as u8 + mods, pressed)
            },
            MouseMode::Normal(is_utf8) => {
                if pressed {
                    self.normal_mouse_report(
                        point,
                        button as u8 + mods,
                        is_utf8,
                    )
                } else {
                    self.normal_mouse_report(point, 3 + mods, is_utf8)
                }
            },
        }
    }

    fn sgr_mouse_report(&self, point: Point, button: u8, pressed: bool) {
        let c = if pressed { 'M' } else { 'm' };

        let msg = format!(
            "\x1b[<{};{};{}{}",
            button,
            point.column + 1,
            point.line + 1,
            c
        );

        self.notifier.notify(msg.as_bytes().to_vec());
    }

    fn normal_mouse_report(&self, point: Point, button: u8, is_utf8: bool) {
        let Point { line, column } = point;
        let max_point = if is_utf8 { 2015 } else { 223 };

        if line >= max_point || column >= max_point {
            return;
        }

        let mut msg = vec![b'\x1b', b'[', b'M', 32 + button];

        let mouse_pos_encode = |pos: usize| -> Vec<u8> {
            let pos = 32 + 1 + pos;
            let first = 0xC0 + pos / 64;
            let second = 0x80 + (pos & 63);
            vec![first as u8, second as u8]
        };

        if is_utf8 && column >= Column(95) {
            msg.append(&mut mouse_pos_encode(column.0));
        } else {
            msg.push(32 + 1 + column.0 as u8);
        }

        if is_utf8 && line >= 95 {
            msg.append(&mut mouse_pos_encode(line.0 as usize));
        } else {
            msg.push(32 + 1 + line.0 as u8);
        }

        self.notifier.notify(msg);
    }

    fn start_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        selection_type: SelectionType,
        x: f32,
        y: f32,
    ) {
        let location = Self::selection_point(
            x,
            y,
            &self.size,
            terminal.grid().display_offset(),
        );
        terminal.selection = Some(Selection::new(
            selection_type,
            location,
            self.selection_side(x),
        ));
    }

    fn update_selection(
        &mut self,
        terminal: &mut Term<EventProxy>,
        x: f32,
        y: f32,
    ) {
        let display_offset = terminal.grid().display_offset();
        if let Some(ref mut selection) = terminal.selection {
            let location =
                Self::selection_point(x, y, &self.size, display_offset);
            selection.update(location, self.selection_side(x));
        }
    }

    fn selection_side(&self, x: f32) -> Side {
        let cell_x = x as usize % self.size.cell_width as usize;
        let half_cell_width = (self.size.cell_width as f32 / 2.0) as usize;

        if cell_x > half_cell_width {
            Side::Right
        } else {
            Side::Left
        }
    }

    fn resize(
        &mut self,
        terminal: &mut Term<EventProxy>,
        layout_size: Size,
        font_size: Size,
    ) {
        if layout_size == self.size.layout_size
            && font_size.width as u16 == self.size.cell_width
            && font_size.height as u16 == self.size.cell_height
        {
            return;
        }

        let lines = (layout_size.height / font_size.height.floor()) as u16;
        let cols = (layout_size.width / font_size.width.floor()) as u16;
        if lines > 0 && cols > 0 {
            self.size = TerminalSize {
                layout_size,
                cell_height: font_size.height as u16,
                cell_width: font_size.width as u16,
                num_lines: lines,
                num_cols: cols,
            };

            self.notifier.on_resize(self.size.into());
            terminal.resize(TermSize::new(
                self.size.num_cols as usize,
                self.size.num_lines as usize,
            ));
            self.dirty.store(true, Ordering::Relaxed);
        }
    }

    fn write<I: Into<Cow<'static, [u8]>>>(&self, input: I) {
        self.notifier.notify(input);
    }

    fn scroll(&mut self, terminal: &mut Term<EventProxy>, delta_value: i32) {
        if delta_value != 0 {
            let scroll = Scroll::Delta(delta_value);
            if terminal
                .mode()
                .contains(TermMode::ALTERNATE_SCROLL | TermMode::ALT_SCREEN)
            {
                let line_cmd = if delta_value > 0 { b'A' } else { b'B' };
                let mut content = vec![];

                for _ in 0..delta_value.abs() {
                    content.push(0x1b);
                    content.push(b'O');
                    content.push(line_cmd);
                }

                self.notifier.notify(content);
            } else {
                terminal.grid_mut().scroll_display(scroll);
            }
        }
    }

    /// Based on alacritty/src/display/hint.rs > regex_match_at
    /// Retrieve the match, if the specified point is inside the content matching the regex.
    fn regex_match_at(
        &self,
        terminal: &Term<EventProxy>,
        point: Point,
        regex: &mut RegexSearch,
    ) -> Option<Match> {
        let x = visible_regex_match_iter(terminal, regex)
            .find(|rm| rm.contains(&point));
        x
    }
}

/// Copied from alacritty/src/display/hint.rs:
/// Iterate over all visible regex matches.
fn visible_regex_match_iter<'a>(
    term: &'a Term<EventProxy>,
    regex: &'a mut RegexSearch,
) -> impl Iterator<Item = Match> + 'a {
    let viewport_start = Line(-(term.grid().display_offset() as i32));
    let viewport_end = viewport_start + term.bottommost_line();
    let mut start =
        term.line_search_left(Point::new(viewport_start, Column(0)));
    let mut end = term.line_search_right(Point::new(viewport_end, Column(0)));
    start.line = start.line.max(viewport_start - 100);
    end.line = end.line.min(viewport_end + 100);

    RegexIter::new(start, end, Direction::Right, term, regex)
        .skip_while(move |rm| rm.end().line < viewport_start)
        .take_while(move |rm| rm.start().line <= viewport_end)
}

pub struct RenderableContent {
    pub grid: Grid<Cell>,
    pub hovered_hyperlink: Option<RangeInclusive<Point>>,
    pub selectable_range: Option<SelectionRange>,
    pub cursor: Cell,
    pub terminal_mode: TermMode,
    pub terminal_size: TerminalSize,
}

impl Default for RenderableContent {
    fn default() -> Self {
        Self {
            grid: Grid::new(0, 0, 0),
            hovered_hyperlink: None,
            selectable_range: None,
            cursor: Cell::default(),
            terminal_mode: TermMode::empty(),
            terminal_size: TerminalSize::default(),
        }
    }
}

impl Drop for TerminalBackend {
    fn drop(&mut self) {
        let _ = self.notifier.0.send(Msg::Shutdown);
    }
}

#[derive(Clone)]
pub struct EventProxy(mpsc::Sender<Event>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        let _ = self.0.send(event.clone());
    }
}
