//! Demo backend: a fake adapter and devices that behave like the real thing
//! (delays, spinners, agent prompts, failures) so the UI can be developed and
//! demoed on machines without Bluetooth. Run with `--demo`.

use std::collections::HashMap;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::time::{interval, sleep};

use super::{AdapterInfo, AgentReply, AgentRequest, Cmd, DeviceInfo, Ev, Handle};

const A2DP_SINK: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const AVRCP: &str = "0000110e-0000-1000-8000-00805f9b34fb";
const HFP: &str = "0000111e-0000-1000-8000-00805f9b34fb";
const HID: &str = "00001124-0000-1000-8000-00805f9b34fb";
const HOG: &str = "00001812-0000-1000-8000-00805f9b34fb";
const BATT: &str = "0000180f-0000-1000-8000-00805f9b34fb";
const PNP: &str = "00001200-0000-1000-8000-00805f9b34fb";
const OPP: &str = "00001105-0000-1000-8000-00805f9b34fb";
const NAP: &str = "00001116-0000-1000-8000-00805f9b34fb";

fn dev(addr: &str, name: &str, icon: &str, uuids: &[&str]) -> DeviceInfo {
    DeviceInfo {
        address: addr.into(),
        adapter: "hci0".into(),
        name: name.into(),
        alias: name.into(),
        icon: icon.into(),
        uuids: uuids.iter().map(|s| s.to_string()).collect(),
        address_type: "public".into(),
        services_resolved: true,
        ..Default::default()
    }
}

pub(crate) fn paired_devices() -> Vec<DeviceInfo> {
    let mut v = vec![
        DeviceInfo {
            paired: true,
            bonded: true,
            trusted: true,
            connected: true,
            battery: Some(82),
            class: Some(0x240418),
            ..dev("AC:80:0A:3F:12:9E", "WH-1000XM5", "audio-headset", &[A2DP_SINK, AVRCP, HFP, BATT, PNP])
        },
        DeviceInfo {
            paired: true,
            bonded: true,
            trusted: true,
            connected: true,
            battery: Some(46),
            address_type: "random".into(),
            ..dev("D4:3B:04:8E:77:01", "MX Master 3S", "input-mouse", &[HOG, BATT, PNP])
        },
        DeviceInfo {
            paired: true,
            bonded: true,
            trusted: true,
            battery: Some(12),
            ..dev("DC:2C:26:14:B5:40", "Keychron K3", "input-keyboard", &[HID, PNP])
        },
        DeviceInfo { paired: true, bonded: true, trusted: false, ..dev("F0:5C:77:A1:09:6D", "Pixel 8", "phone", &[OPP, NAP, AVRCP, HFP, A2DP_SINK]) },
        DeviceInfo { paired: true, bonded: true, trusted: true, ..dev("00:1A:7D:DA:71:13", "Kitchen Speaker", "audio-card", &[A2DP_SINK, AVRCP]) },
    ];
    v[2].alias = "Work keyboard".into();
    v
}

/// Devices that "appear" while scanning, with a delay (in scan ticks).
pub(crate) fn nearby_devices() -> Vec<(u32, DeviceInfo)> {
    vec![
        (1, DeviceInfo { rssi: Some(-48), ..dev("B8:F6:53:2E:90:AA", "JBL Flip 6", "audio-card", &[A2DP_SINK, AVRCP]) }),
        (2, DeviceInfo { rssi: Some(-71), ..dev("64:A2:F9:31:C7:0E", "Galaxy Buds2 Pro", "audio-headphones", &[A2DP_SINK, HFP, AVRCP]) }),
        (2, DeviceInfo { rssi: Some(-83), name: String::new(), alias: "4E-91-0B-C2-55-17".into(), address_type: "random".into(), ..dev("4E:91:0B:C2:55:17", "", "", &[]) }),
        (3, DeviceInfo { rssi: Some(-60), legacy_pairing: false, ..dev("E8:9F:6D:44:20:B1", "Logitech K380", "input-keyboard", &[HID]) }),
        (4, DeviceInfo { rssi: Some(-90), ..dev("38:18:4C:00:A2:31", "[TV] Living Room", "video-display", &[A2DP_SINK]) }),
        (5, DeviceInfo { rssi: Some(-77), address_type: "random".into(), ..dev("C1:05:E2:7B:3A:9F", "Mi Band 8", "", &[BATT]) }),
    ]
}

