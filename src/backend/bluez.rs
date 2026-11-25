//! Real backend: BlueZ over D-Bus via `bluer`.
//!
//! Unlike bluetui/impala (which re-read everything every second) this is
//! fully event-driven: adapter and per-device property streams push changes
//! as they happen, so the UI is instant and D-Bus stays quiet.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bluer::agent::{Agent, AgentHandle, ReqError, ReqResult};
use bluer::{Adapter, AdapterEvent, AdapterProperty, Address, Device, DeviceEvent, DeviceProperty, ErrorKind, Session, SessionEvent};
use futures::StreamExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::{rfkill_bluetooth, AdapterInfo, AgentReply, AgentRequest, Cmd, DeviceInfo, Ev, Handle};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub trust_on_pair: bool,
    pub connect_on_pair: bool,
}

pub fn spawn(opts: Options) -> Handle {
    let (tx, cmd_rx) = unbounded_channel();
    let (ev_tx, rx) = unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run(cmd_rx, ev_tx.clone(), opts).await {
            let _ = ev_tx.send(Ev::Unavailable(format!(
                "Can't talk to BlueZ: {e}. Is bluetooth.service running? (systemctl status bluetooth)"
            )));
        }
    });
    Handle { tx, rx }
}

enum Internal {
    Adapter(String, AdapterEvent),
    Device(Address, DeviceProperty),
    Session(SessionEvent),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<AgentReply>>>>;

struct Backend {
    session: Session,
    opts: Options,
    adapter: Option<Adapter>,
    devices: HashMap<Address, DeviceInfo>,
    watchers: HashMap<Address, JoinHandle<()>>,
    adapter_watch: Option<JoinHandle<()>>,
    scan: Option<JoinHandle<()>>,
    pending: Pending,
    ev: UnboundedSender<Ev>,
    internal: UnboundedSender<Internal>,
}

async fn run(mut cmds: UnboundedReceiver<Cmd>, ev: UnboundedSender<Ev>, opts: Options) -> bluer::Result<()> {
    let session = Session::new().await?;
    let pending: Pending = Arc::default();
    let _agent: AgentHandle = session.register_agent(make_agent(ev.clone(), pending.clone())).await?;
    let (itx, mut irx) = unbounded_channel();

    {
        let mut sess_events = Box::pin(session.events().await?);
        let itx = itx.clone();
        tokio::spawn(async move {
            while let Some(e) = sess_events.next().await {
                if itx.send(Internal::Session(e)).is_err() {
                    break;
                }
            }
        });
    }

    let mut b = Backend {
        session,
        opts,
        adapter: None,
        devices: HashMap::new(),
        watchers: HashMap::new(),
        adapter_watch: None,
        scan: None,
        pending,
        ev,
        internal: itx,
    };
    b.send_adapters().await;
    if let Ok(a) = b.session.default_adapter().await {
        b.select_adapter(a.name().to_string()).await;
    }

    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                Some(c) => b.command(c).await,
                None => break, // UI gone
            },
            Some(i) = irx.recv() => b.internal(i).await,
        }
    }
    Ok(())
}

impl Backend {
    fn emit(&self, e: Ev) {
        let _ = self.ev.send(e);
    }

    async fn send_adapters(&self) {
        let names = self.session.adapter_names().await.unwrap_or_default();
        let mut infos = Vec::new();
        for n in names {
            if let Ok(a) = self.session.adapter(&n)
                && let Ok(i) = adapter_info(&a).await {
                    infos.push(i);
                }
        }
        self.emit(Ev::Adapters(infos));
    }

