//! Windows link detection via PowerShell `Get-NetAdapter`.
//!
//! InterfaceDescription identifies Thunderbolt/USB4 networking adapters
//! ("Thunderbolt(TM) Networking", "USB4 Host Router"…); LinkSpeed reports the
//! negotiated speed ("40 Gbps", "1 Gbps").

use super::{CableInfo, Link, LinkKind};
use crate::util::quiet_command;
use std::collections::HashMap;

pub fn detect() -> Vec<Link> {
    let adapters = net_adapters();

    let mut links: Vec<Link> = Vec::new();
    for (name, ips, is_loopback) in super::interfaces_with_ips() {
        let info = adapters.get(&name);
        let desc = info.map(|a| a.description.clone()).unwrap_or_default();
        let lower = desc.to_lowercase();
        let kind = if is_loopback {
            LinkKind::Loopback
        } else if lower.contains("thunderbolt") || lower.contains("usb4") {
            LinkKind::ThunderboltOrUsb4
        } else if lower.contains("wi-fi") || lower.contains("wireless") || lower.contains("802.11")
        {
            LinkKind::WiFi
        } else if lower.contains("tap")
            || lower.contains("wintun")
            || lower.contains("wireguard")
            || lower.contains("vpn")
            || lower.contains("tailscale")
        {
            LinkKind::Vpn
        } else if !desc.is_empty() {
            LinkKind::Ethernet
        } else {
            LinkKind::Other
        };

        let cable = info.and_then(|a| {
            let speed = parse_gbps(&a.link_speed);
            if kind == LinkKind::ThunderboltOrUsb4 || kind == LinkKind::Ethernet {
                Some(CableInfo {
                    is_usb4: lower.contains("usb4"),
                    generation: if lower.contains("usb4") {
                        Some("USB4".into())
                    } else if lower.contains("thunderbolt") {
                        Some("Thunderbolt".into())
                    } else {
                        None
                    },
                    max_speed_gbps: speed,
                    detail: Some(a.description.clone()),
                })
            } else {
                None
            }
        });

        links.push(Link {
            display: if desc.is_empty() { name.clone() } else { desc },
            interface: name,
            kind,
            ips,
            cable,
        });
    }
    links
}

struct Adapter {
    description: String,
    link_speed: String,
}

fn net_adapters() -> HashMap<String, Adapter> {
    let mut map = HashMap::new();
    let Ok(out) = quiet_command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-NetAdapter | Select-Object Name,InterfaceDescription,LinkSpeed | ConvertTo-Json",
        ])
        .output()
    else {
        return map;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return map;
    };
    let items: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        obj => vec![obj],
    };
    for item in items {
        let name = item.get("Name").and_then(|x| x.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        map.insert(
            name.to_string(),
            Adapter {
                description: item
                    .get("InterfaceDescription")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                link_speed: item
                    .get("LinkSpeed")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            },
        );
    }
    map
}

/// "40 Gbps" -> 40.0, "1 Gbps" -> 1.0, "100 Mbps" -> 0.1
fn parse_gbps(s: &str) -> Option<f64> {
    let lower = s.to_lowercase();
    let num: f64 = lower
        .split_whitespace()
        .next()?
        .replace(',', ".")
        .parse()
        .ok()?;
    if lower.contains("gbps") || lower.contains("gb/s") {
        Some(num)
    } else if lower.contains("mbps") || lower.contains("mb/s") {
        Some(num / 1000.0)
    } else {
        None
    }
}
