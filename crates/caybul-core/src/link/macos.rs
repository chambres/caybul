//! macOS link detection.
//!
//! - `networksetup -listallhardwareports` maps interfaces (en0, bridge0…) to
//!   human names ("Wi-Fi", "Thunderbolt Bridge") for classification.
//! - `system_profiler SPThunderboltDataType -json` reports each Thunderbolt /
//!   USB4 bus: whether a device is attached, and the negotiated receptacle
//!   speed — that gives us the cable's max speed and USB4 yes/no.

use super::{CableInfo, Link, LinkKind};
use std::collections::HashMap;
use std::process::Command;

pub fn detect() -> Vec<Link> {
    let ports = hardware_ports();
    let cable = thunderbolt_cable_info();
    let mac_usb_present = mac_usb_device_present();

    let mut links: Vec<Link> = Vec::new();
    for (name, ips, is_loopback) in super::interfaces_with_ips() {
        let mut display = ports.get(&name).cloned().unwrap_or_else(|| name.clone());
        let unmapped = !ports.contains_key(&name);
        let lower = display.to_lowercase();
        let kind = if is_loopback {
            LinkKind::Loopback
        } else if is_tunnel(&name) {
            LinkKind::Vpn
        } else if lower.contains("thunderbolt") {
            LinkKind::ThunderboltOrUsb4
        } else if lower.contains("wi-fi") || lower.contains("airport") {
            LinkKind::WiFi
        } else if lower.contains("ethernet") || lower.contains("lan") {
            LinkKind::Ethernet
        } else if unmapped && name.starts_with("en") && mac_usb_present {
            // macOS creates an unnamed NCM network interface when another Mac
            // is attached over a plain USB-C data cable ("Mac" USB device).
            display = "USB-C link (Mac)".into();
            LinkKind::UsbLink
        } else {
            LinkKind::Other
        };
        let cable = match kind {
            LinkKind::ThunderboltOrUsb4 => cable.clone(),
            LinkKind::UsbLink => Some(CableInfo {
                is_usb4: false,
                generation: Some("USB-C data cable (not Thunderbolt)".into()),
                max_speed_gbps: None,
                detail: Some("Mac-to-Mac USB link".into()),
            }),
            _ => None,
        };
        links.push(Link {
            interface: name,
            display,
            kind,
            ips,
            cable,
        });
    }
    links
}

/// Is another Mac attached as a USB device? (That's what the Mac-to-Mac
/// USB-C connection enumerates as.)
fn mac_usb_device_present() -> bool {
    Command::new("ioreg")
        .args(["-p", "IOUSB", "-l"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("\"USB Product Name\" = \"Mac\""))
        .unwrap_or(false)
}

fn is_tunnel(name: &str) -> bool {
    ["utun", "tun", "tap", "ppp", "wg", "ipsec"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Parse `networksetup -listallhardwareports` into device -> port name.
fn hardware_ports() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("networksetup")
        .arg("-listallhardwareports")
        .output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut current_port: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Hardware Port: ") {
            current_port = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("Device: ") {
            if let Some(port) = current_port.take() {
                map.insert(rest.trim().to_string(), port);
            }
        }
    }
    map
}

/// Read the Thunderbolt/USB4 bus report and summarize the active connection.
fn thunderbolt_cable_info() -> Option<CableInfo> {
    let out = Command::new("system_profiler")
        .args(["SPThunderboltDataType", "-json"])
        .output()
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let buses = v.get("SPThunderboltDataType")?.as_array()?;

    let mut best: Option<CableInfo> = None;
    for bus in buses {
        let bus_name = bus.get("_name").and_then(|x| x.as_str()).unwrap_or("");
        let devices: Vec<String> = bus
            .get("_items")
            .and_then(|x| x.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|d| d.get("_name").and_then(|n| n.as_str()))
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let connected = !devices.is_empty();

        // Scan the bus subtree for any "... speed ..." string like
        // "Up to 40 Gb/s x1" and keep the fastest.
        let mut speeds: Vec<(String, f64)> = Vec::new();
        scan_speeds(bus, &mut speeds);
        let max = speeds
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .cloned();

        let text = bus.to_string().to_lowercase();
        let is_usb4 = bus_name.to_lowercase().contains("usb4") || text.contains("usb4");
        let generation = if is_usb4 {
            Some("USB4".to_string())
        } else if bus_name.to_lowercase().contains("thunderbolt") {
            Some("Thunderbolt".to_string())
        } else {
            None
        };

        let info = CableInfo {
            is_usb4,
            generation,
            max_speed_gbps: max.as_ref().map(|(_, s)| *s),
            detail: if connected {
                Some(format!("connected: {}", devices.join(", ")))
            } else {
                None
            },
        };

        // Prefer a bus with something plugged in; otherwise keep any bus info.
        let replace = match &best {
            None => true,
            Some(b) => connected && b.detail.is_none(),
        };
        if replace {
            best = Some(info);
        }
    }
    best
}

/// Recursively collect speed strings ("Up to 40 Gb/s x1") from the JSON tree.
fn scan_speeds(v: &serde_json::Value, out: &mut Vec<(String, f64)>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                if k.to_lowercase().contains("speed") {
                    if let Some(s) = val.as_str() {
                        if let Some(g) = parse_gbps(s) {
                            out.push((s.to_string(), g));
                        }
                    }
                }
                scan_speeds(val, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                scan_speeds(item, out);
            }
        }
        _ => {}
    }
}

/// Extract the Gb/s number from strings like "Up to 40 Gb/s x1".
fn parse_gbps(s: &str) -> Option<f64> {
    let lower = s.to_lowercase();
    let idx = lower.find("gb/s").or_else(|| lower.find("gbps"))?;
    let prefix = &s[..idx];
    let num: String = prefix
        .chars()
        .rev()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    num.parse().ok()
}
