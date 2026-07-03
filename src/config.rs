//! User settings, loaded from a Ghostty-style `key = value` config file.
//!
//! The file lives at `$XDG_CONFIG_HOME/tessera/config` (or
//! `~/.config/tessera/config`). Every key is optional; anything missing keeps
//! its built-in default. Unknown keys and malformed lines are reported to
//! stderr but never abort startup - a broken config still gives you a terminal.
//!
//! Supported keys:
//!   font-family       name of an installed font (e.g. "JetBrains Mono")
//!   font-size         point size (default 14)
//!   theme             one of the bundled THEMES (default "default")
//!   window-padding-x  horizontal breathing room inside each pane (default 8)
//!   window-padding-y  vertical breathing room inside each pane (default 8)
//!   shell             program to launch in each pane (default $SHELL)
//!   background        "#rrggbb" override on top of the theme
//!   foreground        "#rrggbb" override on top of the theme

use std::path::PathBuf;

use egui::{Color32, Key, Modifiers};
use egui_term::ColorPalette;

/// A parsed keybinding: the modifiers that must be held plus the trigger key.
#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    pub mods: Modifiers,
    pub key: Key,
}

/// The rebindable, discrete shortcuts. The numbered tab/pane switches
/// (Cmd/Opt+1-9) and arrow navigation (Cmd+Alt+arrows) stay fixed.
#[derive(Debug, Clone)]
pub struct Keybinds {
    pub new_tab: KeySpec,
    pub split_right: KeySpec,
    pub split_down: KeySpec,
    pub close_pane: KeySpec,
    pub find: KeySpec,
    pub clear: KeySpec,
}

impl Default for Keybinds {
    fn default() -> Self {
        let cmd = Modifiers::COMMAND;
        Self {
            new_tab: KeySpec {
                mods: cmd,
                key: Key::T,
            },
            split_right: KeySpec {
                mods: cmd,
                key: Key::D,
            },
            split_down: KeySpec {
                mods: cmd | Modifiers::SHIFT,
                key: Key::D,
            },
            close_pane: KeySpec {
                mods: cmd,
                key: Key::W,
            },
            find: KeySpec {
                mods: cmd,
                key: Key::F,
            },
            clear: KeySpec {
                mods: cmd,
                key: Key::K,
            },
        }
    }
}

/// Parsed, ready-to-apply user settings. Built from the config file (or all
/// defaults if there isn't one).
#[derive(Debug, Clone)]
pub struct Settings {
    pub font_family: Option<String>,
    pub font_size: f32,
    pub theme: String,
    pub padding: (f32, f32),
    pub shell: Option<String>,
    pub background: Option<String>,
    pub foreground: Option<String>,
    pub keybinds: Keybinds,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font_family: None,
            font_size: 14.0,
            theme: "default".to_string(),
            padding: (8.0, 8.0),
            shell: None,
            background: None,
            foreground: None,
            keybinds: Keybinds::default(),
        }
    }
}

impl Settings {
    /// Read and parse the config file, printing any warnings. Missing file =
    /// all defaults (and we drop a commented template there for discoverability).
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (settings, warnings) = parse(&text);
                for w in warnings {
                    eprintln!("tessera: config: {w}");
                }
                settings
            }
            Err(_) => {
                // No file yet: write a commented template so the user has
                // something to edit, then run with defaults. Best-effort.
                write_template(&path);
                Self::default()
            }
        }
    }

    /// The ANSI colour palette for the chosen theme, with `background` /
    /// `foreground` overrides applied on top.
    pub fn palette(&self) -> ColorPalette {
        let mut p = theme_palette(&self.theme);
        if let Some(bg) = self.background.as_ref().filter(|s| parse_hex(s).is_some()) {
            p.background = bg.clone();
        }
        if let Some(fg) = self.foreground.as_ref().filter(|s| parse_hex(s).is_some()) {
            p.foreground = fg.clone();
        }
        p
    }

    /// Load the configured font's bytes from the system, if a `font-family` is
    /// set and matches an installed font. Returns `(bytes, face_index)` ready to
    /// hand to egui. `None` means "use the bundled default".
    pub fn load_font(&self) -> Option<(Vec<u8>, u32)> {
        self.query_font(fontdb::Weight::NORMAL, false)
    }

    /// Load the bold face of the configured font, for cells with the bold
    /// attribute. `None` when no `font-family` is set (the bundled bold is
    /// used) or the family ships no true bold (bold is then synthesised).
    pub fn load_bold_font(&self) -> Option<(Vec<u8>, u32)> {
        self.query_font(fontdb::Weight::BOLD, true)
    }

    /// fontdb matching returns the *closest* face, so `require_weight` rejects
    /// approximations (a regular face standing in for a requested bold).
    fn query_font(&self, weight: fontdb::Weight, require_weight: bool) -> Option<(Vec<u8>, u32)> {
        let family = self.font_family.as_ref()?;
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let query = fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            weight,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        let id = db.query(&query)?;
        if require_weight && db.face(id)?.weight != weight {
            return None;
        }
        db.with_face_data(id, |data, index| (data.to_vec(), index))
    }
}

