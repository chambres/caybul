use anyhow::Context;
use caybul_core::discovery;
use caybul_core::link::{self, Link, LinkKind};
use caybul_core::pair;
use caybul_core::protocol::DEFAULT_PORT;
use caybul_core::transfer::{receiver, sender};
use caybul_core::util::{hostname, human_bytes};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "caybul",
    version,
    about = "Transfer anything between two devices over a cable.\n\
             Plug the machines together (Thunderbolt/USB4 or Ethernet), run\n\
             `caybul receive` on one and `caybul send <files>` on the other."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show detected cable links, their max speed, and whether USB4 is active
    Links,
    /// Wait for files from a sender
    Receive {
        /// Destination directory for received files
        #[arg(long, default_value = "caybul-inbox")]
        dir: PathBuf,
        /// Name announced to senders (defaults to this machine's hostname)
        #[arg(long)]
        name: Option<String>,
        /// TCP port to listen on
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Pair with a receiver and measure the link's real throughput
    Speedtest {
        /// Receiver address (host or host:port); omit to auto-discover
        #[arg(long)]
        to: Option<String>,
        /// Seconds to wait for auto-discovery
        #[arg(long, default_value_t = 3)]
        wait: u64,
    },
    /// Send files or directories to a receiver
    Send {
        /// Files and/or directories to send
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Receiver address (host or host:port); omit to auto-discover
        #[arg(long)]
        to: Option<String>,
        /// Parallel TCP streams (default: chosen from the detected link)
        #[arg(long)]
        streams: Option<u32>,
        /// Seconds to wait for auto-discovery
        #[arg(long, default_value_t = 3)]
        wait: u64,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Links => cmd_links(),
        Command::Receive { dir, name, port } => cmd_receive(dir, name, port),
        Command::Speedtest { to, wait } => cmd_speedtest(to, wait),
        Command::Send {
            paths,
            to,
            streams,
            wait,
        } => cmd_send(paths, to, streams, wait),
    }
}

fn cmd_speedtest(to: Option<String>, wait: u64) -> anyhow::Result<()> {
    let links = link::detect_links();
    print_link_banner(&links);
    let addr: SocketAddr = match to {
        Some(target) => parse_target(&target)?,
        None => {
            println!("Looking for receivers…");
            let peers = discovery::discover(Duration::from_secs(wait.max(1)))?;
            let p = peers
                .first()
                .ok_or_else(|| anyhow::anyhow!("no receiver found; pass --to <address>"))?;
            println!("Found \"{}\" at {}", p.name, p.addr);
            p.addr
        }
    };
    if let Some(l) = link::route_link(&links, addr.ip()) {
        println!("Route: {} ({})", l.kind.label(), l.display);
    }
    println!("Pairing…");
    let pairing = pair::request_pair(addr, &hostname(), 0, |_| {}, |code| {
        println!("Pairing code: {code} — accept on the other device…");
    })?;
    println!("Paired with \"{}\". Measuring…", pairing.peer_name);
    let mbps = caybul_core::transfer::speed::probe(addr, &pairing.send_token)?;
    if mbps >= 1000.0 {
        println!("Measured link speed: {:.2} Gbit/s", mbps / 1000.0);
    } else {
        println!("Measured link speed: {mbps:.0} Mbit/s");
    }
    Ok(())
}

