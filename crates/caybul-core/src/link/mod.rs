//! Physical link detection and classification.
//!
//! v1 keeps this deliberately simple: we only consider links the OS already
//! exposes as an IP interface (Thunderbolt/USB4 host-to-host bridges, direct
//! Ethernet cables, ordinary LAN/Wi-Fi). For Thunderbolt/USB4 links we also
//! read what the cable actually negotiated: max speed and whether it is USB4.

use std::net::IpAddr;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    /// Thunderbolt 3/4 or USB4 host-to-host network bridge.
    ThunderboltOrUsb4,
    /// Direct USB data link between two computers (e.g. the Mac-to-Mac
    /// USB-C link macOS creates over a plain, non-Thunderbolt cable).
    UsbLink,
    /// Wired Ethernet (direct cable or LAN).
    Ethernet,
    WiFi,
    /// VPN / tunnel interfaces (utun, wg, tun/tap): never a cable.
    Vpn,
    Loopback,
    Other,
}

impl LinkKind {
    pub fn label(&self) -> &'static str {
        match self {
            LinkKind::ThunderboltOrUsb4 => "Thunderbolt/USB4",
            LinkKind::UsbLink => "USB cable link",
            LinkKind::Ethernet => "Ethernet",
            LinkKind::WiFi => "Wi-Fi",
            LinkKind::Vpn => "VPN/Tunnel",
            LinkKind::Loopback => "Loopback",
            LinkKind::Other => "Other",
        }
    }

    /// Preference order when choosing the best path (lower is better).
    fn rank(&self) -> u8 {
        match self {
            LinkKind::ThunderboltOrUsb4 => 0,
            LinkKind::UsbLink => 1,
            LinkKind::Ethernet => 2,
            LinkKind::Other => 3,
            LinkKind::WiFi => 4,
            LinkKind::Vpn => 5,
            LinkKind::Loopback => 6,
        }
    }
}

/// What the cable/link actually negotiated, read from the OS.
#[derive(Debug, Clone, Default)]
pub struct CableInfo {
    /// True when the host-to-host connection is running as USB4.
    pub is_usb4: bool,
    /// e.g. "USB4", "Thunderbolt 3", "Thunderbolt 4".
    pub generation: Option<String>,
    /// Negotiated max link speed in Gbit/s (the cable's usable ceiling).
    pub max_speed_gbps: Option<f64>,
    /// Free-form extra detail (connected device name, raw speed string).
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Link {
    /// OS interface name, e.g. "bridge0", "en5", "tb0", "Ethernet 2".
    pub interface: String,
    /// Human-readable name, e.g. "Thunderbolt Bridge".
    pub display: String,
    pub kind: LinkKind,
    pub ips: Vec<IpAddr>,
    pub cable: Option<CableInfo>,
}

impl Link {
    pub fn summary(&self) -> String {
        let ips: Vec<String> = self
            .ips
            .iter()
            .filter(|ip| ip.is_ipv4())
            .map(|ip| ip.to_string())
            .collect();
        let ips = if ips.is_empty() {
            self.ips
                .iter()
                .map(|ip| ip.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            ips.join(", ")
        };
        format!(
            "{:<10} {:<24} [{}]  {}",
            self.interface,
            self.display,
            self.kind.label(),
            ips
        )
    }

    pub fn cable_summary(&self) -> Option<String> {
        let c = self.cable.as_ref()?;
        let mut parts: Vec<String> = Vec::new();
        if c.is_usb4 {
            parts.push("USB4 connection: yes".into());
        } else if let Some(g) = &c.generation {
            parts.push(format!("connection: {g}"));
        }
        if let Some(s) = c.max_speed_gbps {
            parts.push(format!("cable max speed: {s} Gb/s"));
        }
        if let Some(d) = &c.detail {
            parts.push(d.clone());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(", "))
        }
    }
}

/// Enumerate usable links, best first.
pub fn detect_links() -> Vec<Link> {
    #[cfg(target_os = "macos")]
    let mut links = macos::detect();
    #[cfg(target_os = "linux")]
    let mut links = linux::detect();
    #[cfg(target_os = "windows")]
    let mut links = windows::detect();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let mut links: Vec<Link> = generic_detect();

    links.sort_by_key(|l| l.kind.rank());
    links
}

/// Best non-loopback link, if any.
pub fn best_link() -> Option<Link> {
    detect_links()
        .into_iter()
        .find(|l| l.kind != LinkKind::Loopback)
}

/// Local IP the OS would use to reach `peer` (routing-table lookup via a
/// connected UDP socket — nothing is actually sent).
pub fn local_ip_for(peer: IpAddr) -> Option<IpAddr> {
    use std::net::UdpSocket;
    let bind_addr = if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
    let sock = UdpSocket::bind(bind_addr).ok()?;
    sock.connect((peer, 9)).ok()?;
    sock.local_addr().ok().map(|a| a.ip())
}

/// Which detected link a connection to `peer` would actually travel over.
/// This is how the app distinguishes "paired through the cable" from
/// "paired over Wi-Fi".
pub fn route_link<'a>(links: &'a [Link], peer: IpAddr) -> Option<&'a Link> {
    // Dual-stack listeners hand us v4-mapped v6 addresses; normalize first.
    let peer = match peer {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        p => p,
    };
    if peer.is_loopback() {
        return links.iter().find(|l| l.kind == LinkKind::Loopback);
    }
    // If the peer sits inside a subnet that one of our own non-VPN
    // interfaces directly owns, that interface IS the physical path —
    // regardless of what the routing table says (VPNs like Tailscale
    // sometimes capture these routes and would mislabel/slow the link).
    if let IpAddr::V4(v4) = peer {
        if let Some(l) = subnet_link(links, v4) {
            return Some(l);
        }
        // Link-local peer but no matching subnet yet (address still
        // settling): any link-local-bearing interface is the best guess.
        if v4.is_link_local() {
            if let Some(l) = links.iter().find(|l| {
                l.ips
                    .iter()
                    .any(|ip| matches!(ip, IpAddr::V4(x) if x.is_link_local()))
            }) {
                return Some(l);
            }
        }
    }
    let local = local_ip_for(peer)?;
    links.iter().find(|l| l.ips.contains(&local))
}