/// Return the config path, creating a commented template there first if the
/// file doesn't exist yet. Used by the "Settings" menu so there's always
/// something to open. `None` only if we can't even locate a config dir.
pub fn ensure_file() -> Option<PathBuf> {
    let path = config_path()?;
    if !path.exists() {
        write_template(&path);
    }
    Some(path)
}

/// `$XDG_CONFIG_HOME/tessera/config`, falling back to `~/.config/tessera/config`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("tessera").join("config"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("tessera")
            .join("config"),
    )
}

/// Parse config text into `Settings`, collecting human-readable warnings for
/// anything we couldn't make sense of. Pure, so it can be unit-tested.
pub fn parse(text: &str) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut warnings = Vec::new();

    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        // Blank lines and whole-line `#` comments are skipped. We do NOT strip
        // trailing `#...`, because `#` is also a hex-colour prefix.
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            warnings.push(format!(
                "line {}: expected `key = value`, got `{line}`",
                i + 1
            ));
            continue;
        };
        let key = key.trim();
        let val = unquote(val.trim());
        let line = i + 1;

        match key {
            "font-family" => s.font_family = some_unless_empty(val),
            "font-size" => set_num(&mut s.font_size, val, key, line, &mut warnings),
            "theme" => s.theme = normalize_theme(val),
            "window-padding-x" => set_num(&mut s.padding.0, val, key, line, &mut warnings),
            "window-padding-y" => set_num(&mut s.padding.1, val, key, line, &mut warnings),
            "shell" | "command" => s.shell = some_unless_empty(val),
            "background" => s.background = some_unless_empty(val),
            "foreground" => s.foreground = some_unless_empty(val),
            "keybind-new-tab" => set_bind(&mut s.keybinds.new_tab, val, line, &mut warnings),
            "keybind-split-right" => {
                set_bind(&mut s.keybinds.split_right, val, line, &mut warnings)
            }
            "keybind-split-down" => set_bind(&mut s.keybinds.split_down, val, line, &mut warnings),
            "keybind-close-pane" => set_bind(&mut s.keybinds.close_pane, val, line, &mut warnings),
            "keybind-find" => set_bind(&mut s.keybinds.find, val, line, &mut warnings),
            "keybind-clear" => set_bind(&mut s.keybinds.clear, val, line, &mut warnings),
            _ => warnings.push(format!("line {line}: unknown key `{key}`")),
        }
    }
    (s, warnings)
}

fn some_unless_empty(val: &str) -> Option<String> {
    (!val.is_empty()).then(|| val.to_string())
}

/// Parse a float into `target`, warning (and keeping the default) on failure.
fn set_num(target: &mut f32, val: &str, key: &str, line: usize, warns: &mut Vec<String>) {
    match val.parse::<f32>() {
        Ok(n) => *target = n,
        Err(_) => warns.push(format!("line {line}: `{key}` needs a number, got `{val}`")),
    }
}

