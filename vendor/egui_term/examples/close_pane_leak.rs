//! Repro for the close-pane CPU leak.
//!
//! Dropping a `TerminalBackend` (what happens when a pane/tab is closed) must
//! stop its background subscription thread, not leave it busy-spinning on a
//! disconnected channel. This creates a few backends, drops them, then holds so
//! an external profiler can measure CPU:
//!
//!   cargo build --release --example close_pane_leak -p egui_term
//!   ./target/release/examples/close_pane_leak &
//!   ps -o %cpu= -p $!     # buggy build: ~N*100%, fixed build: ~0%
use std::time::Duration;

fn main() {
    let ctx = egui::Context::default();
    let (tx, _rx) = std::sync::mpsc::channel();

    let mut backends = Vec::new();
    for id in 0..3u64 {
        let backend = egui_term::TerminalBackend::new(
            id,
            ctx.clone(),
            tx.clone(),
            egui_term::BackendSettings {
                shell: "/bin/sh".to_string(),
                args: vec![],
                working_directory: None,
            },
        )
        .expect("spawn backend");
        backends.push(backend);
    }
    std::thread::sleep(Duration::from_millis(400));

    eprintln!("dropping {} backends (simulating pane close)...", backends.len());
    drop(backends);

    // With the bug, each dropped backend leaves a subscription thread spinning
    // on recv() == Err(Disconnected). Hold so `ps` can sample CPU.
    eprintln!("observing CPU for 7s (expect ~0% once fixed)...");
    std::thread::sleep(Duration::from_secs(7));
    eprintln!("done");
}