    async fn select_adapter(&mut self, name: String) {
        for (_, w) in self.watchers.drain() {
            w.abort();
        }
        if let Some(w) = self.adapter_watch.take() {
            w.abort();
        }
        if let Some(s) = self.scan.take() {
            s.abort();
        }
        self.devices.clear();
        self.emit(Ev::ResetDevices);
        let adapter = match self.session.adapter(&name) {
            Ok(a) => a,
            Err(e) => return self.emit(Ev::Error(friendly(&e))),
        };
        match adapter.events().await {
            Ok(s) => {
                let mut s = Box::pin(s);
                let itx = self.internal.clone();
                let n = name.clone();
                self.adapter_watch = Some(tokio::spawn(async move {
                    while let Some(e) = s.next().await {
                        if itx.send(Internal::Adapter(n.clone(), e)).is_err() {
                            break;
                        }
                    }
                }));
            }
            Err(e) => self.emit(Ev::Error(friendly(&e))),
        }
        if let Ok(i) = adapter_info(&adapter).await {
            self.emit(Ev::Adapter(i));
        }
        let addrs = adapter.device_addresses().await.unwrap_or_default();
        self.adapter = Some(adapter);
        for a in addrs {
            self.add_device(a).await;
        }
    }

    async fn add_device(&mut self, addr: Address) {
        let Some(adapter) = &self.adapter else { return };
        let Ok(dev) = adapter.device(addr) else { return };
        match device_info(&dev).await {
            Ok(info) => {
                self.emit(Ev::Device(info.clone()));
                self.devices.insert(addr, info);
            }
            Err(e) => tracing::debug!("device {addr}: {e}"),
        }
        if !self.watchers.contains_key(&addr)
            && let Ok(s) = dev.events().await {
                let mut s = Box::pin(s);
                let itx = self.internal.clone();
                self.watchers.insert(
                    addr,
                    tokio::spawn(async move {
                        while let Some(DeviceEvent::PropertyChanged(p)) = s.next().await {
                            if itx.send(Internal::Device(addr, p)).is_err() {
                                break;
                            }
                        }
                    }),
                );
            }
    }

    async fn internal(&mut self, i: Internal) {
        match i {
            Internal::Session(SessionEvent::AdapterAdded(n)) => {
                self.send_adapters().await;
                if self.adapter.is_none() {
                    self.select_adapter(n).await;
                }
            }
            Internal::Session(SessionEvent::AdapterRemoved(n)) => {
                if self.adapter.as_ref().is_some_and(|a| a.name() == n) {
                    self.adapter = None;
                    for (_, w) in self.watchers.drain() {
                        w.abort();
                    }
                    self.devices.clear();
                    self.emit(Ev::ResetDevices);
                }
                self.send_adapters().await;
                if self.adapter.is_none()
                    && let Ok(a) = self.session.default_adapter().await {
                        self.select_adapter(a.name().to_string()).await;
                    }
            }
            Internal::Adapter(n, e) => {
                if !self.adapter.as_ref().is_some_and(|a| a.name() == n) {
                    return;
                }
                match e {
                    AdapterEvent::DeviceAdded(a) => {
                        if !self.devices.contains_key(&a) {
                            self.add_device(a).await;
                        }
                    }
                    AdapterEvent::DeviceRemoved(a) => {
                        self.devices.remove(&a);
                        if let Some(w) = self.watchers.remove(&a) {
                            w.abort();
                        }
                        self.emit(Ev::DeviceRemoved(a.to_string()));
                    }
                    AdapterEvent::PropertyChanged(_) => {
                        if let Some(a) = &self.adapter
                            && let Ok(i) = adapter_info(a).await {
                                self.emit(Ev::Adapter(i));
                            }
                    }
                }
            }
            Internal::Device(addr, p) => {
                if let Some(info) = self.devices.get_mut(&addr) {
                    apply_device_prop(info, p);
                    let info = info.clone();
                    self.emit(Ev::Device(info));
                }
            }
        }
    }

    fn device(&self, addr: &str) -> Option<Device> {
        let a: Address = addr.parse().ok()?;
        self.adapter.as_ref()?.device(a).ok()
    }