/// Longest-prefix match: the local non-VPN interface whose IPv4 subnet
/// contains `peer` — i.e., the peer is directly attached to that wire.
fn subnet_link<'a>(links: &'a [Link], peer: std::net::Ipv4Addr) -> Option<&'a Link> {
    let mut best: Option<(&'a Link, u32)> = None;
    let Ok(ifs) = if_addrs::get_if_addrs() else {
        return None;
    };
    for iface in &ifs {
        let if_addrs::IfAddr::V4(v4) = &iface.addr else {
            continue;
        };
        let mask = u32::from(v4.netmask);
        if mask == 0 || u32::from(peer) & mask != u32::from(v4.ip) & mask {
            continue;
        }
        let Some(l) = links.iter().find(|l| l.interface == iface.name) else {
            continue;
        };
        if l.kind == LinkKind::Vpn || l.kind == LinkKind::Loopback {
            continue;
        }
        let prefix = mask.count_ones();
        if best.map(|(_, b)| prefix > b).unwrap_or(true) {
            best = Some((l, prefix));
        }
    }
    best.map(|(l, _)| l)
}

/// True when `peer` is reached over a wire (Thunderbolt/USB4, the Mac-to-Mac
/// USB link, or Ethernet). A physical cable is treated as consent, so
/// pairing skips the code/confirmation for these peers; Wi-Fi and
/// VPN/network peers always verify.
pub fn is_direct_cable_peer(peer: IpAddr) -> bool {
    let links = detect_links();
    matches!(
        route_link(&links, peer).map(|l| l.kind),
        Some(LinkKind::ThunderboltOrUsb4 | LinkKind::UsbLink | LinkKind::Ethernet)
    )
}

/// Recommended number of parallel TCP streams for a link kind.
pub fn recommended_streams(kind: LinkKind) -> u32 {
    match kind {
        LinkKind::ThunderboltOrUsb4 => 8,
        LinkKind::UsbLink => 4,
        LinkKind::Ethernet => 4,
        LinkKind::Other => 4,
        LinkKind::WiFi => 2,
        LinkKind::Vpn => 2,
        LinkKind::Loopback => 4,
    }
}

/// Fallback for unsupported platforms: interface list only, no classification.
#[allow(dead_code)]
fn generic_detect() -> Vec<Link> {
    let mut links: Vec<Link> = Vec::new();
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for iface in ifs {
            let ip = iface.ip();
            let kind = if iface.is_loopback() {
                LinkKind::Loopback
            } else {
                LinkKind::Other
            };
            if let Some(l) = links.iter_mut().find(|l| l.interface == iface.name) {
                l.ips.push(ip);
            } else {
                links.push(Link {
                    display: iface.name.clone(),
                    interface: iface.name,
                    kind,
                    ips: vec![ip],
                    cable: None,
                });
            }
        }
    }
    links
}

/// Group `if_addrs` results into (interface name -> ips), preserving order.
#[allow(dead_code)]
pub(crate) fn interfaces_with_ips() -> Vec<(String, Vec<IpAddr>, bool)> {
    let mut out: Vec<(String, Vec<IpAddr>, bool)> = Vec::new();
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for iface in ifs {
            let ip = iface.ip();
            let lo = iface.is_loopback();
            if let Some(e) = out.iter_mut().find(|e| e.0 == iface.name) {
                e.1.push(ip);
            } else {
                out.push((iface.name, vec![ip], lo));
            }
        }
    }
    out
}
