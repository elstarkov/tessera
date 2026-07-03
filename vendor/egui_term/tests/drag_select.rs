//! End-to-end selection tests: drive `TerminalView` headlessly with synthetic
//! pointer events against a real PTY, and assert on the backend's selection.
//!
//! The interesting case mirrors TUIs like Claude Code: the app enables
//! click-only mouse reporting (`?1000h`), so presses belong to the app but a
//! drag must still produce a local selection (iTerm2-style).

use egui_term::{BackendSettings, TerminalBackend, TerminalMode, TerminalView};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const ROW_Y: f32 = 16.0; // a point inside the first text row (panel margin + half row)

fn spawn_backend(ctx: &egui::Context, child: &str) -> TerminalBackend {
    let (tx, rx) = mpsc::channel();
    // Keep the receiver alive for the backend's lifetime.
    std::mem::forget(rx);
    TerminalBackend::new(
        0,
        ctx.clone(),
        tx,
        BackendSettings {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), child.into()],
            working_directory: None,
        },
    )
    .expect("failed to spawn pty")
}

/// Run one egui frame showing the terminal, feeding the given input events.
/// Returns the frame's platform output (carries e.g. clipboard commands).
fn frame(
    ctx: &egui::Context,
    backend: &mut TerminalBackend,
    events: Vec<egui::Event>,
) -> egui::PlatformOutput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(800.0, 600.0),
        )),
        ..Default::default()
    };
    input.events = events;
    ctx.run(input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let view = TerminalView::new(ui, backend).set_focus(true);
            ui.add(view);
        });
    })
    .platform_output
}

/// Text put on the clipboard by a frame, if any.
fn copied_text(output: &egui::PlatformOutput) -> Option<String> {
    output.commands.iter().find_map(|cmd| match cmd {
        egui::OutputCommand::CopyText(text) => Some(text.clone()),
        _ => None,
    })
}

fn pointer_moved(x: f32, y: f32) -> egui::Event {
    egui::Event::PointerMoved(egui::pos2(x, y))
}

fn pointer_button(x: f32, y: f32, pressed: bool) -> egui::Event {
    egui::Event::PointerButton {
        pos: egui::pos2(x, y),
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::default(),
    }
}