    /// Run a device operation in the background with a row spinner and a toast.
    fn device_op<F, Fut>(&self, addr: String, label: &'static str, ok_msg: String, f: F)
    where
        F: FnOnce(Device) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = bluer::Result<()>> + Send,
    {
        let Some(dev) = self.device(&addr) else {
            return self.emit(Ev::Error(format!("Unknown device {addr}")));
        };
        let ev = self.ev.clone();
        tokio::spawn(async move {
            let _ = ev.send(Ev::Busy { addr: addr.clone(), label, on: true });
            let r = f(dev).await;
            let _ = ev.send(Ev::Busy { addr: addr.clone(), label, on: false });
            match r {
                Ok(()) if !ok_msg.is_empty() => {
                    let _ = ev.send(Ev::Ok(ok_msg));
                }
                Ok(()) => {}
                Err(e) => {
                    let _ = ev.send(Ev::Error(format!("{label}: {}", friendly(&e))));
                }
            }
        });
    }

    fn adapter_op<F, Fut>(&self, f: F)
    where
        F: FnOnce(Adapter) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = bluer::Result<()>> + Send,
    {
        let Some(a) = self.adapter.clone() else {
            return self.emit(Ev::Error("No adapter selected".into()));
        };
        let ev = self.ev.clone();
        tokio::spawn(async move {
            if let Err(e) = f(a).await {
                let _ = ev.send(Ev::Error(friendly(&e)));
            }
        });
    }

    fn name_of(&self, addr: &str) -> String {
        addr.parse::<Address>()
            .ok()
            .and_then(|a| self.devices.get(&a))
            .map(|d| d.display_name().to_string())
            .unwrap_or_else(|| addr.to_string())
    }

    async fn command(&mut self, c: Cmd) {
        match c {
            Cmd::SelectAdapter(n) => self.select_adapter(n).await,
            Cmd::Scan(on) => {
                if on {
                    if self.scan.is_some() {
                        return;
                    }
                    let Some(a) = self.adapter.clone() else { return };
                    let ev = self.ev.clone();
                    self.scan = Some(tokio::spawn(async move {
                        match a.discover_devices().await {
                            // Keep the stream alive: dropping it stops discovery.
                            // New devices arrive through the adapter event watcher.
                            Ok(s) => {
                                let mut s = Box::pin(s);
                                while s.next().await.is_some() {}
                            }
                            Err(e) => {
                                let _ = ev.send(Ev::Error(format!("Scan: {}", friendly(&e))));
                            }
                        }
                    }));
                } else if let Some(s) = self.scan.take() {
                    s.abort();
                }
            }
            Cmd::Connect(a) => {
                let n = self.name_of(&a);
                self.device_op(a, "Connecting", format!("Connected to {n}"), |d| async move { d.connect().await })
            }
            Cmd::Disconnect(a) => {
                let n = self.name_of(&a);
                self.device_op(a, "Disconnecting", format!("Disconnected {n}"), |d| async move { d.disconnect().await })
            }
            Cmd::Pair(a) => {
                let n = self.name_of(&a);
                let opts = self.opts;
                self.device_op(a, "Pairing", format!("Paired with {n}"), move |d| async move {
                    d.pair().await?;
                    if opts.trust_on_pair {
                        d.set_trusted(true).await?;
                    }
                    if opts.connect_on_pair {
                        d.connect().await?;
                    }
                    Ok(())
                })
            }
            Cmd::Trust(a, on) => self.device_op(a, "Trust", String::new(), move |d| async move { d.set_trusted(on).await }),
            Cmd::Block(a, on) => self.device_op(a, "Block", String::new(), move |d| async move { d.set_blocked(on).await }),
            Cmd::SetAlias(a, alias) => {
                self.device_op(a, "Rename", String::new(), move |d| async move { d.set_alias(alias).await })
            }
            Cmd::Remove(a) => {
                let n = self.name_of(&a);
                let Ok(addr) = a.parse::<Address>() else { return };
                let ev = self.ev.clone();
                self.adapter_op(move |ad| async move {
                    ad.remove_device(addr).await?;
                    let _ = ev.send(Ev::Ok(format!("Removed {n}")));
                    Ok(())
                })
            }
            Cmd::SetPowered(on) => self.adapter_op(move |a| async move { a.set_powered(on).await }),
            Cmd::SetDiscoverable(on) => self.adapter_op(move |a| async move { a.set_discoverable(on).await }),
            Cmd::SetPairable(on) => self.adapter_op(move |a| async move { a.set_pairable(on).await }),
            Cmd::SetAdapterAlias(s) => self.adapter_op(move |a| async move { a.set_alias(s).await }),
            Cmd::AgentReply(id, r) => {
                if let Some(tx) = self.pending.lock().expect("agent map").remove(&id) {
                    let _ = tx.send(r);
                }
            }
        }
    }
}

async fn adapter_info(a: &Adapter) -> bluer::Result<AdapterInfo> {
    let mut i = AdapterInfo { name: a.name().to_string(), ..Default::default() };
    for p in a.all_properties().await? {
        match p {
            AdapterProperty::Address(v) => i.address = v.to_string(),
            AdapterProperty::Alias(v) => i.alias = v,
            AdapterProperty::Powered(v) => i.powered = v,
            AdapterProperty::Discoverable(v) => i.discoverable = v,
            AdapterProperty::DiscoverableTimeout(v) => i.discoverable_timeout = v,
            AdapterProperty::Pairable(v) => i.pairable = v,
            AdapterProperty::PairableTimeout(v) => i.pairable_timeout = v,
            AdapterProperty::Discovering(v) => i.discovering = v,
            _ => {}
        }
    }
    (i.rfkill_soft, i.rfkill_hard) = rfkill_bluetooth();
    Ok(i)
}

async fn device_info(d: &Device) -> bluer::Result<DeviceInfo> {
    let mut i = DeviceInfo { address: d.address().to_string(), adapter: d.adapter_name().to_string(), ..Default::default() };
    for p in d.all_properties().await? {
        apply_device_prop(&mut i, p);
    }
    Ok(i)
}

fn apply_device_prop(i: &mut DeviceInfo, p: DeviceProperty) {
    match p {
        DeviceProperty::Name(v) => i.name = v,
        DeviceProperty::Alias(v) => i.alias = v,
        DeviceProperty::Icon(v) => i.icon = v,
        DeviceProperty::Class(v) => i.class = Some(v),
        DeviceProperty::AddressType(v) => i.address_type = v.to_string(),
        DeviceProperty::Uuids(v) => {
            let mut u: Vec<String> = v.iter().map(|u| u.to_string()).collect();
            u.sort();
            i.uuids = u;
        }
        DeviceProperty::Paired(v) => i.paired = v,
        DeviceProperty::Connected(v) => i.connected = v,
        DeviceProperty::Trusted(v) => i.trusted = v,
        DeviceProperty::Blocked(v) => i.blocked = v,
        DeviceProperty::LegacyPairing(v) => i.legacy_pairing = v,
        DeviceProperty::Rssi(v) => i.rssi = Some(v),
        DeviceProperty::TxPower(v) => i.tx_power = Some(v),
        DeviceProperty::ServicesResolved(v) => i.services_resolved = v,
        DeviceProperty::BatteryPercentage(v) => i.battery = Some(v),
        _ => {}
    }
}

/// Turn BlueZ's terse errors into something a person can act on.
pub fn friendly(e: &bluer::Error) -> String {
    let m = e.message.as_str();
    let hint = if m.contains("page-timeout") || m.contains("Page Timeout") || m.contains("Host is down") {
        "device didn't answer — is it on, in range, and not connected to something else?"
    } else if m.contains("profile-unavailable") {
        "no usable profile — for headphones, make sure PipeWire/WirePlumber is running"
    } else if m.contains("Resource temporarily unavailable") {
        "device busy — try again in a moment"
    } else {
        match e.kind {
            ErrorKind::AuthenticationFailed => "pairing failed — wrong PIN or the device rejected it",
            ErrorKind::AuthenticationCanceled => "pairing cancelled",
            ErrorKind::AuthenticationRejected => "the device rejected pairing",
            ErrorKind::AuthenticationTimeout => "pairing timed out — confirm on the device sooner",
            ErrorKind::ConnectionAttemptFailed => "connection attempt failed",
            ErrorKind::AlreadyConnected => "already connected",
            ErrorKind::AlreadyExists => "already paired",
            ErrorKind::InProgress => "already in progress",
            ErrorKind::NotReady => "adapter not ready — is it powered on?",
            ErrorKind::DoesNotExist => "device no longer exists",
            ErrorKind::NotAuthorized | ErrorKind::NotPermitted => "not permitted (polkit?)",
            _ => "",
        }
    };
    match (hint.is_empty(), m.is_empty()) {
        (true, true) => e.kind.to_string(),
        (true, false) => m.to_string(),
        (false, _) => hint.to_string(),
    }
}

// ---------------------------------------------------------------- agent

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Sends `AgentCancelled` if BlueZ drops the request before the user answers.
struct CancelGuard {
    id: u64,
    ev: UnboundedSender<Ev>,
    pending: Pending,
    done: bool,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.done {
            self.pending.lock().expect("agent map").remove(&self.id);
            let _ = self.ev.send(Ev::AgentCancelled(self.id));
        }
    }
}