/// Parse a keybinding into `target`, warning (and keeping the default) on failure.
fn set_bind(target: &mut KeySpec, val: &str, line: usize, warns: &mut Vec<String>) {
    match parse_keyspec(val) {
        Some(spec) => *target = spec,
        None => warns.push(format!("line {line}: couldn't parse keybinding `{val}`")),
    }
}

/// Parse a keybinding like "cmd+shift+d" into modifiers + key. Modifier aliases:
/// cmd / command / super, ctrl / control, alt / opt / option, shift. The final
/// token is the key (a letter, digit, or an egui key name like "Enter").
/// Case-insensitive. Returns `None` on any unrecognised token.
pub fn parse_keyspec(s: &str) -> Option<KeySpec> {
    let parts: Vec<&str> = s
        .split('+')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let (key_tok, mod_toks) = parts.split_last()?;
    let mut mods = Modifiers::default();
    for m in mod_toks {
        mods |= match m.to_lowercase().as_str() {
            "cmd" | "command" | "super" | "win" | "meta" => Modifiers::COMMAND,
            "ctrl" | "control" => Modifiers::CTRL,
            "alt" | "opt" | "option" => Modifiers::ALT,
            "shift" => Modifiers::SHIFT,
            _ => return None,
        };
    }
    Some(KeySpec {
        mods,
        key: parse_key(key_tok)?,
    })
}

fn parse_key(tok: &str) -> Option<Key> {
    Key::from_name(tok).or_else(|| {
        // Title-case fallback so "enter" / "esc" / "space" work, not just "Enter".
        let mut chars = tok.chars();
        let titled: String = chars
            .next()?
            .to_uppercase()
            .chain(chars.flat_map(char::to_lowercase))
            .collect();
        Key::from_name(&titled)
    })
}

/// Strip a single pair of surrounding double quotes, if present.
fn unquote(val: &str) -> &str {
    val.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(val)
}

/// Theme names are matched leniently: case-insensitive, spaces or underscores
/// treated as hyphens. So "Catppuccin Mocha" == "catppuccin-mocha".
fn normalize_theme(val: &str) -> String {
    val.to_lowercase().replace([' ', '_'], "-")
}

/// Parse "#rrggbb" into a colour. `None` for anything malformed.
pub fn parse_hex(hex: &str) -> Option<Color32> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&h[0..2], 16).ok()?;
    let g = u8::from_str_radix(&h[2..4], 16).ok()?;
    let b = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color32::from_rgb(r, g, b))
}

/// Names of the bundled themes, for `--help` / docs.
pub const THEMES: &[&str] = &[
    "default",
    "catppuccin-mocha",
    "dracula",
    "nord",
    "tokyo-night",
    "gruvbox-dark",
    "solarized-dark",
];