fn cmd_links() -> anyhow::Result<()> {
    let links = link::detect_links();
    if links.is_empty() {
        println!("No network-capable links detected.");
        return Ok(());
    }
    println!("Detected links (best first):\n");
    for l in &links {
        println!("  {}", l.summary());
        if let Some(c) = l.cable_summary() {
            println!("             {c}");
        }
    }
    println!();
    match link::best_link() {
        Some(best) if best.kind == LinkKind::ThunderboltOrUsb4 => {
            let cable = best
                .cable_summary()
                .unwrap_or_else(|| "cable details unavailable".into());
            println!("Best path: {} — {}", best.kind.label(), cable);
            println!(
                "Recommended streams: {}",
                link::recommended_streams(best.kind)
            );
        }
        Some(best) if best.kind == LinkKind::UsbLink => {
            println!("Best path: {} ({})", best.kind.label(), best.display);
            println!(
                "A direct USB-C data link to the other machine is up — transfers\n\
                 will work over it. A Thunderbolt/USB4 cable would be faster."
            );
        }
        Some(best) => {
            println!("Best path: {} ({})", best.kind.label(), best.display);
            println!(
                "No Thunderbolt/USB4 host-to-host link detected. For the fastest\n\
                 transfer, connect the two machines with a Thunderbolt/USB4 cable\n\
                 (both OSes will bring up a network bridge automatically), or use\n\
                 a direct Ethernet cable."
            );
        }
        None => println!("No usable link found. Connect a cable and re-run."),
    }
    Ok(())
}

fn cmd_receive(dir: PathBuf, name: Option<String>, port: u16) -> anyhow::Result<()> {
    let name = name.unwrap_or_else(hostname);
    print_link_banner(&link::detect_links());
    println!("Saving into: {}", dir.display());

    let r = receiver::Receiver {
        name: name.clone(),
        port,
        dest: dir,
        accepted: Arc::new(Mutex::new(HashSet::new())),
        pair_handler: Arc::new(|peer: &str, code: &str| {
            println!("\nPair request from \"{peer}\" — code: {code}");
            println!("Check that the same code is shown on the other device.");
            print!("Accept? [y/N] ");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            matches!(line.trim(), "y" | "Y" | "yes")
        }),
    };
    r.run(move |event| match event {
        receiver::Event::Listening { port, name } => {
            println!("Ready. Announcing as \"{name}\" on port {port}.");
            println!("On the other machine, run:  caybul send <files>\n");
        }
        receiver::Event::Paired { peer_name, .. } => {
            println!("Paired with \"{peer_name}\". They can now send to this device.\n");
        }
        receiver::Event::Unpaired => {
            println!("The paired device disconnected.\n");
        }
        receiver::Event::SessionStarted {
            sender,
            files,
            total_bytes,
            resumed_bytes,
        } => {
            if resumed_bytes > 0 {
                println!(
                    "Incoming from {sender}: {files} file(s), {} (resuming, {} already here)",
                    human_bytes(total_bytes),
                    human_bytes(resumed_bytes)
                );
            } else {
                println!(
                    "Incoming from {sender}: {files} file(s), {}",
                    human_bytes(total_bytes)
                );
            }
        }
        receiver::Event::FileCompleted { rel_path, bytes } => {
            println!("  received {rel_path} ({})", human_bytes(bytes));
        }
        receiver::Event::Progress { .. } => {}
        receiver::Event::SessionEnded { ok, message } => {
            println!("{}{message}\n", if ok { "Done: " } else { "Note: " });
        }
    })
}

