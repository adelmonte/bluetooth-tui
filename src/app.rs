//! Application state and input handling. Rendering lives in `view.rs`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::style::Color;
use tokio::sync::mpsc::UnboundedSender;

use crate::action::{Action, Ctx};
use crate::backend::{AdapterInfo, AgentReply, AgentRequest, Cmd, DeviceInfo, Ev};
use crate::config::Config;
use crate::core::keymap::{ActionSet, Keymap};
use crate::core::keys::KeyCombo;
use crate::ui::input::{InputOutcome, TextInput};
use crate::ui::mouse::MouseMap;
use crate::ui::palette::{Entry, Palette, PaletteOutcome};
use crate::ui::theme::Theme;
use crate::ui::widgets::{ConfirmHit, Level, Toasts};

pub const ACCENT: Color = Color::Rgb(0x89, 0xb4, 0xfa);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Devices,
    Adapters,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Devices, Tab::Adapters];
    pub fn title(self) -> &'static str {
        match self {
            Tab::Devices => "Devices",
            Tab::Adapters => "Adapters",
        }
    }
    pub fn ctx(self) -> Ctx {
        match self {
            Tab::Devices => Ctx::Devices,
            Tab::Adapters => Ctx::Adapters,
        }
    }
}

/// Toggle rows in the adapter settings card and device details panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Powered,
    Discoverable,
    Pairable,
    Scanning,
    Alias,
}

/// What a mouse click can land on.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Tab(usize),
    Device(String),
    Adapter(usize),
    AdapterField(Field),
    Run(Action),
    Confirm(ConfirmHit),
    PaletteRow(usize),
    MenuRow(usize),
    ListArea,
    FilterBar,
}

pub enum Popup {
    Help { scroll: u16, max: u16 },
    Palette(Palette<Action>),
    Menu { items: Vec<Action>, sel: usize, at: (u16, u16) },
    Confirm { title: String, msg: String, yes: String, yes_sel: bool, cmd: Cmd },
    Rename { addr: Option<String>, input: TextInput },
    Agent { id: u64, device: String, req: AgentRequest, input: TextInput, yes_sel: bool },
}

pub struct App {
    pub cfg: Config,
    pub cfg_path: PathBuf,
    pub theme: Theme,
    pub keymap: Keymap<Action>,
    pub tx: UnboundedSender<Cmd>,
    pub demo: bool,

