//! Tessera — a GPU-accelerated terminal emulator in Rust with native,
//! draggable pane splitting (Ghostty look, iTerm2-style splits).
//!
//! Usage:
//!   tessera                      run your $SHELL
//!   tessera <cmd> [args...]      run a specific command in every pane
//!   tessera tmux new -A -s main  attach tmux for a tmux-native workflow
//!   tessera --help

mod app;
mod layout;

use app::{Config, Tessera};
use eframe::egui;

fn main() -> eframe::Result<()> {
    let mut argv = std::env::args().skip(1);
    let first = argv.next();

    if matches!(first.as_deref(), Some("--help" | "-h")) {
        print_help();
        return Ok(());
    }

    let (shell, args) = match first {
        Some(cmd) => (cmd, argv.collect::<Vec<_>>()),
        None => (
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".to_string()),
            Vec::new(),
        ),
    };
    let cfg = Config { shell, args };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Tessera")
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([420.0, 280.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Tessera",
        native_options,
        Box::new(|cc| Ok(Box::new(Tessera::new(cc, cfg)))),
    )
}

fn print_help() {
    println!(
        "Tessera — terminal with native, draggable pane splitting\n\n\
         USAGE:\n  \
           tessera [COMMAND [ARGS...]]\n\n\
         If COMMAND is omitted, $SHELL (or /bin/zsh) is launched in each pane.\n\n\
         KEYBOARD:\n  \
           Cmd+T          new tab\n  \
           Cmd+1 .. Cmd+9 switch to tab N\n  \
           Opt+1 .. Opt+9 focus pane N in the current tab\n  \
           Cmd+D          split right (panes side-by-side)\n  \
           Cmd+Shift+D    split down (panes stacked)\n  \
           Cmd+W          close the focused pane\n  \
           Cmd+Alt+Arrow  move focus between panes\n  \
           drag a border  resize the adjacent panes\n  \
           click a pane   focus it\n\n\
         TMUX:\n  \
           tessera tmux new -A -s main   run inside tmux for tmux-native splits"
    );
}
