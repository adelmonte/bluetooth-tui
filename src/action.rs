//! Every user-triggerable action. Names are the keys used in `[keys]` in
//! config.toml; descriptions feed the help popup, footer and command palette.

use crate::define_actions;

define_actions! {
    pub enum Action {
        // global
        Quit => "quit", "Quit", ["q", "ctrl-c"],
        Help => "help", "Show keys & help", ["?", "f1"],
        Palette => "palette", "Command palette", [":", "ctrl-p"],
        NextTab => "next_tab", "Next tab", ["tab", "L"],
        PrevTab => "prev_tab", "Previous tab", ["shift-tab", "H"],
        TabDevices => "tab_devices", "Go to Devices", ["1"],
        TabAdapters => "tab_adapters", "Go to Adapters", ["2"],
        Up => "up", "Move up", ["k", "up"],
        Down => "down", "Move down", ["j", "down"],
        Top => "top", "Jump to top", ["g", "home"],
        Bottom => "bottom", "Jump to bottom", ["G", "end"],
        PageUp => "page_up", "Page up", ["pgup", "ctrl-u"],
        PageDown => "page_down", "Page down", ["pgdn", "ctrl-d"],
        Filter => "filter", "Filter list", ["/"],
        Scan => "scan", "Start/stop scanning", ["s"],
        Power => "power", "Adapter power on/off", ["o"],
        Details => "details", "Show/hide details panel", ["i"],
        Menu => "menu", "Actions for selection", ["m", "."],
        // devices
        Connect => "connect", "Connect / disconnect", ["enter", "space"],
        Pair => "pair", "Pair", ["p"],
        Trust => "trust", "Trust / untrust", ["t"],
        Block => "block", "Block / unblock", ["b"],
        Rename => "rename", "Rename (alias)", ["e", "r"],
        Remove => "remove", "Remove / forget", ["x", "delete"],
        Favorite => "favorite", "Pin to top", ["f"],
        Yank => "yank", "Copy address to clipboard", ["y"],
        // adapters
        UseAdapter => "use_adapter", "Use this adapter", ["enter"],
        Discoverable => "discoverable", "Discoverable on/off", ["d"],
        Pairable => "pairable", "Pairable on/off", ["P"],
        RenameAdapter => "rename_adapter", "Rename adapter", ["e", "r"],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctx {
    Devices,
    Adapters,
}

impl Action {
    pub fn available(self, ctx: Ctx) -> bool {
        use Action::*;
        match self {
            Connect | Pair | Trust | Block | Rename | Remove | Favorite | Yank => ctx == Ctx::Devices,
            UseAdapter | Discoverable | Pairable | RenameAdapter => ctx == Ctx::Adapters,
            _ => true,
        }
    }

    /// Toggles are debounced so a held key doesn't flap the device.
    pub fn is_toggle(self) -> bool {
        use Action::*;
        matches!(self, Connect | Pair | Trust | Block | Favorite | Scan | Power | Discoverable | Pairable)
    }

    /// Shown in the footer for each context, in this order.
    pub fn footer(ctx: Ctx) -> &'static [Action] {
        use Action::*;
        match ctx {
            Ctx::Devices => &[Connect, Pair, Scan, Trust, Rename, Remove, Filter, Menu, Help],
            Ctx::Adapters => &[UseAdapter, Power, Discoverable, Pairable, RenameAdapter, Help],
        }
    }

    pub fn section(self) -> &'static str {
        if self.available(Ctx::Devices) && !self.available(Ctx::Adapters) {
            "Devices"
        } else if self.available(Ctx::Adapters) && !self.available(Ctx::Devices) {
            "Adapters"
        } else {
            "General"
        }
    }
}