    pub tab: Tab,
    pub adapters: Vec<AdapterInfo>,
    pub adapter: Option<AdapterInfo>,
    pub adapter_sel: usize,
    pub devices: HashMap<String, DeviceInfo>,
    /// First-seen order, so nearby devices don't jump around while RSSI changes.
    pub seen_order: HashMap<String, u64>,
    seen_counter: u64,
    pub selected: Option<String>,
    pub list_offset: usize,
    pub list_height: usize,
    pub filter: Option<TextInput>,
    pub filter_focus: bool,
    pub busy: HashMap<String, &'static str>,
    pub popup: Option<Popup>,
    pub agent_queue: VecDeque<Popup>,
    pub toasts: Toasts,
    pub unavailable: Option<String>,
    pub loaded: bool,
    pub tick: u64,
    pub scan_since: Option<Instant>,
    pub show_details: bool,
    pub mouse: MouseMap<Target>,
    last_click: Option<(Instant, Target)>,
    last_toggle: Option<(Action, Instant)>,
    pub quit: bool,
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf, tx: UnboundedSender<Cmd>, demo: bool, mut warnings: Vec<String>) -> Self {
        let (keymap, kw) = Keymap::build(&cfg.keys);
        warnings.extend(kw);
        let theme = Theme::from_config(&cfg.theme, ACCENT, &mut warnings);
        let mut toasts = Toasts::default();
        for w in warnings {
            toasts.push(Level::Warn, w);
        }
        if demo {
            toasts.push(Level::Info, "Demo mode — fake devices, nothing touches real Bluetooth");
        }
        let show_details = cfg.general.details;
        Self {
            cfg,
            cfg_path,
            theme,
            keymap,
            tx,
            demo,
            tab: Tab::Devices,
            adapters: Vec::new(),
            adapter: None,
            adapter_sel: 0,
            devices: HashMap::new(),
            seen_order: HashMap::new(),
            seen_counter: 0,
            selected: None,
            list_offset: 0,
            list_height: 10,
            filter: None,
            filter_focus: false,
            busy: HashMap::new(),
            popup: None,
            agent_queue: VecDeque::new(),
            toasts,
            unavailable: None,
            loaded: false,
            tick: 0,
            scan_since: None,
            show_details,
            mouse: MouseMap::default(),
            last_click: None,
            last_toggle: None,
            quit: false,
        }
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    pub fn ctx(&self) -> Ctx {
        self.tab.ctx()
    }

    // ------------------------------------------------------------ device list model

    pub fn is_favorite(&self, addr: &str) -> bool {
        self.cfg.devices.favorites.iter().any(|f| f.eq_ignore_ascii_case(addr))
    }

    fn matches_filter(&self, d: &DeviceInfo) -> bool {
        let Some(f) = &self.filter else { return true };
        let q = f.value().to_lowercase();
        q.is_empty()
            || d.display_name().to_lowercase().contains(&q)
            || d.name.to_lowercase().contains(&q)
            || d.address.to_lowercase().contains(&q)
    }

    /// (paired, nearby) device lists, sorted for display.
    pub fn sections(&self) -> (Vec<&DeviceInfo>, Vec<&DeviceInfo>) {
        let mut paired: Vec<&DeviceInfo> = self.devices.values().filter(|d| d.paired && self.matches_filter(d)).collect();
        paired.sort_by_cached_key(|d| {
            (!self.is_favorite(&d.address), !d.connected, d.display_name().to_lowercase(), d.address.clone())
        });
        let mut nearby: Vec<&DeviceInfo> = self
            .devices
            .values()
            .filter(|d| !d.paired && self.matches_filter(d))
            .filter(|d| !(self.cfg.general.hide_unnamed && d.name.is_empty()))
            .collect();
        nearby.sort_by_key(|d| (d.rssi.is_none(), self.seen_order.get(&d.address).copied().unwrap_or(u64::MAX)));
        (paired, nearby)
    }

    /// Flat list of addresses in display order (what j/k walk through).
    pub fn ordered(&self) -> Vec<String> {
        let (p, n) = self.sections();
        p.into_iter().chain(n).map(|d| d.address.clone()).collect()
    }

    pub fn selected_device(&self) -> Option<&DeviceInfo> {
        self.selected.as_ref().and_then(|a| self.devices.get(a))
    }

    fn fix_selection(&mut self) {
        let order = self.ordered();
        if order.is_empty() {
            self.selected = None;
        } else if self.selected.as_ref().is_none_or(|s| !order.contains(s)) {
            self.selected = order.first().cloned();
        }
    }

    fn move_sel(&mut self, delta: isize) {
        match self.tab {
            Tab::Devices => {
                let order = self.ordered();
                if order.is_empty() {
                    return;
                }
                let cur = self.selected.as_ref().and_then(|s| order.iter().position(|a| a == s)).unwrap_or(0) as isize;
                let next = (cur + delta).clamp(0, order.len() as isize - 1) as usize;
                self.selected = Some(order[next].clone());
            }
            Tab::Adapters => {
                if !self.adapters.is_empty() {
                    self.adapter_sel = (self.adapter_sel as isize + delta).clamp(0, self.adapters.len() as isize - 1) as usize;
                }
            }
        }
    }

    // ------------------------------------------------------------ backend events

    pub fn on_backend(&mut self, ev: Ev) {
        match ev {
            Ev::Unavailable(m) => {
                self.unavailable = Some(m);
                self.loaded = true;
            }
            Ev::Adapters(list) => {
                self.loaded = true;
                self.adapters = list;
                if let Some(a) = &self.adapter
                    && !self.adapters.iter().any(|x| x.name == a.name) {
                        self.adapter = None;
                    }
                self.adapter_sel = self.adapter_sel.min(self.adapters.len().saturating_sub(1));
            }
            Ev::Adapter(a) => {
                self.loaded = true;
                if a.discovering && self.scan_since.is_none() {
                    self.scan_since = Some(Instant::now());
                } else if !a.discovering {
                    self.scan_since = None;
                }
                if let Some(x) = self.adapters.iter_mut().find(|x| x.name == a.name) {
                    *x = a.clone();
                } else {
                    self.adapters.push(a.clone());
                }
                self.adapter = Some(a);
            }
            Ev::Device(d) => {
                if !self.seen_order.contains_key(&d.address) {
                    self.seen_counter += 1;
                    self.seen_order.insert(d.address.clone(), self.seen_counter);
                }
                // Only the updated device can have dropped out of view.
                let recheck = self.selected.as_ref().is_none_or(|s| *s == d.address);
                self.devices.insert(d.address.clone(), d);
                if recheck {
                    self.fix_selection();
                }
            }
            Ev::DeviceRemoved(a) => {
                self.devices.remove(&a);
                self.seen_order.remove(&a);
                self.busy.remove(&a);
                self.fix_selection();
            }
            Ev::ResetDevices => {
                self.devices.clear();
                self.seen_order.clear();
                self.busy.clear();
                self.selected = None;
            }
            Ev::Agent { id, device, req } => self.agent_request(id, device, req),
            Ev::AgentCancelled(id) => {
                if matches!(&self.popup, Some(Popup::Agent { id: i, .. }) if *i == id) {
                    self.popup = self.agent_queue.pop_front();
                } else {
                    self.agent_queue.retain(|p| !matches!(p, Popup::Agent { id: i, .. } if *i == id));
                }
            }
            Ev::Busy { addr, label, on } => {
                if on {
                    self.busy.insert(addr, label);
                } else {
                    self.busy.remove(&addr);
                }
            }
            Ev::Ok(m) => self.toasts.push(Level::Ok, m),
            Ev::Error(m) => self.toasts.push(Level::Error, m),
        }
    }

    fn agent_request(&mut self, id: u64, device: String, req: AgentRequest) {
        // DisplayPasskey repeats with more digits entered: update in place.
        if let Some(Popup::Agent { id: cur, req: r, .. }) = &mut self.popup
            && *cur == id {
                *r = req;
                return;
            }
        let numeric = matches!(req, AgentRequest::Passkey);
        let mut input = TextInput::default();
        if numeric {
            input.placeholder = "6 digits".into();
        } else if matches!(req, AgentRequest::PinCode) {
            input.placeholder = "e.g. 0000".into();
        }
        let p = Popup::Agent { id, device, req, input, yes_sel: true };
        match &self.popup {
            Some(Popup::Agent { .. }) => self.agent_queue.push_back(p),
            _ => self.popup = Some(p), // agent prompts are urgent: replace menus/help
        }
    }

    pub fn device_name(&self, addr: &str) -> String {
        self.devices.get(addr).map(|d| d.display_name().to_string()).unwrap_or_else(|| addr.to_string())
    }

    /// Advances timers; true when that changed what's on screen.
    pub fn on_tick(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);
        let mut changed = self.toasts.prune();
        let timeout = self.cfg.general.scan_timeout;
        if let Some(since) = self.scan_since
            && timeout > 0 && since.elapsed() > Duration::from_secs(timeout) {
                self.scan_since = None;
                self.send(Cmd::Scan(false));
                self.toasts.push(Level::Info, format!("Scan stopped after {timeout}s (scan_timeout in config)"));
                changed = true;
            }
        changed
    }

    // ------------------------------------------------------------ terminal events

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) if self.cfg.general.mouse => self.on_mouse(m),
            Event::Paste(s) => self.on_paste(&s),
            _ => {}
        }
    }

    fn on_paste(&mut self, s: &str) {
        let s = s.lines().next().unwrap_or("");
        match &mut self.popup {
            Some(Popup::Rename { input, .. }) | Some(Popup::Agent { input, .. }) => input.insert_str(s),
            Some(Popup::Palette(p)) => p.input.insert_str(s),
            None if self.filter_focus => {
                if let Some(f) = &mut self.filter {
                    f.insert_str(s);
                    self.fix_selection();
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.popup.is_some() {
            return self.popup_key(k);
        }
        if self.filter_focus {
            if let Some(f) = &mut self.filter {
                match f.handle_key(&k) {
                    InputOutcome::Submit => self.filter_focus = false,
                    InputOutcome::Cancel => {
                        self.filter = None;
                        self.filter_focus = false;
                    }
                    InputOutcome::Ignored => {
                        // Let navigation through while typing a filter.
                        let combo = KeyCombo::from_event(&k);
                        if let Some(&a) = self.keymap.actions_for(&combo).iter().find(|a| matches!(a, Action::Up | Action::Down)) {
                            self.run(a);
                        }
                    }
                    _ => {}
                }
                self.fix_selection();
            }
            return;
        }
        if k.code == crossterm::event::KeyCode::Esc {
            if self.filter.is_some() {
                self.filter = None;
                self.fix_selection();
            } else {
                self.toasts.dismiss_all();
            }
            return;
        }
        let combo = KeyCombo::from_event(&k);
        let ctx = self.ctx();
        if let Some(&a) = self.keymap.actions_for(&combo).iter().find(|a| a.available(ctx)) {
            if a.is_toggle() {
                if let Some((last, at)) = self.last_toggle
                    && last == a && at.elapsed() < Duration::from_millis(300) {
                        return; // held key / accidental double press
                    }
                self.last_toggle = Some((a, Instant::now()));
            }
            self.run(a);
        }
    }

    fn popup_key(&mut self, k: KeyEvent) {
        use crossterm::event::KeyCode::*;
        let combo = KeyCombo::from_event(&k);
        let is = |a: Action| self.keymap.keys_for(a).contains(&combo);
        let Some(popup) = self.popup.take() else { return };
        let mut keep = Some(popup);
        match keep.as_mut().expect("just set") {
            Popup::Help { scroll, max } => match k.code {
                Esc | Char('q') | Char('?') | Enter => keep = None,
                Down | Char('j') => *scroll = (*scroll + 1).min(*max),
                Up | Char('k') => *scroll = scroll.saturating_sub(1),
                PageDown | Char(' ') => *scroll = (*scroll + 10).min(*max),
                PageUp => *scroll = scroll.saturating_sub(10),
                Home | Char('g') => *scroll = 0,
                End | Char('G') => *scroll = *max,
                _ => {}
            },
            Popup::Palette(p) => match p.handle_key(&k) {
                PaletteOutcome::Run(a) => {
                    self.popup = None;
                    self.run(a);
                    return;
                }
                PaletteOutcome::Close => keep = None,
                PaletteOutcome::None => {}
            },
            Popup::Menu { items, sel, .. } => match k.code {
                Esc | Char('q') => keep = None,
                Down | Char('j') | Tab => *sel = (*sel + 1) % items.len().max(1),
                Up | Char('k') | BackTab => *sel = (*sel + items.len().max(1) - 1) % items.len().max(1),
                Enter | Char(' ') => {
                    let a = items.get(*sel).copied();
                    self.popup = None;
                    if let Some(a) = a {
                        self.run(a);
                    }
                    return;
                }
                _ => {
                    // Pressing an action's own key inside the menu runs it.
                    if let Some(&a) = items.iter().find(|&&a| is(a)) {
                        self.popup = None;
                        self.run(a);
                        return;
                    }
                }
            },
            Popup::Confirm { yes_sel, cmd, .. } => match k.code {
                Esc | Char('n') | Char('q') => keep = None,
                Left | Right | Tab | BackTab | Char('h') | Char('l') => *yes_sel = !*yes_sel,
                Char('y') => {
                    self.send(cmd.clone());
                    keep = None;
                }
                Enter => {
                    if *yes_sel {
                        self.send(cmd.clone());
                    }
                    keep = None;
                }
                _ => {}
            },
            Popup::Rename { addr, input } => match input.handle_key(&k) {
                InputOutcome::Submit => {
                    let v = input.value().trim().to_string();
                    match addr {
                        Some(a) => self.send(Cmd::SetAlias(a.clone(), v)),
                        None if !v.is_empty() => self.send(Cmd::SetAdapterAlias(v)),
                        None => {}
                    }
                    keep = None;
                }
                InputOutcome::Cancel => keep = None,
                _ => {}
            },
            Popup::Agent { id, req, input, yes_sel, .. } => {
                let id = *id;
                let reply = match req {
                    AgentRequest::PinCode | AgentRequest::Passkey => match input.handle_key(&k) {
                        InputOutcome::Submit => {
                            let v = input.value();
                            if matches!(req, AgentRequest::Passkey) {
                                match v.trim().parse::<u32>() {
                                    Ok(n) if n <= 999_999 => Some(AgentReply::Passkey(n)),
                                    _ => {
                                        self.toasts.push(Level::Warn, "Passkey must be up to 6 digits");
                                        None
                                    }
                                }
                            } else if v.is_empty() {
                                None
                            } else {
                                Some(AgentReply::Pin(v))
                            }
                        }
                        InputOutcome::Cancel => Some(AgentReply::Reject),
                        _ => None,
                    },
                    AgentRequest::DisplayPinCode(_) | AgentRequest::DisplayPasskey { .. } => match k.code {
                        // Nothing to answer; just hide it. Pairing continues on the device.
                        Esc | Enter => {
                            keep = self.agent_queue.pop_front();
                            None
                        }
                        _ => None,
                    },
                    _ => match k.code {
                        Left | Right | Tab | BackTab | Char('h') | Char('l') => {
                            *yes_sel = !*yes_sel;
                            None
                        }
                        Char('y') => Some(AgentReply::Accept),
                        Char('n') | Esc => Some(AgentReply::Reject),
                        Enter => Some(if *yes_sel { AgentReply::Accept } else { AgentReply::Reject }),
                        _ => None,
                    },
                };
                if let Some(r) = reply {
                    self.send(Cmd::AgentReply(id, r));
                    keep = self.agent_queue.pop_front();
                }
            }
        }
        if self.popup.is_none() {
            self.popup = keep;
        }
    }

    // ------------------------------------------------------------ mouse

    fn on_mouse(&mut self, m: MouseEvent) {
        let hit = self.mouse.hit(m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let d = if m.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
                match &mut self.popup {
                    Some(Popup::Help { scroll, max }) => {
                        *scroll = (*scroll as i32 + d * 3).clamp(0, *max as i32) as u16;
                    }
                    Some(Popup::Menu { items, sel, .. }) => {
                        *sel = (*sel as isize + d as isize).clamp(0, items.len() as isize - 1) as usize;
                    }
                    Some(_) => {}
                    None => self.move_sel(d as isize),
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((rect, t)) = hit else {
                    // Click outside a modal closes it (but never an agent prompt).
                    if matches!(self.popup, Some(Popup::Help { .. } | Popup::Menu { .. } | Popup::Palette(_))) {
                        self.popup = None;
                    }
                    return;
                };
                let double = self
                    .last_click
                    .as_ref()
                    .is_some_and(|(at, prev)| *prev == t && at.elapsed() < Duration::from_millis(400));
                self.last_click = Some((Instant::now(), t.clone()));
                self.click(t, double, m.column.saturating_sub(rect.x));
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if self.popup.is_some() {
                    return;
                }
                if let Some((_, Target::Device(a))) = hit {
                    self.selected = Some(a);
                    self.open_menu(Some((m.column, m.row)));
                } else if let Some((_, Target::Adapter(i))) = hit {
                    self.adapter_sel = i;
                    self.open_menu(Some((m.column, m.row)));
                }
            }
            _ => {}
        }
    }

    fn click(&mut self, t: Target, double: bool, rel_x: u16) {
        match t {
            Target::Tab(i) => {
                self.tab = Tab::ALL[i.min(Tab::ALL.len() - 1)];
            }
            Target::Device(a) => {
                self.filter_focus = false;
                self.selected = Some(a);
                if double {
                    self.last_click = None;
                    self.run(Action::Connect);
                }
            }
            Target::Adapter(i) => {
                self.adapter_sel = i;
                if double {
                    self.run(Action::UseAdapter);
                }
            }
            Target::AdapterField(f) => {
                let a = match f {
                    Field::Powered => Action::Power,
                    Field::Discoverable => Action::Discoverable,
                    Field::Pairable => Action::Pairable,
                    Field::Scanning => Action::Scan,
                    Field::Alias => Action::RenameAdapter,
                };
                self.run(a);
            }
            Target::Run(a) => {
                if self.popup.is_some() && !matches!(self.popup, Some(Popup::Menu { .. })) {
                    return;
                }
                self.popup = None;
                self.run(a);
            }
            Target::Confirm(h) => match self.popup.take() {
                Some(Popup::Confirm { cmd, .. }) => {
                    if h == ConfirmHit::Yes {
                        self.send(cmd);
                    }
                }
                Some(Popup::Agent { id, .. }) => {
                    let r = if h == ConfirmHit::Yes { AgentReply::Accept } else { AgentReply::Reject };
                    self.send(Cmd::AgentReply(id, r));
                    self.popup = self.agent_queue.pop_front();
                }
                other => self.popup = other,
            },
            Target::PaletteRow(row) => {
                if let Some(Popup::Palette(p)) = &self.popup
                    && let Some(a) = p.value_at(row) {
                        self.popup = None;
                        self.run(a);
                    }
            }
            Target::MenuRow(row) => {
                if let Some(Popup::Menu { items, .. }) = &self.popup
                    && let Some(&a) = items.get(row) {
                        self.popup = None;
                        self.run(a);
                    }
            }
            Target::FilterBar => {
                if let Some(f) = &mut self.filter {
                    f.click(rel_x.saturating_sub(3));
                }
                self.filter_focus = true;
            }
            Target::ListArea => self.filter_focus = false,
        }
    }

    // ------------------------------------------------------------ actions

    fn open_menu(&mut self, at: Option<(u16, u16)>) {
        let ctx = self.ctx();
        let items: Vec<Action> = match ctx {
            Ctx::Devices => {
                let Some(d) = self.selected_device() else { return };
                let mut v = vec![Action::Connect];
                if !d.paired {
                    v.push(Action::Pair);
                }
                v.extend([Action::Trust, Action::Favorite, Action::Rename, Action::Block, Action::Yank, Action::Remove]);
                v
            }
            Ctx::Adapters => vec![Action::UseAdapter, Action::Power, Action::Discoverable, Action::Pairable, Action::RenameAdapter],
        };
        self.popup = Some(Popup::Menu { items, sel: 0, at: at.unwrap_or((0, 0)) });
    }

    /// Label for an action given the current selection (e.g. Connect vs Disconnect).
    pub fn action_label(&self, a: Action) -> String {
        let d = self.selected_device();
        let ad = self.adapter.as_ref();
        match a {
            Action::Connect => if d.is_some_and(|d| d.connected) { "Disconnect" } else { "Connect" }.into(),
            Action::Trust => if d.is_some_and(|d| d.trusted) { "Untrust" } else { "Trust" }.into(),
            Action::Block => if d.is_some_and(|d| d.blocked) { "Unblock" } else { "Block" }.into(),
            Action::Favorite => if d.is_some_and(|d| self.is_favorite(&d.address)) { "Unpin" } else { "Pin to top" }.into(),
            Action::Remove => if d.is_some_and(|d| d.paired) { "Forget" } else { "Remove" }.into(),
            Action::Scan => if ad.is_some_and(|a| a.discovering) { "Stop scan" } else { "Scan" }.into(),
            Action::Power => if ad.is_some_and(|a| a.powered) { "Power off" } else { "Power on" }.into(),
            Action::Discoverable => if ad.is_some_and(|a| a.discoverable) { "Hide" } else { "Make visible" }.into(),
            Action::Pairable => if ad.is_some_and(|a| a.pairable) { "Not pairable" } else { "Pairable" }.into(),
            Action::Rename | Action::RenameAdapter => "Rename".into(),
            other => other.description().into(),
        }
    }

    /// Short footer word.
    pub fn short_label(&self, a: Action) -> String {
        match a {
            Action::Connect | Action::Trust | Action::Scan | Action::Remove => self.action_label(a).to_lowercase(),
            Action::Pair => "pair".into(),
            Action::Rename | Action::RenameAdapter => "rename".into(),
            Action::Filter => "filter".into(),
            Action::Menu => "menu".into(),
            Action::Help => "help".into(),
            Action::UseAdapter => "use".into(),
            Action::Power => "power".into(),
            Action::Discoverable => "visible".into(),
            Action::Pairable => "pairable".into(),
            other => other.name().replace('_', " "),
        }
    }

    pub fn run(&mut self, a: Action) {
        let dev = self.selected_device().cloned();
        let need_dev = |s: &mut Self| {
            if dev.is_none() {
                s.toasts.push(Level::Info, "No device selected");
            }
            dev.clone()
        };
        match a {
            Action::Quit => self.quit = true,
            Action::Help => self.popup = Some(Popup::Help { scroll: 0, max: 0 }),
            Action::Palette => {
                let ctx = self.ctx();
                let entries = Action::all()
                    .iter()
                    .copied()
                    .filter(|a| a.available(ctx) && !matches!(a, Action::Palette | Action::Up | Action::Down))
                    .map(|a| Entry { label: self.action_label(a), keys: self.keymap.hint(a).unwrap_or_default(), value: a })
                    .collect();
                self.popup = Some(Popup::Palette(Palette::new(entries)));
            }
            Action::NextTab | Action::PrevTab => {
                let i = Tab::ALL.iter().position(|t| *t == self.tab).unwrap_or(0);
                let n = Tab::ALL.len();
                let j = if a == Action::NextTab { (i + 1) % n } else { (i + n - 1) % n };
                self.tab = Tab::ALL[j];
            }
            Action::TabDevices => self.tab = Tab::Devices,
            Action::TabAdapters => self.tab = Tab::Adapters,
            Action::Up => self.move_sel(-1),
            Action::Down => self.move_sel(1),
            Action::Top => self.move_sel(-100_000),
            Action::Bottom => self.move_sel(100_000),
            Action::PageUp => self.move_sel(-(self.list_height.max(2) as isize - 1)),
            Action::PageDown => self.move_sel(self.list_height.max(2) as isize - 1),
            Action::Filter => {
                self.tab = Tab::Devices;
                if self.filter.is_none() {
                    self.filter = Some(TextInput::default().with_placeholder("name or address"));
                }
                self.filter_focus = true;
            }
            Action::Scan => {
                let on = !self.adapter.as_ref().is_some_and(|a| a.discovering);
                if on && !self.adapter.as_ref().is_some_and(|a| a.powered) {
                    self.toasts.push(Level::Warn, "Adapter is off — press o to power it on");
                    return;
                }
                self.send(Cmd::Scan(on));
            }
            Action::Power => {
                let on = !self.adapter.as_ref().is_some_and(|a| a.powered);
                if let Some(a) = &self.adapter {
                    if on && a.rfkill_soft == Some(true) {
                        self.toasts.push(Level::Warn, "Bluetooth is soft-blocked: run `rfkill unblock bluetooth`");
                    } else if on && a.rfkill_hard == Some(true) {
                        self.toasts.push(Level::Error, "Bluetooth is hard-blocked by a hardware switch");
                    }
                }
                self.send(Cmd::SetPowered(on));
            }
            Action::Details => self.show_details = !self.show_details,
            Action::Menu => self.open_menu(None),
            Action::Connect => {
                let Some(d) = need_dev(self) else { return };
                if self.busy.contains_key(&d.address) {
                    return;
                }
                if d.connected {
                    self.send(Cmd::Disconnect(d.address));
                } else if !d.paired && !d.trusted {
                    // Connecting an unpaired device implies pairing first.
                    self.send(Cmd::Pair(d.address));
                } else {
                    self.send(Cmd::Connect(d.address));
                }
            }
            Action::Pair => {
                let Some(d) = need_dev(self) else { return };
                if d.paired {
                    self.toasts.push(Level::Info, format!("{} is already paired", d.display_name()));
                } else {
                    self.send(Cmd::Pair(d.address));
                }
            }
            Action::Trust => {
                let Some(d) = need_dev(self) else { return };
                self.send(Cmd::Trust(d.address, !d.trusted));
            }
            Action::Block => {
                let Some(d) = need_dev(self) else { return };
                self.send(Cmd::Block(d.address, !d.blocked));
            }
            Action::Rename => {
                let Some(d) = need_dev(self) else { return };
                let mut input = TextInput::new(&d.alias);
                input.placeholder = if d.name.is_empty() { "alias".into() } else { format!("empty = reset to “{}”", d.name) };
                self.popup = Some(Popup::Rename { addr: Some(d.address), input });
            }
            Action::Remove => {
                let Some(d) = need_dev(self) else { return };
                let cmd = Cmd::Remove(d.address.clone());
                if self.cfg.general.confirm_remove && d.paired {
                    self.popup = Some(Popup::Confirm {
                        title: "Forget device".into(),
                        msg: format!("Forget {}? You'll need to pair it again to use it.", d.display_name()),
                        yes: "Forget".into(),
                        yes_sel: false,
                        cmd,
                    });
                } else {
                    self.send(cmd);
                }
            }
            Action::Favorite => {
                let Some(d) = need_dev(self) else { return };
                let favs = &mut self.cfg.devices.favorites;
                let pinned = if let Some(i) = favs.iter().position(|f| f.eq_ignore_ascii_case(&d.address)) {
                    favs.remove(i);
                    false
                } else {
                    favs.push(d.address.clone());
                    true
                };
                self.save_favorites();
                self.toasts.push(Level::Ok, format!("{} {}", if pinned { "Pinned" } else { "Unpinned" }, d.display_name()));
            }
            Action::Yank => {
                let Some(d) = need_dev(self) else { return };
                osc52_copy(&d.address);
                self.toasts.push(Level::Ok, format!("Copied {}", d.address));
            }
            Action::UseAdapter => {
                if let Some(a) = self.adapters.get(self.adapter_sel) {
                    self.send(Cmd::SelectAdapter(a.name.clone()));
                    self.toasts.push(Level::Info, format!("Using {}", a.name));
                    self.tab = Tab::Devices;
                }
            }
            Action::Discoverable => {
                let on = !self.adapter.as_ref().is_some_and(|a| a.discoverable);
                self.send(Cmd::SetDiscoverable(on));
            }
            Action::Pairable => {
                let on = !self.adapter.as_ref().is_some_and(|a| a.pairable);
                self.send(Cmd::SetPairable(on));
            }
            Action::RenameAdapter => {
                let cur = self.adapter.as_ref().map(|a| a.alias.clone()).unwrap_or_default();
                self.popup = Some(Popup::Rename { addr: None, input: TextInput::new(&cur) });
            }
        }
    }

    fn save_favorites(&mut self) {
        let mut arr = toml_edit::Array::new();
        for f in &self.cfg.devices.favorites {
            arr.push(f.as_str());
        }
        if let Err(e) = crate::core::config::set_value(&self.cfg_path, "devices", "favorites", toml_edit::value(arr)) {
            self.toasts.push(Level::Error, format!("Couldn't save favorites to {}: {e}", self.cfg_path.display()));
        }
    }
}

/// Copy to the system clipboard through the terminal (OSC 52). Works in kitty,
/// foot, wezterm, alacritty, tmux (with set-clipboard on) and over SSH.
fn osc52_copy(text: &str) {
    use std::io::Write;
    let _ = write!(std::io::stdout(), "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = std::io::stdout().flush();
}

fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_reference() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"AC:80:0A"), "QUM6ODA6MEE=");
    }
}