pub fn spawn() -> Handle {
    let (tx, cmd_rx) = unbounded_channel();
    let (ev_tx, rx) = unbounded_channel();
    tokio::spawn(run(cmd_rx, ev_tx));
    Handle { tx, rx }
}

struct Demo {
    ev: UnboundedSender<Ev>,
    adapter: AdapterInfo,
    adapter2: AdapterInfo,
    devices: HashMap<String, DeviceInfo>,
    scanning: bool,
    scan_ticks: u32,
    tick: u64,
    next_agent: u64,
    pending: HashMap<u64, String>,
}

enum Later {
    Done(String, &'static str, Result<String, String>),
    Update(DeviceInfo),
}

async fn run(mut cmds: UnboundedReceiver<Cmd>, ev: UnboundedSender<Ev>) {
    let mut d = Demo {
        ev,
        adapter: AdapterInfo {
            name: "hci0".into(),
            address: "9C:B6:D0:F1:3A:20".into(),
            alias: "archbox".into(),
            powered: true,
            pairable: true,
            discoverable_timeout: 180,
            rfkill_soft: Some(false),
            rfkill_hard: Some(false),
            ..Default::default()
        },
        adapter2: AdapterInfo {
            name: "hci1".into(),
            address: "00:1A:7D:DA:71:FF".into(),
            alias: "archbox #2 (USB dongle)".into(),
            powered: false,
            discoverable_timeout: 180,
            rfkill_soft: Some(false),
            rfkill_hard: Some(false),
            ..Default::default()
        },
        devices: paired_devices().into_iter().map(|d| (d.address.clone(), d)).collect(),
        scanning: false,
        scan_ticks: 0,
        tick: 0,
        next_agent: 1,
        pending: HashMap::new(),
    };
    let (ltx, mut lrx) = unbounded_channel::<Later>();
    sleep(Duration::from_millis(150)).await; // feel like a real D-Bus round-trip
    d.emit(Ev::Adapters(vec![d.adapter.clone(), d.adapter2.clone()]));
    d.emit(Ev::Adapter(d.adapter.clone()));
    for x in d.devices.values() {
        d.emit(Ev::Device(x.clone()));
    }
    let mut t = interval(Duration::from_millis(1200));
    loop {
        tokio::select! {
            c = cmds.recv() => match c { Some(c) => d.command(c, &ltx), None => break },
            Some(l) = lrx.recv() => d.later(l),
            _ = t.tick() => d.on_tick(),
        }
    }
}

impl Demo {
    fn emit(&self, e: Ev) {
        let _ = self.ev.send(e);
    }

    fn on_tick(&mut self) {
        self.tick += 1;
        if self.scanning {
            self.scan_ticks += 1;
            for (at, mut dv) in nearby_devices() {
                if at == self.scan_ticks && !self.devices.contains_key(&dv.address) {
                    dv.rssi = dv.rssi.map(|r| r + (self.tick % 5) as i16 - 2);
                    self.devices.insert(dv.address.clone(), dv.clone());
                    self.emit(Ev::Device(dv));
                }
            }
            // RSSI jitter for unpaired devices in range.
            let tick = self.tick;
            let mut changed = Vec::new();
            for dv in self.devices.values_mut() {
                if let Some(r) = dv.rssi.as_mut() {
                    let j = ((tick as i64 * 7 + dv.address.len() as i64 * 3 + *r as i64) % 7) - 3;
                    *r = (*r as i64 + j).clamp(-95, -35) as i16;
                    changed.push(dv.clone());
                }
            }
            for c in changed {
                self.emit(Ev::Device(c));
            }
        }
        // Slow battery drain on connected devices, so live updates are visible.
        if self.tick.is_multiple_of(25) {
            let mut changed = Vec::new();
            for dv in self.devices.values_mut().filter(|d| d.connected) {
                if let Some(b) = dv.battery.as_mut() {
                    *b = b.saturating_sub(1);
                    changed.push(dv.clone());
                }
            }
            for c in changed {
                self.emit(Ev::Device(c));
            }
        }
    }

