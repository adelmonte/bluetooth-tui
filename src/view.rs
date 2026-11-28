//! Rendering. Pure function of `App` state, except that it records mouse
//! hit-areas and the visible list height for paging.

use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::action::Action;
use crate::app::{App, Field, Popup, Tab, Target};
use crate::backend::{uuid_name, AdapterInfo, AgentRequest, DeviceInfo};
use crate::config::IconSet;
use crate::core::keymap::ActionSet;
use crate::ui::widgets::{self, checkbox, empty_state, mini_bar, panel, popup, signal_bars, spinner, toggle, truncate, HelpRow};

pub fn render(app: &mut App, f: &mut Frame) {
    app.mouse.clear();
    let area = f.area();
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(area);

    render_header(app, f, header);
    if let Some(msg) = app.unavailable.clone() {
        empty_state(f, body, &app.theme, "Bluetooth is unavailable", &[&msg]);
    } else if !app.loaded {
        let s = format!("{} Talking to BlueZ…", spinner(app.tick));
        empty_state(f, body, &app.theme, &s, &[]);
    } else if app.adapters.is_empty() {
        empty_state(
            f,
            body,
            &app.theme,
            "No Bluetooth adapter found",
            &[
                "Plug in a Bluetooth dongle, or check that the radio isn't disabled in firmware.",
                "Useful checks:  rfkill list   ·   systemctl status bluetooth   ·   lsmod | grep btusb",
                "This screen updates by itself when an adapter appears.",
            ],
        );
    } else {
        match app.tab {
            Tab::Devices => render_devices(app, f, body),
            Tab::Adapters => render_adapters(app, f, body),
        }
    }
    render_footer(app, f, footer);
    app.toasts.render(f, body, &app.theme);
    render_popup(app, f, area);
}

// ---------------------------------------------------------------- header / footer

fn render_header(app: &mut App, f: &mut Frame, area: Rect) {
    let t = &app.theme;
    // Right side: adapter + scan status.
    let mut right: Vec<Span> = Vec::new();
    if app.demo {
        right.push(Span::styled(" DEMO ", Style::default().add_modifier(Modifier::REVERSED)));
        right.push(Span::raw(" "));
    }
    if let Some(a) = &app.adapter {
        if a.discovering {
            let secs = app.scan_since.map(|s| s.elapsed().as_secs()).unwrap_or(0);
            right.push(Span::styled(format!("{} scanning {secs}s  ", spinner(app.tick)), t.info()));
        }
        right.push(Span::styled(format!("{} ", a.name), t.dim()));
        right.push(Span::styled(format!("{} ", a.alias), t.text()));
        right.push(if a.powered { Span::styled("● on ", t.ok()) } else { Span::styled("○ off ", t.error()) });
    }
    let rw: u16 = right.iter().map(|s| s.content.width() as u16).sum();
    let tabs_area = Rect { width: area.width.saturating_sub(rw), ..area };
    let titles: Vec<&str> = Tab::ALL.iter().map(|t| t.title()).collect();
    let sel = Tab::ALL.iter().position(|x| *x == app.tab).unwrap_or(0);
    widgets::tabs(f, tabs_area, &titles, sel, &app.theme, &mut app.mouse, Target::Tab);
    if rw <= area.width {
        let r = Rect { x: area.right() - rw, width: rw, ..area };
        f.render_widget(Line::from(right), r);
        if app.adapter.is_some() {
            // Clicking the power dot toggles power.
            app.mouse.add(Rect { x: area.right().saturating_sub(6), width: 6, ..area }, Target::Run(Action::Power));
        }
    }
}

fn render_footer(app: &mut App, f: &mut Frame, area: Rect) {
    let ctx = app.ctx();
    let items: Vec<(String, String, Target)> = Action::footer(ctx)
        .iter()
        .filter_map(|&a| app.keymap.hint(a).map(|k| (k, app.short_label(a), Target::Run(a))))
        .collect();
    let area = Rect { x: area.x + 1, width: area.width.saturating_sub(2), ..area };
    widgets::hints(f, area, &items, &app.theme, &mut app.mouse);
}

// ---------------------------------------------------------------- devices

