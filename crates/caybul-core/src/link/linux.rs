//! Linux link detection via sysfs.
//!
//! - Interface classification from /sys/class/net/<if>: the `wireless` dir
//!   marks Wi-Fi, the device driver symlink identifies thunderbolt-net.
//! - Cable capability from /sys/bus/thunderbolt/devices/*: `generation`
//!   ("USB4" / "Thunderbolt 3" / …) and `rx_speed`/`tx_speed` ("40.0 Gb/s").

use super::{CableInfo, Link, LinkKind};
use std::fs;
use std::path::Path;

pub fn detect() -> Vec<Link> {
    let cable = thunderbolt_cable_info();

    let mut links: Vec<Link> = Vec::new();
    for (name, ips, is_loopback) in super::interfaces_with_ips() {
        let sys = format!("/sys/class/net/{name}");
        let driver = fs::read_link(format!("{sys}/device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_default();

        let is_tunnel = ["tun", "tap", "wg", "ppp", "utun"]
            .iter()
            .any(|p| name.starts_with(p));
        let kind = if is_loopback {
            LinkKind::Loopback
        } else if is_tunnel {
            LinkKind::Vpn
        } else if driver.contains("thunderbolt") || name.starts_with("tb") {
            LinkKind::ThunderboltOrUsb4
        } else if Path::new(&format!("{sys}/wireless")).exists() {
            LinkKind::WiFi
        } else if Path::new(&format!("{sys}/device")).exists() {
            LinkKind::Ethernet
        } else {
            LinkKind::Other
        };

        // Negotiated NIC speed (Mb/s) as extra detail for Ethernet links.
        let nic_speed_mbps: Option<u64> = fs::read_to_string(format!("{sys}/speed"))
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|s| *s > 0)
            .map(|s| s as u64);

        let cable = match kind {
            LinkKind::ThunderboltOrUsb4 => cable.clone(),
            LinkKind::Ethernet => nic_speed_mbps.map(|m| CableInfo {
                is_usb4: false,
                generation: None,
                max_speed_gbps: Some(m as f64 / 1000.0),
                detail: Some(format!("NIC negotiated {m} Mb/s")),
            }),
            _ => None,
        };

        links.push(Link {
            display: display_name(&name, kind),
            interface: name,
            kind,
            ips,
            cable,
        });
    }
    links
}

fn display_name(iface: &str, kind: LinkKind) -> String {
    match kind {
        LinkKind::ThunderboltOrUsb4 => format!("Thunderbolt/USB4 ({iface})"),
        LinkKind::UsbLink => format!("USB link ({iface})"),
        LinkKind::Ethernet => format!("Ethernet ({iface})"),
        LinkKind::WiFi => format!("Wi-Fi ({iface})"),
        LinkKind::Vpn => format!("VPN/Tunnel ({iface})"),
        LinkKind::Loopback => "Loopback".into(),
        LinkKind::Other => iface.to_string(),
    }
}

fn thunderbolt_cable_info() -> Option<CableInfo> {
    let entries = fs::read_dir("/sys/bus/thunderbolt/devices").ok()?;
    let mut best: Option<CableInfo> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        let read = |f: &str| -> Option<String> {
            fs::read_to_string(path.join(f))
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        // Only device routers (they have a generation/speed); skip host paths
        // without them.
        let generation = read("generation");
        let rx = read("rx_speed");
        let tx = read("tx_speed");
        if generation.is_none() && rx.is_none() {
            continue;
        }
        let speed = [rx.as_deref(), tx.as_deref()]
            .iter()
            .flatten()
            .filter_map(|s| parse_gbps(s))
            .fold(None::<f64>, |acc, v| {
                Some(acc.map_or(v, |a: f64| a.max(v)))
            });
        let device = read("device_name");
        let is_usb4 = generation
            .as_deref()
            .map(|g| g.to_lowercase().contains("usb4"))
            .unwrap_or(false);
        let info = CableInfo {
            is_usb4,
            generation,
            max_speed_gbps: speed,
            detail: device.map(|d| format!("connected: {d}")),
        };
        let current = best.as_ref().and_then(|b| b.max_speed_gbps);
        if best.is_none() || info.max_speed_gbps > current {
            best = Some(info);
        }
    }
    best
}

fn parse_gbps(s: &str) -> Option<f64> {
    s.split_whitespace().next()?.parse().ok()
}
