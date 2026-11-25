//! bluetooth-tui configuration (`~/.config/bluetooth-tui/config.toml`).

use std::collections::HashMap;

use serde::Deserialize;

use crate::ui::theme::ThemeConfig;

pub const APP: &str = "bluetooth-tui";

/// Written to disk on first run; every option with its default and a comment.
pub const DEFAULT_TOML: &str = r##"# bluetooth-tui configuration
# Every option is listed here with its default value.
# Delete a line to go back to the default. Reload by restarting the app.

[general]
# Icon set: "nerd" (Nerd Font glyphs; kitty ships them), "unicode", or "ascii"
icons = "nerd"
# Enable mouse: click, double-click to connect, right-click for a menu, wheel to scroll
mouse = true
# Start scanning as soon as the app opens
scan_on_start = false
# Stop scanning automatically after this many seconds (0 = keep scanning)
scan_timeout = 60
# Hide nearby devices that don't advertise a name
hide_unnamed = false
# After pairing: trust the device (lets it reconnect on its own) and connect
trust_on_pair = true
connect_on_pair = true
# Show the details panel when the terminal is wide enough
details = true
# Ask before removing (forgetting) a device
confirm_remove = true

[devices]
# Pinned devices (by address) always sort to the top. Toggle with `f` in the app.
favorites = []

[theme]
# preset = "dark"        # "dark" or "mono". The NO_COLOR env var also forces mono.
# accent = "#89b4fa"     # names ("blue"), 0-255 indexes, or "#rrggbb"
# fg = "reset"
# dim = "#6c7086"
# border = "#45475a"
# selection = "#313244"
# ok = "#a6e3a1"
# warn = "#f9e2af"
# error = "#f38ba8"
# info = "#89dceb"

[keys]
# Rebind any action: action_name = ["key", "another-key"]. [] unbinds.
# Key syntax: "j", "J", "ctrl-p", "alt-x", "shift-tab", "enter", "space",
# "esc", "up", "pgdn", "home", "delete", "f1".
# Press `?` in the app to see every action name and its current keys.
# connect = ["enter", "space"]
# remove = ["x", "delete"]
"##;

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct Config {
    pub general: General,
    pub devices: Devices,
    pub theme: ThemeConfig,
    pub keys: HashMap<String, Vec<String>>,
}


#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IconSet {
    Nerd,
    Unicode,
    Ascii,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(default = "d_icons")]
    pub icons: IconSet,
    #[serde(default = "d_true")]
    pub mouse: bool,
    #[serde(default)]
    pub scan_on_start: bool,
    #[serde(default = "d_scan_timeout")]
    pub scan_timeout: u64,
    #[serde(default)]
    pub hide_unnamed: bool,
    #[serde(default = "d_true")]
    pub trust_on_pair: bool,
    #[serde(default = "d_true")]
    pub connect_on_pair: bool,
    #[serde(default = "d_true")]
    pub details: bool,
    #[serde(default = "d_true")]
    pub confirm_remove: bool,
}

impl Default for General {
    fn default() -> Self {
        toml::from_str("").expect("general defaults")
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Devices {
    pub favorites: Vec<String>,
}

fn d_true() -> bool {
    true
}
fn d_icons() -> IconSet {
    IconSet::Nerd
}
fn d_scan_timeout() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_parses_and_matches_struct_defaults() {
        let c: Config = toml::from_str(DEFAULT_TOML).unwrap();
        assert_eq!(c.general.icons, IconSet::Nerd);
        assert_eq!(c.general.scan_timeout, 60);
        assert!(c.devices.favorites.is_empty());
        let g = General::default();
        assert_eq!(g.scan_timeout, c.general.scan_timeout);
        assert_eq!(g.trust_on_pair, c.general.trust_on_pair);
    }

    #[test]
    fn partial_file_fills_defaults() {
        let c: Config = toml::from_str("[general]\nmouse = false\n").unwrap();
        assert!(!c.general.mouse);
        assert!(c.general.details);
    }

    #[test]
    fn unknown_key_is_an_error() {
        assert!(toml::from_str::<Config>("[general]\nbogus = 1\n").is_err());
    }
}