pub fn icon(d: &DeviceInfo, set: IconSet) -> &'static str {
    let kind = d.icon.as_str();
    match set {
        IconSet::Nerd => match kind {
            "audio-headset" | "audio-headphones" => "󰋋",
            "audio-card" => "󰓃",
            "input-keyboard" => "󰌌",
            "input-mouse" => "󰍽",
            "input-gaming" => "󰊴",
            "input-tablet" => "󰓶",
            "phone" => "󰏲",
            "computer" => "󰍹",
            "video-display" => "󰔂",
            "camera-photo" | "camera-video" => "󰄀",
            "printer" => "󰐪",
            "network-wireless" => "󰖩",
            _ => "󰂯",
        },
        IconSet::Unicode => match kind {
            "audio-headset" | "audio-headphones" | "audio-card" => "♪",
            "input-keyboard" => "⌨",
            "input-mouse" => "⌖",
            "input-gaming" => "◎",
            "phone" => "☏",
            "computer" | "video-display" => "▭",
            _ => "•",
        },
        IconSet::Ascii => match kind {
            k if k.starts_with("audio") => "a",
            k if k.starts_with("input") => "i",
            "phone" => "p",
            _ => "-",
        },
    }
}

pub fn kind_label(d: &DeviceInfo) -> &'static str {
    match d.icon.as_str() {
        "audio-headset" => "Headset",
        "audio-headphones" => "Headphones",
        "audio-card" => "Speaker / audio",
        "input-keyboard" => "Keyboard",
        "input-mouse" => "Mouse",
        "input-gaming" => "Game controller",
        "input-tablet" => "Tablet",
        "phone" => "Phone",
        "computer" => "Computer",
        "video-display" => "Display / TV",
        "printer" => "Printer",
        "" => "Unknown",
        _ => "Other",
    }
}

/// RSSI (dBm) to a rough 0–100 strength.
fn rssi_pct(rssi: i16) -> u8 {
    (((rssi as i32 + 100) * 100) / 60).clamp(0, 100) as u8
}

