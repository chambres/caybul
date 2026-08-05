use std::fs::File;
use std::io;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

pub fn hex_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(s.len() % 2 == 0, "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

pub fn bitmap_len(chunks: u64) -> usize {
    ((chunks + 7) / 8) as usize
}

pub fn bitmap_get(bm: &[u8], idx: u64) -> bool {
    let i = (idx / 8) as usize;
    i < bm.len() && bm[i] & (1 << (idx % 8)) != 0
}

pub fn bitmap_set(bm: &mut [u8], idx: u64) {
    bm[(idx / 8) as usize] |= 1 << (idx % 8);
}

#[cfg(unix)]
pub fn read_exact_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.read_exact_at(buf, off)
}

#[cfg(unix)]
pub fn write_all_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    f.write_all_at(buf, off)
}

#[cfg(windows)]
pub fn read_exact_at(f: &File, mut buf: &mut [u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_read(buf, off)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected eof"));
        }
        buf = &mut buf[n..];
        off += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
pub fn write_all_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Random-enough session token without pulling in a RNG crate. A process-wide
/// counter guarantees uniqueness even for back-to-back calls.
pub fn new_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    blake3::hash(format!("caybul:{t}:{pid}:{c}").as_bytes())
        .to_hex()
        .to_string()
}

pub fn hostname() -> String {
    quiet_command("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "caybul-device".into())
}

/// Normalize IPv4-mapped IPv6 addresses (::ffff:a.b.c.d, produced by
/// dual-stack listeners) back to plain IPv4 so link-local handling and route
/// classification see them correctly.
pub fn canonical_addr(addr: SocketAddr) -> SocketAddr {
    if let IpAddr::V6(v6) = addr.ip() {
        if let Some(v4) = v6.to_ipv4_mapped() {
            return SocketAddr::new(IpAddr::V4(v4), addr.port());
        }
    }
    addr
}

/// Connect with a timeout. If the peer's address is inside a subnet one of
/// our own physical interfaces directly owns, bind the source to that
/// interface so the traffic goes over the wire — otherwise the OS (or a VPN
/// like Tailscale capturing routes) may send it out the wrong interface.
/// With several matching interfaces, all candidates race and the first
/// completed handshake wins.
pub fn connect_cable_aware(addr: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    let addr = canonical_addr(addr);
    if let IpAddr::V4(v4) = addr.ip() {
        let sources = candidate_sources(v4);
        match sources.len() {
            0 => {}
            1 => return connect_from(Some(sources[0]), addr, timeout),
            _ => {
                let (tx, rx) = std::sync::mpsc::channel();
                for src in sources {
                    let tx = tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(connect_from(Some(src), addr, timeout));
                    });
                }
                drop(tx);
                let mut last_err: Option<io::Error> = None;
                while let Ok(result) = rx.recv() {
                    match result {
                        Ok(stream) => return Ok(stream),
                        Err(e) => last_err = Some(e),
                    }
                }
                return Err(last_err
                    .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "connect failed")));
            }
        }
    }
    TcpStream::connect_timeout(&addr, timeout)
}

/// Local addresses on directly-attached (non-tunnel) interfaces whose subnet
/// contains `peer`, longest prefix first.
fn candidate_sources(peer: std::net::Ipv4Addr) -> Vec<std::net::Ipv4Addr> {
    const TUNNELS: [&str; 6] = ["utun", "tun", "tap", "ppp", "wg", "ipsec"];
    let mut found: Vec<(std::net::Ipv4Addr, u32)> = Vec::new();
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for iface in ifs {
            if iface.is_loopback() || TUNNELS.iter().any(|p| iface.name.starts_with(p)) {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                let mask = u32::from(v4.netmask);
                if mask != 0
                    && u32::from(peer) & mask == u32::from(v4.ip) & mask
                    && !found.iter().any(|(ip, _)| *ip == v4.ip)
                {
                    found.push((v4.ip, mask.count_ones()));
                }
            }
        }
    }
    found.sort_by(|a, b| b.1.cmp(&a.1));
    found.into_iter().map(|(ip, _)| ip).collect()
}

fn connect_from(
    src: Option<std::net::Ipv4Addr>,
    addr: SocketAddr,
    timeout: Duration,
) -> io::Result<TcpStream> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    if let Some(src) = src {
        let bind_to: SocketAddr = SocketAddr::new(IpAddr::V4(src), 0);
        sock.bind(&bind_to.into())?;
    }
    sock.connect_timeout(&addr.into(), timeout)?;
    sock.set_nonblocking(false)?;
    Ok(sock.into())
}

fn all_v4_link_local() -> Vec<std::net::Ipv4Addr> {
    let mut out = Vec::new();
    if let Ok(ifs) = if_addrs::get_if_addrs() {
        for iface in ifs {
            if let if_addrs::IfAddr::V4(v4) = &iface.addr {
                if v4.ip.is_link_local() && !out.contains(&v4.ip) {
                    out.push(v4.ip);
                }
            }
        }
    }
    out
}

/// `Command` that never flashes a console window on Windows (GUI apps
/// otherwise pop a visible console for every spawned process).
pub fn quiet_command(program: &str) -> std::process::Command {
    #[allow(unused_mut)]
    let mut c = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, UNITS[u])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}