    fn later(&mut self, l: Later) {
        match l {
            Later::Done(addr, label, r) => {
                self.emit(Ev::Busy { addr, label, on: false });
                match r {
                    Ok(m) if !m.is_empty() => self.emit(Ev::Ok(m)),
                    Ok(_) => {}
                    Err(e) => self.emit(Ev::Error(format!("{label}: {e}"))),
                }
            }
            Later::Update(dv) => {
                self.devices.insert(dv.address.clone(), dv.clone());
                self.emit(Ev::Device(dv));
            }
        }
    }

    /// Simulate a slow operation: spinner now, result after `ms`.
    fn op(&self, ltx: &UnboundedSender<Later>, addr: &str, label: &'static str, ms: u64, after: Option<DeviceInfo>, r: Result<String, String>) {
        self.emit(Ev::Busy { addr: addr.to_string(), label, on: true });
        let ltx = ltx.clone();
        let addr = addr.to_string();
        tokio::spawn(async move {
            sleep(Duration::from_millis(ms)).await;
            if let Some(dv) = after {
                let _ = ltx.send(Later::Update(dv));
            }
            let _ = ltx.send(Later::Done(addr, label, r));
        });
    }

    fn command(&mut self, c: Cmd, ltx: &UnboundedSender<Later>) {
        let get = |s: &Self, a: &str| s.devices.get(a).cloned();
        match c {
            Cmd::SelectAdapter(n) => {
                if n == self.adapter.name {
                    return;
                }
                std::mem::swap(&mut self.adapter, &mut self.adapter2);
                self.emit(Ev::ResetDevices);
                self.emit(Ev::Adapter(self.adapter.clone()));
                if self.adapter.name == "hci0" {
                    for x in self.devices.values() {
                        self.emit(Ev::Device(x.clone()));
                    }
                }
            }
            Cmd::Scan(on) => {
                if on && !self.adapter.powered {
                    return self.emit(Ev::Error("Scan: adapter not ready — is it powered on?".into()));
                }
                self.scanning = on;
                self.adapter.discovering = on;
                if !on {
                    self.scan_ticks = 0;
                }
                self.emit(Ev::Adapter(self.adapter.clone()));
            }
            Cmd::Connect(a) => {
                let Some(mut dv) = get(self, &a) else { return };
                if dv.address == "00:1A:7D:DA:71:13" {
                    // The "kitchen speaker" is always off: show a realistic failure.
                    return self.op(ltx, &a, "Connecting", 2500, None, Err("device didn't answer — is it on, in range, and not connected to something else?".into()));
                }
                dv.connected = true;
                if dv.battery.is_none() && dv.uuids.iter().any(|u| u == BATT || u == HFP) {
                    dv.battery = Some(64);
                }
                let msg = format!("Connected to {}", dv.display_name());
                self.op(ltx, &a, "Connecting", 1100, Some(dv), Ok(msg));
            }
            Cmd::Disconnect(a) => {
                let Some(mut dv) = get(self, &a) else { return };
                dv.connected = false;
                let msg = format!("Disconnected {}", dv.display_name());
                self.op(ltx, &a, "Disconnecting", 500, Some(dv), Ok(msg));
            }
            Cmd::Pair(a) => {
                let Some(dv) = get(self, &a) else { return };
                self.emit(Ev::Busy { addr: a.clone(), label: "Pairing", on: true });
                let id = self.next_agent;
                self.next_agent += 1;
                self.pending.insert(id, a.clone());
                let req = if dv.icon == "input-keyboard" {
                    AgentRequest::DisplayPasskey { passkey: 482913, entered: 0 }
                } else if dv.name.is_empty() {
                    AgentRequest::PinCode
                } else {
                    AgentRequest::Confirmation(731052)
                };
                let display = matches!(req, AgentRequest::DisplayPasskey { .. });
                self.emit(Ev::Agent { id, device: a.clone(), req });
                if display {
                    // Pretend the user types on the keyboard, then pairing completes.
                    let ev = self.ev.clone();
                    let ltx = ltx.clone();
                    let mut done = dv.clone();
                    done.paired = true;
                    done.trusted = true;
                    done.connected = true;
                    tokio::spawn(async move {
                        for n in 1..=6u16 {
                            sleep(Duration::from_millis(450)).await;
                            let _ = ev.send(Ev::Agent { id, device: done.address.clone(), req: AgentRequest::DisplayPasskey { passkey: 482913, entered: n } });
                        }
                        sleep(Duration::from_millis(400)).await;
                        let _ = ev.send(Ev::AgentCancelled(id));
                        let msg = format!("Paired with {}", done.display_name());
                        let addr = done.address.clone();
                        let _ = ltx.send(Later::Update(done));
                        let _ = ltx.send(Later::Done(addr, "Pairing", Ok(msg)));
                    });
                }
            }
            Cmd::AgentReply(id, reply) => {
                let Some(a) = self.pending.remove(&id) else { return };
                let Some(mut dv) = get(self, &a) else { return };
                match reply {
                    AgentReply::Reject => {
                        self.emit(Ev::Busy { addr: a, label: "Pairing", on: false });
                        self.emit(Ev::Error("Pairing: pairing cancelled".into()));
                    }
                    _ => {
                        dv.paired = true;
                        dv.bonded = true;
                        dv.trusted = true;
                        dv.connected = true;
                        let msg = format!("Paired with {}", dv.display_name());
                        let ltx = ltx.clone();
                        let addr = a.clone();
                        tokio::spawn(async move {
                            sleep(Duration::from_millis(1300)).await;
                            let _ = ltx.send(Later::Update(dv));
                            let _ = ltx.send(Later::Done(addr, "Pairing", Ok(msg)));
                        });
                    }
                }
            }
            Cmd::Trust(a, on) => {
                let Some(mut dv) = get(self, &a) else { return };
                dv.trusted = on;
                self.op(ltx, &a, "Trust", 150, Some(dv), Ok(String::new()));
            }
            Cmd::Block(a, on) => {
                let Some(mut dv) = get(self, &a) else { return };
                dv.blocked = on;
                if on {
                    dv.connected = false;
                }
                self.op(ltx, &a, "Block", 150, Some(dv), Ok(String::new()));
            }
            Cmd::SetAlias(a, alias) => {
                let Some(mut dv) = get(self, &a) else { return };
                dv.alias = if alias.is_empty() { dv.name.clone() } else { alias };
                self.op(ltx, &a, "Rename", 150, Some(dv), Ok(String::new()));
            }
            Cmd::Remove(a) => {
                if let Some(dv) = self.devices.remove(&a) {
                    self.emit(Ev::DeviceRemoved(a));
                    self.emit(Ev::Ok(format!("Removed {}", dv.display_name())));
                }
            }
            Cmd::SetPowered(on) => {
                self.adapter.powered = on;
                if !on {
                    self.adapter.discovering = false;
                    self.adapter.discoverable = false;
                    self.scanning = false;
                    let mut changed = Vec::new();
                    for dv in self.devices.values_mut().filter(|d| d.connected) {
                        dv.connected = false;
                        changed.push(dv.clone());
                    }
                    for c in changed {
                        self.emit(Ev::Device(c));
                    }
                }
                self.emit(Ev::Adapter(self.adapter.clone()));
            }
            Cmd::SetDiscoverable(on) => {
                self.adapter.discoverable = on;
                self.emit(Ev::Adapter(self.adapter.clone()));
            }
            Cmd::SetPairable(on) => {
                self.adapter.pairable = on;
                self.emit(Ev::Adapter(self.adapter.clone()));
            }
            Cmd::SetAdapterAlias(s) => {
                self.adapter.alias = s;
                self.emit(Ev::Adapter(self.adapter.clone()));
            }
        }
    }
}