enum Row<'a> {
    Header(String, Option<Span<'a>>),
    Blank,
    Hint(String),
    Dev(&'a DeviceInfo),
}

fn render_devices(app: &mut App, f: &mut Frame, area: Rect) {
    let wide = app.show_details && area.width >= 96;
    let (list_area, details_area) = if wide {
        let [l, d] = Layout::horizontal([Constraint::Min(50), Constraint::Length(44)]).areas(area);
        (l, Some(d))
    } else {
        (area, None)
    };

    let focused = !app.filter_focus;
    let block = panel(&app.theme, "Devices", focused);
    let inner = block.inner(list_area);
    f.render_widget(block, list_area);
    app.mouse.add(inner, Target::ListArea);

    let mut inner = inner;
    // Power-off banner.
    if app.adapter.as_ref().is_some_and(|a| !a.powered) {
        let b = Rect { height: 1, ..inner };
        let msg = match app.keymap.hint(Action::Power) {
            Some(k) => format!(" Adapter is off — press {k} or click here to turn it on "),
            None => " Adapter is off — click here to turn it on ".into(),
        };
        f.render_widget(Span::styled(msg, app.theme.warn().add_modifier(Modifier::REVERSED)), b);
        app.mouse.add(b, Target::Run(Action::Power));
        inner.y += 2;
        inner.height = inner.height.saturating_sub(2);
    }
    // Filter bar at the bottom.
    if let Some(fi) = &app.filter {
        let fb = Rect { y: inner.bottom().saturating_sub(1), height: 1, ..inner };
        inner.height = inner.height.saturating_sub(1);
        let label = Span::styled(" / ", app.theme.key());
        f.render_widget(label, Rect { width: 3, ..fb });
        fi.render(f, Rect { x: fb.x + 3, width: fb.width.saturating_sub(3), ..fb }, &app.theme, app.filter_focus);
        app.mouse.add(fb, Target::FilterBar);
    }

    let theme = app.theme.clone();
    let scanning = app.adapter.as_ref().is_some_and(|a| a.discovering);
    let (paired, nearby) = app.sections();
    let mut rows: Vec<Row> = Vec::new();
    rows.push(Row::Header(format!("Paired · {}", paired.len()), None));
    if paired.is_empty() {
        rows.push(Row::Hint(if app.filter.is_some() { "no matches".into() } else { "no paired devices yet".into() }));
    }
    rows.extend(paired.iter().map(|d| Row::Dev(d)));
    rows.push(Row::Blank);
    let scan_span = if scanning {
        Some(Span::styled(format!("{} scanning", spinner(app.tick)), theme.info()))
    } else {
        app.keymap.hint(Action::Scan).map(|k| Span::styled(format!("press {k} to scan"), theme.dim()))
    };
    rows.push(Row::Header(format!("Nearby · {}", nearby.len()), scan_span));
    if nearby.is_empty() {
        rows.push(Row::Hint(if scanning { "looking for devices… put yours in pairing mode".into() } else { "nothing found yet".into() }));
    }
    rows.extend(nearby.iter().map(|d| Row::Dev(d)));

    // Keep the selection visible.
    let height = inner.height as usize;
    let sel_idx = rows.iter().position(|r| matches!(r, Row::Dev(d) if Some(&d.address) == app.selected.as_ref()));
    let mut offset = app.list_offset.min(rows.len().saturating_sub(height));
    if let Some(i) = sel_idx {
        // Show the section header above the first device when scrolling up.
        let top = if i <= 1 { 0 } else { i };
        if top < offset {
            offset = top;
        } else if i >= offset + height {
            offset = i + 1 - height;
        }
    }

    let mut targets = Vec::new();
    for (n, row) in rows.iter().enumerate().skip(offset).take(height) {
        let r = Rect { y: inner.y + (n - offset) as u16, height: 1, ..inner };
        match row {
            Row::Header(t, extra) => {
                let mut spans = vec![Span::styled(format!(" {}", t.to_uppercase()), theme.dim().add_modifier(Modifier::BOLD))];
                if let Some(e) = extra {
                    spans.push(Span::raw("   "));
                    spans.push(e.clone());
                }
                f.render_widget(Line::from(spans), r);
            }
            Row::Blank => {}
            Row::Hint(h) => f.render_widget(Span::styled(format!("   {h}"), theme.dim().add_modifier(Modifier::ITALIC)), r),
            Row::Dev(d) => {
                let selected = Some(&d.address) == app.selected.as_ref();
                device_row(app, f, r, d, selected, focused);
                targets.push((r, d.address.clone()));
            }
        }
    }
    // Scrollbar hint.
    if rows.len() > height && height > 0 {
        let pos = (offset * (height.saturating_sub(1))) / rows.len().saturating_sub(height).max(1);
        let sb = Rect { x: list_area.right() - 1, y: inner.y + pos as u16, width: 1, height: 1 };
        f.render_widget(Span::styled("┃", theme.accent()), sb);
    }
    app.list_offset = offset;
    app.list_height = height;
    for (r, a) in targets {
        app.mouse.add(r, Target::Device(a));
    }

    if let Some(d) = details_area {
        render_details(app, f, d);
    }
}

fn device_row(app: &App, f: &mut Frame, r: Rect, d: &DeviceInfo, selected: bool, focused: bool) {
    let t = &app.theme;
    let base = if selected { t.selected(focused) } else { Style::default() };
    f.render_widget(Span::styled(" ".repeat(r.width as usize), base), r);

    // Right-hand status cells, built first so the name can use the remaining width.
    let mut right: Vec<Span> = Vec::new();
    if let Some(label) = app.busy.get(&d.address) {
        right.push(Span::styled(format!("{} {label}…", spinner(app.tick)), t.info()));
    } else if d.blocked {
        right.push(Span::styled("⊘ blocked", t.error()));
    } else if d.connected {
        right.push(Span::styled("● connected", t.ok()));
    } else if d.paired {
        right.push(Span::styled("○ paired", t.dim()));
    } else if let Some(rssi) = d.rssi {
        right.push(signal_bars(rssi_pct(rssi), t));
        right.push(Span::styled(format!(" {rssi:>4} dBm"), t.dim()));
    } else {
        right.push(Span::styled("out of range", t.dim()));
    }
    let batt = match d.battery {
        Some(b) => {
            let style = if b <= 15 { t.error() } else if b <= 30 { t.warn() } else { t.dim() };
            Span::styled(format!("  {} {b:>3}%", mini_bar(b, 4)), style)
        }
        None => Span::raw(" ".repeat(10)),
    };
    right.push(batt);
    right.push(Span::raw(" "));
    let rw: usize = right.iter().map(|s| s.content.width()).sum();

    let marker = if selected { Span::styled("▌", t.accent()) } else { Span::raw(" ") };
    let fav = if app.is_favorite(&d.address) { Span::styled("★ ", t.warn()) } else { Span::raw("  ") };
    let ic = Span::styled(format!("{} ", icon(d, app.cfg.general.icons)), if d.connected { t.accent() } else { t.dim() });
    let name_style = if d.connected { t.bold() } else if d.paired { t.text() } else if d.name.is_empty() { t.dim() } else { t.text() };
    let used = 1 + 2 + ic.content.width();
    let avail = (r.width as usize).saturating_sub(used + rw + 1);
    let mut name = truncate(d.display_name(), avail);
    if d.paired && !d.trusted && name.width() + 11 < avail {
        name.push_str("  untrusted");
    }
    let pad = avail.saturating_sub(name.width());
    let mut spans = vec![marker, fav, ic, Span::styled(name, name_style), Span::raw(" ".repeat(pad + 1))];
    spans.extend(right);
    f.render_widget(Line::from(spans).style(base), r);
}

fn render_details(app: &mut App, f: &mut Frame, area: Rect) {
    let Some(d) = app.selected_device().cloned() else {
        let block = panel(&app.theme, "Details", false);
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Span::styled(" select a device", app.theme.dim()), inner);
        return;
    };
    let t = app.theme.clone();
    let block = panel(&t, "Details", false);
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);

    let mut y = inner.y;
    let mut line = |f: &mut Frame, l: Line, target: Option<Target>, app: &mut App| {
        if y < inner.bottom() {
            let r = Rect { y, height: 1, ..inner };
            f.render_widget(l, r);
            if let Some(tg) = target {
                app.mouse.add(r, tg);
            }
        }
        y += 1;
    };
    let kv = |k: &str, v: Span<'static>| Line::from(vec![Span::styled(format!("{k:<10}"), t.dim()), v]);

    line(f, Line::from(Span::styled(truncate(d.display_name(), inner.width as usize), t.accent().add_modifier(Modifier::BOLD))), None, app);
    if !d.name.is_empty() && d.alias != d.name {
        line(f, Line::from(Span::styled(format!("({})", d.name), t.dim())), None, app);
    }
    line(f, Line::raw(""), None, app);
    line(f, kv("Address", Span::styled(format!("{} ", d.address), t.text())), Some(Target::Run(Action::Yank)), app);
    if !d.address_type.is_empty() {
        line(f, kv("", Span::styled(format!("{} address · y to copy", d.address_type), t.dim())), None, app);
    }
    line(f, kv("Type", Span::styled(kind_label(&d).to_string(), t.text())), None, app);
    let state = if let Some(l) = app.busy.get(&d.address) {
        Span::styled(format!("{} {l}…", spinner(app.tick)), t.info())
    } else if d.connected {
        Span::styled("● connected", t.ok())
    } else {
        Span::styled("○ disconnected", t.dim())
    };
    line(f, kv("State", state), Some(Target::Run(Action::Connect)), app);
    if let Some(b) = d.battery {
        let style = if b <= 15 { t.error() } else if b <= 30 { t.warn() } else { t.ok() };
        line(f, kv("Battery", Span::styled(format!("{} {b}%", mini_bar(b, 10)), style)), None, app);
    }
    if let Some(r) = d.rssi {
        line(f, Line::from(vec![Span::styled(format!("{:<10}", "Signal"), t.dim()), signal_bars(rssi_pct(r), &t), Span::styled(format!(" {r} dBm"), t.dim())]), None, app);
    }
    line(f, Line::raw(""), None, app);
    let flag = |label: &str, on: bool, help: &str| {
        Line::from(vec![checkbox(on, &t), Span::styled(format!(" {label:<9}"), t.text()), Span::styled(help.to_string(), t.dim())])
    };
    line(f, flag("Paired", d.paired, "keys exchanged"), if d.paired { None } else { Some(Target::Run(Action::Pair)) }, app);
    line(f, flag("Trusted", d.trusted, "may reconnect by itself"), Some(Target::Run(Action::Trust)), app);
    line(f, flag("Blocked", d.blocked, "refuse all connections"), Some(Target::Run(Action::Block)), app);
    line(f, flag("Pinned", app.is_favorite(&d.address), "sorted to the top"), Some(Target::Run(Action::Favorite)), app);

    let profiles: Vec<&str> = d.uuids.iter().filter_map(|u| uuid_name(u)).collect();
    if !profiles.is_empty() || !d.uuids.is_empty() {
        line(f, Line::raw(""), None, app);
        line(f, Line::from(Span::styled("PROFILES", t.dim().add_modifier(Modifier::BOLD))), None, app);
        for p in &profiles {
            line(f, Line::from(Span::styled(format!(" · {p}"), t.text())), None, app);
        }
        let unknown = d.uuids.len() - profiles.len();
        if unknown > 0 {
            line(f, Line::from(Span::styled(format!(" · {unknown} vendor service(s)"), t.dim())), None, app);
        }
    }
    if d.is_audio() && d.connected {
        line(f, Line::raw(""), None, app);
        line(f, Line::from(Span::styled("Audio codec & profile: use audio-tui", t.info())), None, app);
    }

    // Action buttons pinned to the bottom of the panel.
    let by = inner.bottom().saturating_sub(1);
    if by > y {
        let mut x = inner.x;
        let mut buttons = vec![Action::Connect];
        if !d.paired {
            buttons.push(Action::Pair);
        }
        buttons.extend([Action::Rename, Action::Remove]);
        for a in buttons {
            let label = format!(" {} ", app.action_label(a));
            let w = label.width() as u16;
            if x + w > inner.right() {
                break;
            }
            let style = if a == Action::Remove { t.error().add_modifier(Modifier::REVERSED) } else { t.accent().add_modifier(Modifier::REVERSED) };
            let r = Rect::new(x, by, w, 1);
            f.render_widget(Span::styled(label, style), r);
            app.mouse.add(r, Target::Run(a));
            x += w + 1;
        }
    }
}

