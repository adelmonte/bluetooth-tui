//! Backend interface. The UI never talks to BlueZ directly: it sends
//! [`Cmd`]s and receives [`Ev`]s over channels. Two implementations exist:
//! [`bluez`] (real, via the `bluer` crate, fully event-driven) and
//! [`demo`] (fake devices for UI work on machines without Bluetooth).

pub mod bluez;
pub mod demo;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub type Addr = String;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AdapterInfo {
    pub name: String, // hci0
    pub address: String,
    pub alias: String,
    pub powered: bool,
    pub discoverable: bool,
    pub discoverable_timeout: u32,
    pub pairable: bool,
    pub pairable_timeout: u32,
    pub discovering: bool,
    /// From /sys/class/rfkill: Some(true) soft-blocked, Some(false) not, None unknown.
    pub rfkill_soft: Option<bool>,
    pub rfkill_hard: Option<bool>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceInfo {
    pub address: Addr,
    pub adapter: String,
    /// Advertised/remote name, may be empty.
    pub name: String,
    /// User alias (BlueZ falls back to name or address).
    pub alias: String,
    pub icon: String,
    pub class: Option<u32>,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub blocked: bool,
    pub connected: bool,
    pub rssi: Option<i16>,
    pub tx_power: Option<i16>,
    pub battery: Option<u8>,
    pub uuids: Vec<String>,
    pub legacy_pairing: bool,
    pub services_resolved: bool,
    pub address_type: String,
}

impl DeviceInfo {
    pub fn display_name(&self) -> &str {
        if !self.alias.is_empty() {
            &self.alias
        } else if !self.name.is_empty() {
            &self.name
        } else {
            &self.address
        }
    }

    /// Whether this looks like an audio device (for the audio-tui hint).
    pub fn is_audio(&self) -> bool {
        self.icon.starts_with("audio") || self.uuids.iter().any(|u| uuid_is_audio(u))
    }
}

/// Requests from BlueZ's pairing agent that need the user.
#[derive(Clone, Debug, PartialEq)]
pub enum AgentRequest {
    /// Type a PIN (legacy pairing).
    PinCode,
    /// Type a 6-digit passkey shown on the other device.
    Passkey,
    /// Show this PIN; the user types it on the other device.
    DisplayPinCode(String),
    /// Show this passkey; `entered` digits typed so far on the remote keyboard.
    DisplayPasskey { passkey: u32, entered: u16 },
    /// Does the passkey match what the other device shows?
    Confirmation(u32),
    /// Accept incoming pairing (no passkey).
    Authorization,
    /// Allow a paired device to use a service/profile.
    AuthorizeService(String),
}

#[derive(Clone, Debug)]
pub enum AgentReply {
    Pin(String),
    Passkey(u32),
    Accept,
    Reject,
}

#[derive(Clone, Debug)]
pub enum Cmd {
    SelectAdapter(String),
    Scan(bool),
    Connect(Addr),
    Disconnect(Addr),
    Pair(Addr),
    Trust(Addr, bool),
    Block(Addr, bool),
    Remove(Addr),
    SetAlias(Addr, String),
    SetPowered(bool),
    SetDiscoverable(bool),
    SetPairable(bool),
    SetAdapterAlias(String),
    AgentReply(u64, AgentReply),
}

#[derive(Clone, Debug)]
pub enum Ev {
    /// Backend could not start (no D-Bus, no bluetoothd...). Human readable.
    Unavailable(String),
    /// Full adapter list (sent on start and when adapters come/go).
    Adapters(Vec<AdapterInfo>),
    Adapter(AdapterInfo),
    Device(DeviceInfo),
    DeviceRemoved(Addr),
    /// Clear all devices (adapter switched).
    ResetDevices,
    Agent { id: u64, device: Addr, req: AgentRequest },
    /// BlueZ cancelled an agent request (e.g. timeout on the remote side).
    AgentCancelled(u64),
    /// An operation on a device started/finished; drives row spinners.
    Busy { addr: Addr, label: &'static str, on: bool },
    Ok(String),
    Error(String),
}

pub struct Handle {
    pub tx: UnboundedSender<Cmd>,
    pub rx: UnboundedReceiver<Ev>,
}

/// Bluetooth SIG assigned 16-bit UUIDs we care about, to show friendly profile names.
pub fn uuid_name(uuid: &str) -> Option<&'static str> {
    let short = uuid.get(4..8)?.to_ascii_lowercase();
    if !uuid.to_ascii_lowercase().ends_with("-0000-1000-8000-00805f9b34fb") {
        return None;
    }
    Some(match short.as_str() {
        "1101" => "Serial Port",
        "1103" => "Dial-up Networking",
        "1105" => "Object Push",
        "1106" => "File Transfer",
        "1108" => "Headset",
        "110a" => "Audio Source (A2DP)",
        "110b" => "Audio Sink (A2DP)",
        "110c" => "Remote Control Target",
        "110d" => "Advanced Audio",
        "110e" => "Remote Control (AVRCP)",
        "110f" => "Remote Control Controller",
        "1112" => "Headset Audio Gateway",
        "1115" => "PAN User",
        "1116" => "Network Access Point",
        "1117" => "Group Network",
        "111e" => "Handsfree",
        "111f" => "Handsfree Audio Gateway",
        "1124" => "Human Interface Device",
        "112f" => "Phonebook Access",
        "1132" => "Message Access",
        "1200" => "PnP Information",
        "1203" => "Generic Audio",
        "1800" => "Generic Access",
        "1801" => "Generic Attribute",
        "180a" => "Device Information",
        "180f" => "Battery Service",
        "1812" => "HID over GATT",
        "184e" => "Audio Stream Control (LE Audio)",
        "184f" => "Broadcast Audio Scan",
        "1850" => "Published Audio Capabilities",
        "1853" => "Common Audio",
        _ => return None,
    })
}

fn uuid_is_audio(uuid: &str) -> bool {
    matches!(
        uuid_name(uuid),
        Some(
            "Headset"
                | "Audio Source (A2DP)"
                | "Audio Sink (A2DP)"
                | "Advanced Audio"
                | "Handsfree"
                | "Audio Stream Control (LE Audio)"
                | "Published Audio Capabilities"
        )
    )
}

/// Read rfkill state for bluetooth from sysfs: (soft, hard).
pub fn rfkill_bluetooth() -> (Option<bool>, Option<bool>) {
    let Ok(dir) = std::fs::read_dir("/sys/class/rfkill") else { return (None, None) };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).ok().map(|s| s.trim() == "1");
    for e in dir.flatten() {
        let p = e.path();
        if std::fs::read_to_string(p.join("type")).is_ok_and(|t| t.trim() == "bluetooth") {
            return (read(p.join("soft")), read(p.join("hard")));
        }
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_names() {
        assert_eq!(uuid_name("0000110b-0000-1000-8000-00805f9b34fb"), Some("Audio Sink (A2DP)"));
        assert_eq!(uuid_name("0000180F-0000-1000-8000-00805F9B34FB"), Some("Battery Service"));
        assert_eq!(uuid_name("12345678-1234-1234-1234-123456789abc"), None);
        let d = DeviceInfo { uuids: vec!["0000110b-0000-1000-8000-00805f9b34fb".into()], ..Default::default() };
        assert!(d.is_audio());
    }
}