/// Map a (normalized) theme name to its palette. Unknown names fall back to the
/// default base16 palette. Only the 16 ANSI colours + fg/bg are set per theme;
/// the rarely-used dim_* slots keep their defaults via struct-update syntax.
fn theme_palette(name: &str) -> ColorPalette {
    match name {
        "catppuccin-mocha" => ColorPalette {
            foreground: "#cdd6f4".into(),
            background: "#1e1e2e".into(),
            black: "#45475a".into(),
            red: "#f38ba8".into(),
            green: "#a6e3a1".into(),
            yellow: "#f9e2af".into(),
            blue: "#89b4fa".into(),
            magenta: "#f5c2e7".into(),
            cyan: "#94e2d5".into(),
            white: "#bac2de".into(),
            bright_black: "#585b70".into(),
            bright_red: "#f38ba8".into(),
            bright_green: "#a6e3a1".into(),
            bright_yellow: "#f9e2af".into(),
            bright_blue: "#89b4fa".into(),
            bright_magenta: "#f5c2e7".into(),
            bright_cyan: "#94e2d5".into(),
            bright_white: "#a6adc8".into(),
            ..ColorPalette::default()
        },
        "dracula" => ColorPalette {
            foreground: "#f8f8f2".into(),
            background: "#282a36".into(),
            black: "#21222c".into(),
            red: "#ff5555".into(),
            green: "#50fa7b".into(),
            yellow: "#f1fa8c".into(),
            blue: "#bd93f9".into(),
            magenta: "#ff79c6".into(),
            cyan: "#8be9fd".into(),
            white: "#f8f8f2".into(),
            bright_black: "#6272a4".into(),
            bright_red: "#ff6e6e".into(),
            bright_green: "#69ff94".into(),
            bright_yellow: "#ffffa5".into(),
            bright_blue: "#d6acff".into(),
            bright_magenta: "#ff92df".into(),
            bright_cyan: "#a4ffff".into(),
            bright_white: "#ffffff".into(),
            ..ColorPalette::default()
        },
        "nord" => ColorPalette {
            foreground: "#d8dee9".into(),
            background: "#2e3440".into(),
            black: "#3b4252".into(),
            red: "#bf616a".into(),
            green: "#a3be8c".into(),
            yellow: "#ebcb8b".into(),
            blue: "#81a1c1".into(),
            magenta: "#b48ead".into(),
            cyan: "#88c0d0".into(),
            white: "#e5e9f0".into(),
            bright_black: "#4c566a".into(),
            bright_red: "#bf616a".into(),
            bright_green: "#a3be8c".into(),
            bright_yellow: "#ebcb8b".into(),
            bright_blue: "#81a1c1".into(),
            bright_magenta: "#b48ead".into(),
            bright_cyan: "#8fbcbb".into(),
            bright_white: "#eceff4".into(),
            ..ColorPalette::default()
        },
        "tokyo-night" => ColorPalette {
            foreground: "#c0caf5".into(),
            background: "#1a1b26".into(),
            black: "#15161e".into(),
            red: "#f7768e".into(),
            green: "#9ece6a".into(),
            yellow: "#e0af68".into(),
            blue: "#7aa2f7".into(),
            magenta: "#bb9af7".into(),
            cyan: "#7dcfff".into(),
            white: "#a9b1d6".into(),
            bright_black: "#414868".into(),
            bright_red: "#f7768e".into(),
            bright_green: "#9ece6a".into(),
            bright_yellow: "#e0af68".into(),
            bright_blue: "#7aa2f7".into(),
            bright_magenta: "#bb9af7".into(),
            bright_cyan: "#7dcfff".into(),
            bright_white: "#c0caf5".into(),
            ..ColorPalette::default()
        },
        "gruvbox-dark" => ColorPalette {
            foreground: "#ebdbb2".into(),
            background: "#282828".into(),
            black: "#282828".into(),
            red: "#cc241d".into(),
            green: "#98971a".into(),
            yellow: "#d79921".into(),
            blue: "#458588".into(),
            magenta: "#b16286".into(),
            cyan: "#689d6a".into(),
            white: "#a89984".into(),
            bright_black: "#928374".into(),
            bright_red: "#fb4934".into(),
            bright_green: "#b8bb26".into(),
            bright_yellow: "#fabd2f".into(),
            bright_blue: "#83a598".into(),
            bright_magenta: "#d3869b".into(),
            bright_cyan: "#8ec07c".into(),
            bright_white: "#ebdbb2".into(),
            ..ColorPalette::default()
        },
        "solarized-dark" => ColorPalette {
            foreground: "#839496".into(),
            background: "#002b36".into(),
            black: "#073642".into(),
            red: "#dc322f".into(),
            green: "#859900".into(),
            yellow: "#b58900".into(),
            blue: "#268bd2".into(),
            magenta: "#d33682".into(),
            cyan: "#2aa198".into(),
            white: "#eee8d5".into(),
            bright_black: "#586e75".into(),
            bright_red: "#cb4b16".into(),
            bright_green: "#586e75".into(),
            bright_yellow: "#657b83".into(),
            bright_blue: "#839496".into(),
            bright_magenta: "#6c71c4".into(),
            bright_cyan: "#93a1a1".into(),
            bright_white: "#fdf6e3".into(),
            ..ColorPalette::default()
        },
        // "default" and anything unrecognised: the vendored base16 palette.
        _ => ColorPalette::default(),
    }
}