/// Wait until `pred` holds on the synced content (the PTY child's output has
/// been parsed), or panic after a timeout.
fn wait_for(backend: &mut TerminalBackend, what: &str, pred: impl Fn(&TerminalBackend) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        backend.sync();
        if pred(backend) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn drag_across_first_row(ctx: &egui::Context, backend: &mut TerminalBackend) {
    // Warm-up frames so the panel's layout and hit-test data exist.
    frame(ctx, backend, vec![]);
    frame(ctx, backend, vec![pointer_moved(20.0, ROW_Y)]);
    frame(ctx, backend, vec![pointer_button(20.0, ROW_Y, true)]);
    frame(ctx, backend, vec![pointer_moved(80.0, ROW_Y)]);
    frame(ctx, backend, vec![pointer_moved(220.0, ROW_Y)]);
    frame(ctx, backend, vec![pointer_button(220.0, ROW_Y, false)]);
}

#[test]
fn drag_selects_without_mouse_mode() {
    let ctx = egui::Context::default();
    let mut backend = spawn_backend(&ctx, "printf 'plenty of words to select in row one'; cat");
    wait_for(&mut backend, "text on the grid", |b| {
        !b.selectable_content().is_empty() || {
            // No selection yet; check the grid via a probe selection instead:
            // simply wait until the terminal saw some output at all.
            b.last_content().grid.display_iter().any(|c| c.c == 'p')
        }
    });

    drag_across_first_row(&ctx, &mut backend);

    backend.sync();
    let selected = backend.selectable_content();
    assert!(
        selected.contains("of words"),
        "expected a plain drag to select text, got {selected:?}"
    );
}

#[test]
fn drag_selects_on_alt_screen() {
    let ctx = egui::Context::default();
    // Alt screen + click-only reporting, static content.
    let mut backend = spawn_backend(
        &ctx,
        "printf '\\033[?1049h\\033[?1006h\\033[?1000h'; printf 'plenty of words to select in row one'; cat",
    );
    wait_for(&mut backend, "mouse mode + text", |b| {
        b.last_content()
            .terminal_mode
            .intersects(TerminalMode::MOUSE_REPORT_CLICK)
    });

    drag_across_first_row(&ctx, &mut backend);

    backend.sync();
    let selected = backend.selectable_content();
    assert!(
        selected.contains("of words"),
        "expected a drag on the alt screen to select text, got {selected:?}"
    );
}

#[test]
fn drag_survives_app_redraws() {
    let ctx = egui::Context::default();
    // Alt screen + click-only reporting + a busy app erasing-and-rewriting the
    // selected row (`\e[2K` + rewrite, how Ink-style TUIs repaint). An erase
    // intersecting the selection makes the terminal drop it, which used to
    // dead-end the rest of the drag.
    let mut backend = spawn_backend(
        &ctx,
        "printf '\\033[?1049h\\033[?1006h\\033[?1000h'; \
         i=0; while true; do printf '\\033[1;1H\\033[2Kplenty of words to select in row one %d' $i; i=$((i+1)); sleep 0.02; done",
    );
    wait_for(&mut backend, "mouse mode + text", |b| {
        b.last_content()
            .terminal_mode
            .intersects(TerminalMode::MOUSE_REPORT_CLICK)
    });

    // Drag slowly, with real time passing so spinner output lands mid-drag.
    frame(&ctx, &mut backend, vec![]);
    frame(&ctx, &mut backend, vec![pointer_moved(20.0, ROW_Y)]);
    frame(&ctx, &mut backend, vec![pointer_button(20.0, ROW_Y, true)]);
    frame(&ctx, &mut backend, vec![pointer_moved(80.0, ROW_Y)]);
    std::thread::sleep(Duration::from_millis(200));
    frame(&ctx, &mut backend, vec![pointer_moved(220.0, ROW_Y)]);
    std::thread::sleep(Duration::from_millis(200));
    // Mid-drag, before release: the selection should still exist and render.
    backend.sync();
    let mid_drag = backend.last_content().selectable_range;
    let release = frame(
        &ctx,
        &mut backend,
        vec![pointer_button(220.0, ROW_Y, false)],
    );

    assert!(
        mid_drag.is_some(),
        "selection vanished mid-drag while the app was redrawing"
    );
    // Copy-on-select fires at release; after that the app's repaints may
    // legitimately clear the highlight again, so assert on the copied text.
    let copied = copied_text(&release).unwrap_or_default();
    assert!(
        copied.trim().len() > 10,
        "expected the finished drag to have copied a span of the row, got {copied:?}"
    );
}

#[test]
fn motion_tracking_apps_receive_the_drag() {
    let ctx = egui::Context::default();
    let dump = std::env::temp_dir().join(format!("egui_term_motion_{}.dump", std::process::id()));
    let dump_path = dump.to_string_lossy().to_string();
    // Like Claude Code: any-motion tracking (1003) + SGR. Such apps run their
    // own drag selection, so the drag must be forwarded, not handled locally.
    // stty raw: a real TUI puts the tty in raw mode; without it the line
    // discipline would hold our newline-less reports back from the app.
    let mut backend = spawn_backend(
        &ctx,
        &format!(
            "stty raw -echo; printf '\\033[?1049h\\033[?1006h\\033[?1003h'; \
             printf 'plenty of words to select in row one'; cat > '{dump_path}'"
        ),
    );
    wait_for(&mut backend, "motion tracking", |b| {
        b.last_content()
            .terminal_mode
            .intersects(TerminalMode::MOUSE_MOTION)
    });

    drag_across_first_row(&ctx, &mut backend);
    std::thread::sleep(Duration::from_millis(300));

    let dumped = std::fs::read(&dump).unwrap_or_default();
    let _ = std::fs::remove_file(&dump);
    let received = String::from_utf8_lossy(&dumped);
    assert!(
        received.contains("\x1b[<0;"),
        "app never received the press report: {received:?}"
    );
    assert!(
        received.contains("\x1b[<32;"),
        "app never received button-held motion reports: {received:?}"
    );
    // The gesture belongs to the app - no local selection.
    backend.sync();
    let selected = backend.selectable_content();
    assert!(
        selected.is_empty(),
        "expected no local selection for a motion-tracking app, got {selected:?}"
    );
}

#[test]
fn drag_selects_when_app_tracks_only_clicks() {
    let ctx = egui::Context::default();
    // Like Claude Code: SGR encoding + click-only tracking (no 1002/1003).
    let mut backend = spawn_backend(
        &ctx,
        "printf 'plenty of words to select in row one'; printf '\\033[?1006h\\033[?1000h'; cat",
    );
    wait_for(&mut backend, "mouse mode + text", |b| {
        b.last_content()
            .terminal_mode
            .intersects(TerminalMode::MOUSE_REPORT_CLICK)
    });

    drag_across_first_row(&ctx, &mut backend);

    backend.sync();
    let selected = backend.selectable_content();
    assert!(
        selected.contains("of words"),
        "expected a drag under click-only mouse reporting to select text locally, got {selected:?}"
    );
}