// ---------------------------------------------------------------- adapters

fn render_adapters(app: &mut App, f: &mut Frame, area: Rect) {
    let [list_area, card_area] = Layout::vertical([Constraint::Length(app.adapters.len() as u16 + 2), Constraint::Min(8)]).areas(area);
    let t = app.theme.clone();
    let block = panel(&t, "Adapters", true);
    let inner = block.inner(list_area);
    f.render_widget(block, list_area);
    let current = app.adapter.as_ref().map(|a| a.name.clone());
    for (i, a) in app.adapters.clone().iter().enumerate() {
        let r = Rect { y: inner.y + i as u16, height: 1, ..inner };
        if r.y >= inner.bottom() {
            break;
        }
        let sel = i == app.adapter_sel;
        let base = if sel { t.selected(true) } else { Style::default() };
        let in_use = current.as_deref() == Some(a.name.as_str());
        let line = Line::from(vec![
            if sel { Span::styled("▌", t.accent()) } else { Span::raw(" ") },
            Span::styled(format!("{:<6}", a.name), t.bold()),
            Span::styled(format!("{:<28}", truncate(&a.alias, 27)), t.text()),
            Span::styled(format!("{}  ", a.address), t.dim()),
            if a.powered { Span::styled("● on ", t.ok()) } else { Span::styled("○ off", t.dim()) },
            if in_use { Span::styled("   in use", t.accent()) } else { Span::raw("") },
        ])
        .style(base);
        f.render_widget(Span::styled(" ".repeat(r.width as usize), base), r);
        f.render_widget(widgets::fit_line(line, r.width as usize), r);
        app.mouse.add(r, Target::Adapter(i));
    }

    let Some(a) = app.adapter.clone() else { return };
    let title = format!("{} settings", a.name);
    let block = panel(&t, &title, false);
    let inner = block.inner(card_area).inner(Margin::new(1, 1));
    f.render_widget(block, card_area);
    let rows: Vec<(Field, Line)> = vec![
        (Field::Powered, setting_line(&t, "Powered", toggle(a.powered, &t), power_note(&a))),
        (
            Field::Discoverable,
            setting_line(
                &t,
                "Visible",
                toggle(a.discoverable, &t),
                if a.discoverable_timeout > 0 {
                    format!("other devices can find this computer (turns off after {}s)", a.discoverable_timeout)
                } else {
                    "other devices can find this computer".into()
                },
            ),
        ),
        (Field::Pairable, setting_line(&t, "Pairable", toggle(a.pairable, &t), "accept new pairing requests".into())),
        (Field::Scanning, setting_line(&t, "Scanning", toggle(a.discovering, &t), "look for nearby devices".into())),
        (Field::Alias, setting_line(&t, "Name", Span::styled(a.alias.clone(), t.bold()), "how other devices see this computer · click to rename".into())),
    ];
    for (i, (field, l)) in rows.into_iter().enumerate() {
        let r = Rect { y: inner.y + (i as u16) * 2, height: 1, ..inner };
        if r.y >= inner.bottom() {
            break;
        }
        f.render_widget(widgets::fit_line(l, r.width as usize), r);
        app.mouse.add(r, Target::AdapterField(field));
    }
    let info_y = inner.y + 10;
    if info_y < inner.bottom() {
        let info = Line::from(vec![
            Span::styled(format!("{:<12}", "Address"), t.dim()),
            Span::styled(a.address.clone(), t.text()),
        ]);
        f.render_widget(info, Rect { y: info_y, height: 1, ..inner });
    }
}

