//! Tessera - a GPU-accelerated terminal emulator in Rust with native,
//! draggable pane splitting (Ghostty look, iTerm2-style splits).
//!
//! Usage:
//!   tessera                      run your $SHELL
//!   tessera <cmd> [args...]      run a specific command in every pane
//!   tessera tmux new -A -s main  attach tmux for a tmux-native workflow
//!   tessera --help

mod app;
mod config;
mod layout;
#[cfg(target_os = "macos")]
mod macos;

use app::{Config, Tessera};
use eframe::egui;

fn main() -> eframe::Result<()> {
    let mut argv = std::env::args().skip(1);
    let first = argv.next();

    if matches!(first.as_deref(), Some("--help" | "-h")) {
        print_help();
        return Ok(());
    }

    // Load user settings (font, theme, padding, ...) before building the window.
    let settings = config::Settings::load();

    // Shell precedence: an explicit CLI command wins, then the config `shell`
    // key, then $SHELL, then a sensible default.
    let (shell, args) = match first {
        Some(cmd) => (cmd, argv.collect::<Vec<_>>()),
        None => {
            let shell = settings
                .shell
                .clone()
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/zsh".to_string());
            // Run as a login shell so app-bundle launches (Dock/Finder), which
            // start with a minimal environment, still source the user's
            // profile and get a full PATH - the same behaviour as
            // Terminal.app, iTerm2 and Alacritty.
            (shell, vec!["-l".to_string()])
        }
    };
    let cfg = Config { shell, args };

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Tessera")
        .with_inner_size([1100.0, 720.0])
        .with_min_inner_size([420.0, 280.0]);
    // Set the runtime window/Dock icon (otherwise eframe shows its default "e").
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(icon);
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Tessera",
        native_options,
        Box::new(|cc| Ok(Box::new(Tessera::new(cc, cfg, settings)))),
    )
}

fn print_help() {
    println!(
        "Tessera - terminal with native, draggable pane splitting\n\n\
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
           Cmd+K          clear the terminal (scrollback + screen)\n  \
           Cmd+F          search the scrollback\n  \
           Cmd+Alt+Arrow  move focus between panes\n  \
           drag a border  resize the adjacent panes\n  \
           click a pane   focus it\n\n\
         CONFIG:\n  \
           ~/.config/tessera/config   font-family, font-size, theme, padding, shell\n  \
           (a commented template is written there on first run; or use the gear menu)\n  \
           themes: {}\n\n\
         TMUX:\n  \
           tessera tmux new -A -s main   run inside tmux for tmux-native splits",
        config::THEMES.join(", ")
    );
}