fn cmd_send(
    paths: Vec<PathBuf>,
    to: Option<String>,
    streams: Option<u32>,
    wait: u64,
) -> anyhow::Result<()> {
    let links = link::detect_links();
    print_link_banner(&links);
    let via = |ip: std::net::IpAddr| {
        link::route_link(&links, ip)
            .map(|l| format!(" via {}", l.kind.label()))
            .unwrap_or_default()
    };

    let addr: SocketAddr = match to {
        Some(target) => parse_target(&target)?,
        None => {
            println!("Looking for receivers (make sure `caybul receive` is running)…");
            let peers = discovery::discover(Duration::from_secs(wait.max(1)))?;
            match peers.len() {
                0 => anyhow::bail!(
                    "no receiver found. Start `caybul receive` on the other machine, \
                     or pass --to <address> (shown in its startup banner)"
                ),
                1 => {
                    let p = &peers[0];
                    println!("Found \"{}\" ({}) at {}{}", p.name, p.os, p.addr, via(p.addr.ip()));
                    p.addr
                }
                _ => {
                    eprintln!("Multiple receivers found (a device shows once per path — prefer the cable one):");
                    for p in &peers {
                        eprintln!("  {} ({}) at {}{}", p.name, p.os, p.addr, via(p.addr.ip()));
                    }
                    anyhow::bail!("pass --to <address> to choose one");
                }
            }
        }
    };

    // Tell the user which physical path this transfer will take.
    match link::route_link(&links, addr.ip()) {
        Some(l) if l.kind == LinkKind::WiFi => {
            println!("Route: Wi-Fi ({})", l.display);
            println!("⚠ This is Wi-Fi, not a cable — connect the machines directly for full speed.");
        }
        Some(l) => println!("Route: {} ({})", l.kind.label(), l.display),
        None => {}
    }

    // Pair first — the receiver only accepts transfers from paired devices.
    println!("Pairing…");
    let pairing = pair::request_pair(addr, &hostname(), 0, |_| {}, |code| {
        println!("Pairing code: {code}");
        println!("Accept on the other device (check the code matches)…");
    })?;
    println!("Paired with \"{}\".", pairing.peer_name);

    let streams = streams.unwrap_or_else(|| {
        link::route_link(&links, addr.ip())
            .map(|l| link::recommended_streams(l.kind))
            .unwrap_or(4)
    });
    println!("Sending with {streams} parallel stream(s)…");

    let bar = ProgressBar::hidden();
    bar.set_style(
        ProgressStyle::with_template(
            "{bar:40.cyan/blue} {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})",
        )
        .context("bad progress template")?,
    );
    let bar_for_cb = bar.clone();
    let report = sender::send(
        addr,
        &paths,
        &hostname(),
        &pairing.send_token,
        streams,
        move |done, total| {
            if bar_for_cb.is_hidden() && total > 0 {
                bar_for_cb.set_length(total);
                bar_for_cb.set_draw_target(indicatif::ProgressDrawTarget::stderr());
            }
            bar_for_cb.set_position(done);
        },
    )?;
    bar.finish_and_clear();

    println!("\nTransfer to \"{}\" finished:", report.receiver_name);
    for f in &report.files {
        println!(
            "  {} {} — {}",
            if f.ok { "✓" } else { "✗" },
            f.rel_path,
            f.message
        );
    }
    if report.bytes_skipped > 0 {
        println!(
            "  ({} skipped — already on the receiver from an earlier run)",
            human_bytes(report.bytes_skipped)
        );
    }
    println!(
        "  {} in {:.1?} — {:.0} Mbit/s",
        human_bytes(report.bytes_sent),
        report.elapsed,
        report.throughput_mbps()
    );
    anyhow::ensure!(report.all_ok(), "some files did not complete");
    Ok(())
}

fn print_link_banner(links: &[Link]) {
    let best = links
        .iter()
        .find(|l| l.kind != LinkKind::Loopback && l.kind != LinkKind::Vpn);
    match best {
        Some(l) => {
            println!("Link: {} ({})", l.kind.label(), l.display);
            if let Some(c) = l.cable_summary() {
                println!("      {c}");
            }
            let v4: Vec<String> = l
                .ips
                .iter()
                .filter(|ip| ip.is_ipv4())
                .map(|ip| ip.to_string())
                .collect();
            if !v4.is_empty() {
                println!("      this machine: {}", v4.join(", "));
            }
        }
        None => println!("Link: none detected yet — plug in a cable."),
    }
}

fn parse_target(target: &str) -> anyhow::Result<SocketAddr> {
    if let Ok(addr) = target.parse::<SocketAddr>() {
        return Ok(addr);
    }
    let with_port = format!("{target}:{DEFAULT_PORT}");
    with_port
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {target}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve {target}"))
}