async fn ask(ev: &UnboundedSender<Ev>, pending: &Pending, device: Address, req: AgentRequest) -> ReqResult<AgentReply> {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    pending.lock().expect("agent map").insert(id, tx);
    let mut guard = CancelGuard { id, ev: ev.clone(), pending: pending.clone(), done: false };
    let _ = ev.send(Ev::Agent { id, device: device.to_string(), req });
    let r = rx.await;
    guard.done = true;
    match r {
        Ok(AgentReply::Reject) => Err(ReqError::Rejected),
        Ok(r) => Ok(r),
        Err(_) => Err(ReqError::Canceled),
    }
}

fn show(ev: &UnboundedSender<Ev>, device: Address, req: AgentRequest, cancel: oneshot::Receiver<()>) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let _ = ev.send(Ev::Agent { id, device: device.to_string(), req });
    let ev = ev.clone();
    tokio::spawn(async move {
        let _ = cancel.await;
        let _ = ev.send(Ev::AgentCancelled(id));
    });
}

fn make_agent(ev: UnboundedSender<Ev>, pending: Pending) -> Agent {
    let (e1, p1) = (ev.clone(), pending.clone());
    let (e2, p2) = (ev.clone(), pending.clone());
    let (e3, p3) = (ev.clone(), pending.clone());
    let (e4, p4) = (ev.clone(), pending.clone());
    let (e5, p5) = (ev.clone(), pending.clone());
    let e6 = ev.clone();
    let e7 = ev.clone();
    Agent {
        request_default: true,
        request_pin_code: Some(Box::new(move |r| {
            let (e, p) = (e1.clone(), p1.clone());
            Box::pin(async move {
                match ask(&e, &p, r.device, AgentRequest::PinCode).await? {
                    AgentReply::Pin(s) => Ok(s),
                    _ => Err(ReqError::Rejected),
                }
            })
        })),
        request_passkey: Some(Box::new(move |r| {
            let (e, p) = (e2.clone(), p2.clone());
            Box::pin(async move {
                match ask(&e, &p, r.device, AgentRequest::Passkey).await? {
                    AgentReply::Passkey(k) => Ok(k),
                    _ => Err(ReqError::Rejected),
                }
            })
        })),
        request_confirmation: Some(Box::new(move |r| {
            let (e, p) = (e3.clone(), p3.clone());
            Box::pin(async move { ask(&e, &p, r.device, AgentRequest::Confirmation(r.passkey)).await.map(|_| ()) })
        })),
        request_authorization: Some(Box::new(move |r| {
            let (e, p) = (e4.clone(), p4.clone());
            Box::pin(async move { ask(&e, &p, r.device, AgentRequest::Authorization).await.map(|_| ()) })
        })),
        authorize_service: Some(Box::new(move |r| {
            let (e, p) = (e5.clone(), p5.clone());
            Box::pin(async move {
                let svc = super::uuid_name(&r.service.to_string()).map(str::to_string).unwrap_or_else(|| r.service.to_string());
                ask(&e, &p, r.device, AgentRequest::AuthorizeService(svc)).await.map(|_| ())
            })
        })),
        display_pin_code: Some(Box::new(move |r| {
            show(&e6, r.device, AgentRequest::DisplayPinCode(r.pincode), r.cancel);
            Box::pin(async { Ok(()) })
        })),
        display_passkey: Some(Box::new(move |r| {
            show(&e7, r.device, AgentRequest::DisplayPasskey { passkey: r.passkey, entered: r.entered }, r.cancel);
            Box::pin(async { Ok(()) })
        })),
        ..Default::default()
    }
}
