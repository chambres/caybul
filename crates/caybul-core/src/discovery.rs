//! Zero-config peer discovery.
//!
//! The receiver broadcasts a small UDP beacon once a second on every
//! interface; the sender listens briefly and collects the peers it hears.
//! Beacons go out two ways:
//! - IPv4 broadcast (works on LANs, Ethernet cables, Thunderbolt bridges)
//! - IPv6 link-local multicast to ff02::1 (works on links where one side has
//!   no IPv4 at all — e.g. the Mac-to-Mac USB-C link)
//!
//! The discovery port is opened with address/port reuse so several Caybul
//! processes on one machine (the app plus the CLI) can all listen at once.

use crate::protocol::DISCOVERY_PORT;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub const BEACON_MAGIC: &str = "caybul/1";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Beacon {
    pub magic: String,
    pub name: String,
    pub port: u16,
    pub os: String,
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub name: String,
    pub os: String,
    pub addr: SocketAddr,
}

/// Send one round of beacons (receiver side). Best-effort: individual
/// interfaces failing to send is fine.
pub fn beacon_once(name: &str, port: u16) -> anyhow::Result<()> {
    let beacon = Beacon {
        magic: BEACON_MAGIC.into(),
        name: name.into(),
        port,
        os: std::env::consts::OS.into(),
    };
    let payload = serde_json::to_vec(&beacon)?;

    // IPv4 broadcast. Each interface's broadcast is sent from a socket BOUND
    // to that interface's own address — otherwise the OS routes link-local
    // broadcasts (169.254.255.255) out an arbitrary interface and beacons
    // never cross the cable.
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for iface in ifs {
            if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                if let Some(b) = v4.broadcast {
                    if let Ok(sock) = UdpSocket::bind(SocketAddr::new(IpAddr::V4(v4.ip), 0)) {
                        let _ = sock.set_broadcast(true);
                        let _ = sock.send_to(&payload, SocketAddr::new(IpAddr::V4(b), DISCOVERY_PORT));
                    }
                }
            }
        }
    }
    if let Ok(sock) = UdpSocket::bind(("0.0.0.0", 0)) {
        let _ = sock.set_broadcast(true);
        // Global broadcast via the default route, plus loopback so sender and
        // receiver on the same machine can find each other.
        let _ = sock.send_to(
            &payload,
            SocketAddr::new(IpAddr::from([255, 255, 255, 255]), DISCOVERY_PORT),
        );
        let _ = sock.send_to(
            &payload,
            SocketAddr::new(IpAddr::from([127, 0, 0, 1]), DISCOVERY_PORT),
        );
    }

    // IPv6 link-local multicast, one send per interface that has an fe80::
    // address. Reaches peers on links with no IPv4 (Mac-to-Mac USB-C).
    #[cfg(unix)]
    beacon_v6(&payload);

    Ok(())
}

#[cfg(unix)]
fn beacon_v6(payload: &[u8]) {
    use std::ffi::CString;
    use std::net::{Ipv6Addr, SocketAddrV6};
    let Ok(ifs) = if_addrs::get_if_addrs() else {
        return;
    };
    let Ok(sock) = UdpSocket::bind("[::]:0") else {
        return;
    };
    let all_nodes = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
    let mut done: Vec<u32> = Vec::new();
    for iface in ifs {
        let if_addrs::IfAddr::V6(v6) = &iface.addr else {
            continue;
        };
        if v6.ip.segments()[0] != 0xfe80 {
            continue;
        }
        let Ok(cname) = CString::new(iface.name.clone()) else {
            continue;
        };
        let idx = unsafe { libc::if_nametoindex(cname.as_ptr()) };
        if idx == 0 || done.contains(&idx) {
            continue;
        }
        done.push(idx);
        let dest = SocketAddrV6::new(all_nodes, DISCOVERY_PORT, 0, idx);
        let _ = sock.send_to(payload, dest);
    }
}

/// Listen for beacons for `wait` and return the deduplicated peers heard.
pub fn discover(wait: Duration) -> anyhow::Result<Vec<Peer>> {
    let v4 = bind_reusable_v4(DISCOVERY_PORT).map_err(|e| {
        anyhow::anyhow!("could not listen for peers on udp/{DISCOVERY_PORT} ({e})")
    })?;
    v4.set_nonblocking(true)?;
    let v6 = bind_reusable_v6(DISCOVERY_PORT).ok();
    if let Some(s) = &v6 {
        let _ = s.set_nonblocking(true);
    }

    let deadline = Instant::now() + wait;
    let mut peers: Vec<Peer> = Vec::new();
    let mut buf = [0u8; 2048];
    while Instant::now() < deadline {
        let mut got_any = false;
        for sock in std::iter::once(&v4).chain(v6.iter()) {
            while let Ok((n, from)) = sock.recv_from(&mut buf) {
                got_any = true;
                if let Ok(b) = serde_json::from_slice::<Beacon>(&buf[..n]) {
                    if b.magic == BEACON_MAGIC {
                        let addr = SocketAddr::new(from.ip(), b.port);
                        if !peers.iter().any(|p| p.addr == addr) {
                            peers.push(Peer {
                                name: b.name,
                                os: b.os,
                                addr,
                            });
                        }
                    }
                }
            }
        }
        if !got_any {
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    Ok(peers)
}

fn bind_reusable_v4(port: u16) -> anyhow::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let addr: SocketAddr = SocketAddr::new(IpAddr::from([0, 0, 0, 0]), port);
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

fn bind_reusable_v6(port: u16) -> anyhow::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::Ipv6Addr;
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    let _ = socket.set_only_v6(true);
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    let addr: SocketAddr = SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port);
    socket.bind(&addr.into())?;
    Ok(socket.into())
}