fn power_note(a: &AdapterInfo) -> String {
    match (a.rfkill_hard, a.rfkill_soft) {
        (Some(true), _) => "hard-blocked by a hardware switch".into(),
        (_, Some(true)) => "soft-blocked (rfkill) — powering on will try to unblock".into(),
        _ => "radio on/off".into(),
    }
}

fn setting_line<'a>(t: &crate::ui::theme::Theme, label: &str, value: Span<'a>, note: String) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<12}"), t.text()),
        value,
        Span::raw("   "),
        Span::styled(note, t.dim()),
    ])
}

// ---------------------------------------------------------------- popups

fn render_popup(app: &mut App, f: &mut Frame, area: Rect) {
    let t = app.theme.clone();
    let Some(mut p) = app.popup.take() else { return };
    match &mut p {
        Popup::Help { scroll, max } => {
            app.mouse.modal_barrier();
            let rows = help_rows(app);
            *max = widgets::help(f, area, &t, "bluetooth-tui · help", &rows, *scroll);
        }
        Popup::Palette(pal) => pal.render(f, area, &t, &mut app.mouse, Target::PaletteRow),
        Popup::Menu { items, sel, at } => {
            app.mouse.modal_barrier();
            let labels: Vec<(String, String)> =
                items.iter().map(|&a| (app.action_label(a), app.keymap.hint(a).unwrap_or_default())).collect();
            let w = labels.iter().map(|(l, k)| l.width() + k.width() + 4).max().unwrap_or(10) as u16 + 2;
            let h = items.len() as u16 + 2;
            let (mut x, mut y) = *at;
            if (x, y) == (0, 0) {
                // Keyboard-opened: centre it.
                let c = widgets::centered(area, w, h);
                (x, y) = (c.x, c.y);
            }
            let r = Rect::new(x.min(area.right().saturating_sub(w)), y.min(area.bottom().saturating_sub(h)), w, h);
            let inner = popup(f, r, &t, "Actions");
            for (i, (l, k)) in labels.iter().enumerate() {
                let rr = Rect { y: inner.y + i as u16, height: 1, ..inner };
                let style = if i == *sel { t.selected(true) } else { t.text() };
                let pad = (inner.width as usize).saturating_sub(l.width() + k.width() + 2);
                let destructive = items[i] == Action::Remove;
                let ls = if destructive { style.patch(t.error()) } else { style };
                f.render_widget(Line::from(vec![Span::styled(format!(" {l}"), ls), Span::styled(" ".repeat(pad), style), Span::styled(format!("{k} "), style.patch(t.dim()))]), rr);
                app.mouse.add(rr, Target::MenuRow(i));
            }
        }
        Popup::Confirm { title, msg, yes, yes_sel, .. } => {
            widgets::confirm(f, area, &t, title, msg, yes, *yes_sel, &mut app.mouse, Target::Confirm);
        }
        Popup::Rename { addr, input } => {
            app.mouse.modal_barrier();
            let title = if addr.is_some() { "Rename device" } else { "Rename adapter" };
            let r = widgets::centered(area, 54, 7);
            let inner = popup(f, r, &t, title);
            let [lbl, _, field, _, hint] = Layout::vertical([Constraint::Length(1); 5]).areas(inner.inner(Margin::new(1, 0)));
            let who = addr.as_ref().map(|a| app.device_name(a)).unwrap_or_else(|| "This computer".into());
            f.render_widget(Span::styled(format!("New name for {who}:"), t.text()), lbl);
            f.render_widget(Span::styled("─".repeat(field.width as usize), t.border(false)), Rect { y: field.y + 1, ..field });
            input.render(f, field, &t, true);
            f.render_widget(Span::styled("Enter save · Esc cancel", t.dim()), hint);
        }
        Popup::Agent { device, req, input, yes_sel, .. } => {
            let name = app.device_name(device);
            render_agent(app, f, area, &name, req, input, *yes_sel);
        }
    }
    app.popup = Some(p);
}