/// Write a commented config template the first time we don't find one, so the
/// user has a discoverable starting point. Best-effort: failures are ignored.
fn write_template(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = "\
# Tessera config. Uncomment a line to change it; `#` starts a comment.
# Themes: default, catppuccin-mocha, dracula, nord, tokyo-night,
#         gruvbox-dark, solarized-dark.

# font-family      = \"JetBrains Mono\"
# font-size        = 14
# theme            = catppuccin-mocha
# window-padding-x = 8
# window-padding-y = 8
# shell            = /bin/zsh
# background       = #1e1e2e
# foreground       = #cdd6f4

# Keybindings. Modifiers: cmd / ctrl / alt (opt) / shift, then a key.
# (Tab/pane switching with Cmd/Opt+1-9 and Cmd+Alt+arrows are fixed.)
# keybind-new-tab     = cmd+t
# keybind-split-right = cmd+d
# keybind-split-down  = cmd+shift+d
# keybind-close-pane  = cmd+w
# keybind-find        = cmd+f
# keybind-clear       = cmd+k
";
    let _ = std::fs::write(path, body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_keys() {
        let (s, warns) = parse(
            "font-family = \"JetBrains Mono\"\nfont-size = 16\ntheme = Catppuccin Mocha\nwindow-padding-x = 4\n",
        );
        assert_eq!(s.font_family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(s.font_size, 16.0);
        assert_eq!(s.theme, "catppuccin-mocha"); // normalized
        assert_eq!(s.padding.0, 4.0);
        assert!(warns.is_empty());
    }

    #[test]
    fn comments_and_blanks_are_ignored() {
        let (s, warns) = parse("# a comment\n\n   \nfont-size = 12\n");
        assert_eq!(s.font_size, 12.0);
        assert!(warns.is_empty());
    }

    #[test]
    fn hash_in_value_is_not_a_comment() {
        let (s, _) = parse("background = #1e1e2e\n");
        assert_eq!(s.background.as_deref(), Some("#1e1e2e"));
        assert!(parse_hex("#1e1e2e").is_some());
    }

    #[test]
    fn unknown_key_and_bad_number_warn_but_keep_defaults() {
        let (s, warns) = parse("wat = 3\nfont-size = big\n");
        assert_eq!(s.font_size, 14.0); // unchanged
        assert_eq!(warns.len(), 2);
    }

    #[test]
    fn bad_hex_is_rejected() {
        assert!(parse_hex("#xyz").is_none());
        assert!(parse_hex("1e1e2e").is_none()); // missing '#'
        assert!(parse_hex("#fff").is_none()); // too short
    }

    #[test]
    fn parses_keybindings() {
        let cmd_shift_d = parse_keyspec("cmd+shift+d").unwrap();
        assert_eq!(cmd_shift_d.key, Key::D);
        assert!(cmd_shift_d.mods.command && cmd_shift_d.mods.shift && !cmd_shift_d.mods.alt);

        // Aliases + case-insensitivity.
        let opt_enter = parse_keyspec("Option+Enter").unwrap();
        assert_eq!(opt_enter.key, Key::Enter);
        assert!(opt_enter.mods.alt);

        // A bare key with no modifiers is allowed.
        assert_eq!(parse_keyspec("f5").unwrap().key, Key::F5);
    }

    #[test]
    fn rejects_bad_keybindings() {
        assert!(parse_keyspec("cmd+").is_none()); // no key
        assert!(parse_keyspec("hyper+d").is_none()); // unknown modifier
        assert!(parse_keyspec("cmd+notakey").is_none()); // unknown key
    }

    #[test]
    fn keybind_overrides_default() {
        let (s, warns) = parse("keybind-find = ctrl+s\n");
        assert_eq!(s.keybinds.find.key, Key::S);
        assert!(s.keybinds.find.mods.ctrl);
        assert!(warns.is_empty());
        // An unparseable bind warns and keeps the default (Cmd+F).
        let (s, warns) = parse("keybind-find = wat\n");
        assert_eq!(s.keybinds.find.key, Key::F);
        assert_eq!(warns.len(), 1);
    }
}
