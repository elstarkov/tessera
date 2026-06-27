//! Stress test for pane/tab churn: rapidly create and drop many backends and
//! confirm the process returns to ~0% CPU with no accumulated threads.
//!
//!   cargo build --release --example churn_stress -p egui_term
//!   ./target/release/examples/churn_stress &
//!   ps -o %cpu= -p $!     # fixed build: ~0% ; buggy build: grows with churn
//!   ps -M $!              # thread count should be small/steady, not ~30+
use std::time::Duration;

fn main() {
    let ctx = egui::Context::default();
    let (tx, _rx) = std::sync::mpsc::channel();

    let rounds = 10u64;
    let per_round = 3u64;
    for round in 0..rounds {
        let mut backends = Vec::new();
        for i in 0..per_round {
            let backend = egui_term::TerminalBackend::new(
                round * per_round + i,
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
        std::thread::sleep(Duration::from_millis(150));
        drop(backends); // close every pane opened this round
        std::thread::sleep(Duration::from_millis(100));
    }

    eprintln!(
        "churned {} backends; observing steady-state CPU for 6s (expect ~0%)...",
        rounds * per_round
    );
    std::thread::sleep(Duration::from_secs(6));
    eprintln!("done");
}
