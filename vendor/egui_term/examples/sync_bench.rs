//! Micro-benchmark for the sync() grid-clone cost. Fills the scrollback, then
//! times many sync() calls with NO new terminal output - i.e. the cost paid on
//! every mouse-move / hover / idle repaint. With the dirty-gate, all but the
//! first should be near-free.
//!
//!   cargo build --release --example sync_bench -p egui_term
//!   ./target/release/examples/sync_bench
use std::time::{Duration, Instant};

fn main() {
    let ctx = egui::Context::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let mut backend = egui_term::TerminalBackend::new(
        0,
        ctx,
        tx,
        egui_term::BackendSettings {
            shell: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "seq 30000".to_string()],
            working_directory: None,
        },
    )
    .expect("spawn backend");

    // Let `seq 30000` run and fill the ~10k-row scrollback.
    std::thread::sleep(Duration::from_millis(1500));
    while rx.try_recv().is_ok() {} // drain forwarded events

    backend.sync(); // warm-up: capture current content, clear the dirty flag

    let n = 3000u32;
    let t = Instant::now();
    for _ in 0..n {
        let _ = backend.sync();
    }
    let elapsed = t.elapsed();
    eprintln!(
        "{n} syncs with NO new output: {:?} total, {:.2} us/sync",
        elapsed,
        elapsed.as_secs_f64() * 1e6 / n as f64
    );
}