fn render_agent(app: &mut App, f: &mut Frame, area: Rect, name: &str, req: &AgentRequest, input: &crate::ui::input::TextInput, yes_sel: bool) {
    let t = app.theme.clone();
    let big = |s: String| Span::styled(s, t.accent().add_modifier(Modifier::BOLD));
    let fmt6 = |n: u32| {
        let s = format!("{n:06}");
        format!("{} {}", &s[..3], &s[3..])
    };
    match req {
        AgentRequest::Confirmation(k) => widgets::confirm(
            f,
            area,
            &t,
            "Pairing request",
            &format!("Does {name} show the passkey {}? Only confirm if the numbers match.", fmt6(*k)),
            "They match",
            yes_sel,
            &mut app.mouse,
            Target::Confirm,
        ),
        AgentRequest::Authorization => widgets::confirm(f, area, &t, "Pairing request", &format!("{name} wants to pair with this computer. Allow it?"), "Allow", yes_sel, &mut app.mouse, Target::Confirm),
        AgentRequest::AuthorizeService(svc) => widgets::confirm(f, area, &t, "Service request", &format!("Allow {name} to use “{svc}”?"), "Allow", yes_sel, &mut app.mouse, Target::Confirm),
        AgentRequest::PinCode | AgentRequest::Passkey => {
            app.mouse.modal_barrier();
            let r = widgets::centered(area, 56, 8);
            let inner = popup(f, r, &t, "Pairing request").inner(Margin::new(1, 0));
            let [a, _, b, _, c] = Layout::vertical([Constraint::Length(1); 5]).areas(inner);
            let what = if matches!(req, AgentRequest::Passkey) {
                format!("Type the 6-digit passkey shown on {name}:")
            } else {
                format!("Enter the PIN for {name} (often 0000 or 1234):")
            };
            f.render_widget(Paragraph::new(what).wrap(Wrap { trim: true }).style(t.text()), a);
            input.render(f, b, &t, true);
            f.render_widget(Span::styled("─".repeat(b.width as usize), t.border(false)), Rect { y: b.y + 1, ..b });
            f.render_widget(Span::styled("Enter send · Esc reject", t.dim()), c);
        }
        AgentRequest::DisplayPinCode(pin) => {
            app.mouse.modal_barrier();
            let r = widgets::centered(area, 56, 8);
            let inner = popup(f, r, &t, "Pairing request").inner(Margin::new(1, 0));
            let lines = vec![
                Line::from(Span::styled(format!("Type this PIN on {name}, then press its Enter key:"), t.text())),
                Line::raw(""),
                Line::from(big(pin.clone())).centered(),
                Line::raw(""),
                Line::from(Span::styled("Esc hide", t.dim())),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
        AgentRequest::DisplayPasskey { passkey, entered } => {
            app.mouse.modal_barrier();
            let r = widgets::centered(area, 56, 8);
            let inner = popup(f, r, &t, "Pairing request").inner(Margin::new(1, 0));
            let digits = format!("{passkey:06}");
            let mut code: Vec<Span> = Vec::new();
            for (i, ch) in digits.chars().enumerate() {
                let st = if (i as u16) < *entered { t.ok().add_modifier(Modifier::BOLD) } else { t.accent().add_modifier(Modifier::BOLD) };
                code.push(Span::styled(format!("{ch} "), st));
            }
            code.push(Span::styled("↵", t.dim()));
            let lines = vec![
                Line::from(Span::styled(format!("Type this on {name}'s keyboard, then Enter:"), t.text())),
                Line::raw(""),
                Line::from(code).centered(),
                Line::raw(""),
                Line::from(Span::styled(format!("{entered}/6 typed · Esc hide"), t.dim())),
            ];
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
        }
    }
}

fn help_rows(app: &App) -> Vec<HelpRow> {
    let mut rows = Vec::new();
    for section in ["General", "Devices", "Adapters"] {
        rows.push(HelpRow::Section(section.into()));
        for &a in Action::all().iter().filter(|a| a.section() == section) {
            rows.push(HelpRow::Key(app.keymap.keys_label(a), a.description().to_string(), a.name().to_string()));
        }
    }
    rows.push(HelpRow::Section("Mouse".into()));
    rows.push(HelpRow::Text("click to select · double-click to connect · right-click for actions".into()));
    rows.push(HelpRow::Text("click tabs, footer hints, toggles and buttons · wheel scrolls".into()));
    rows.push(HelpRow::Section("Files".into()));
    rows.push(HelpRow::Text(format!("config  {}", app.cfg_path.display())));
    rows.push(HelpRow::Text(format!("log     {}", crate::core::logging::log_dir(crate::config::APP).join("bluetooth-tui.log").display())));
    rows.push(HelpRow::Text("Rebind keys in [keys] using the dim action names, e.g. connect = [\"enter\"].".into()));
    rows
}

#[cfg(test)]
mod snapshots {
    //! Text snapshots of the UI, for eyeballing layout without a terminal:
    //! `cargo test snap -- --nocapture`
    use super::*;
    use crate::backend::{demo, AdapterInfo, Ev};
    use crate::config::Config;
    use ratatui::{backend::TestBackend, Terminal};

    fn app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut a = App::new(Config::default(), "/tmp/x.toml".into(), tx, true, vec![]);
        a.toasts.dismiss_all();
        let ad = AdapterInfo { name: "hci0".into(), alias: "archbox".into(), address: "9C:B6:D0:F1:3A:20".into(), powered: true, pairable: true, discovering: true, discoverable_timeout: 180, ..Default::default() };
        a.on_backend(Ev::Adapters(vec![ad.clone()]));
        a.on_backend(Ev::Adapter(ad));
        for d in demo::paired_devices() {
            a.on_backend(Ev::Device(d));
        }
        for (_, d) in demo::nearby_devices() {
            a.on_backend(Ev::Device(d));
        }
        a
    }

    pub fn dump(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(app, f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            let mut skip = 0;
            for x in 0..w {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                let s = buf[(x, y)].symbol();
                skip = s.width().saturating_sub(1);
                out.push_str(s);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn snap_devices_wide() {
        let mut a = app();
        a.busy.insert("B8:F6:53:2E:90:AA".into(), "Pairing");
        println!("{}", dump(&mut a, 120, 30));
    }

    #[test]
    fn snap_devices_narrow_with_menu() {
        let mut a = app();
        a.run(Action::Menu);
        println!("{}", dump(&mut a, 80, 24));
    }

    #[test]
    fn snap_adapters_and_help() {
        let mut a = app();
        a.tab = Tab::Adapters;
        println!("{}", dump(&mut a, 100, 24));
        a.run(Action::Help);
        println!("{}", dump(&mut a, 100, 30));
    }

    #[test]
    fn snap_agent_prompt() {
        let mut a = app();
        a.on_backend(Ev::Agent { id: 1, device: "B8:F6:53:2E:90:AA".into(), req: AgentRequest::Confirmation(731052) });
        println!("{}", dump(&mut a, 100, 24));
        a.on_backend(Ev::AgentCancelled(1));
        a.on_backend(Ev::Agent { id: 2, device: "E8:9F:6D:44:20:B1".into(), req: AgentRequest::DisplayPasskey { passkey: 482913, entered: 3 } });
        println!("{}", dump(&mut a, 100, 24));
    }

    #[test]
    fn snap_sweep() {
        let mut a = app();
        a.show_details = true;
        for tab in Tab::ALL {
            a.tab = tab;
            for (w, h) in [(60, 18), (90, 24), (140, 30)] {
                println!("=== {tab:?} {w}x{h}\n{}", dump(&mut a, w, h));
            }
        }
    }
}
