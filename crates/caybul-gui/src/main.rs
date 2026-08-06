//! Caybul desktop app (egui/eframe).
//!
//! Flow: the app always listens (so it can be paired with and receive), but
//! the UI starts on a pairing screen — pick a nearby device, both machines
//! show the same 6-digit code, the other side accepts. Only after pairing
//! does the transfer screen (send + receive) appear.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use caybul_core::config;
use caybul_core::discovery::{self, Peer};
use caybul_core::link::{self, Link, LinkKind};
use caybul_core::util::open_folder;
use caybul_core::pair::{self, PairResult};
use caybul_core::protocol::DEFAULT_PORT;
use caybul_core::transfer::{receiver, sender};
use caybul_core::util::{hostname, human_bytes};
use eframe::egui;
use std::collections::HashSet;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver as ChanRx, Sender as ChanTx};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 640.0])
            .with_min_inner_size([600.0, 500.0])
            .with_title("Caybul"),
        ..Default::default()
    };
    eframe::run_native("Caybul", options, Box::new(|_cc| Ok(Box::new(App::new()))))
}

struct PickedPath {
    path: PathBuf,
    label: String,
    bytes: Option<u64>,
}

#[derive(Clone)]
struct PairedPeer {
    name: String,
    addr: SocketAddr,
    token: String,
}

struct IncomingPair {
    name: String,
    code: String,
    reply: Option<ChanTx<bool>>,
}

enum CancelSlot {
    Empty,
    Ready(pair::CancelHandle),
    Cancelled,
}

struct OutgoingPair {
    peer_label: String,
    addr: SocketAddr,
    code: Arc<Mutex<Option<String>>>,
    cancel: Arc<Mutex<CancelSlot>>,
    was_cancelled: bool,
    rx: ChanRx<Result<PairResult, String>>,
}

enum SendState {
    Idle,
    Running {
        done: Arc<AtomicU64>,
        total: Arc<AtomicU64>,
        started: Instant,
        rx: ChanRx<Result<sender::SendReport, String>>,
    },
    Finished {
        ok: bool,
        lines: Vec<String>,
    },
}

enum SpeedState {
    Unknown,
    Testing,
    Done(f64),
    Failed,
}

enum RecvMsg {
    Event(receiver::Event),
    PairPrompt {
        name: String,
        code: String,
        reply: ChanTx<bool>,
    },
    Fatal(String),
}

struct App {
    my_name: String,
    // Link banner
    links: Vec<Link>,
    link_rx: ChanRx<Vec<Link>>,
    // Always-on receiver
    accepted: Arc<Mutex<HashSet<String>>>,
    recv_rx: ChanRx<RecvMsg>,
    listen_port: u16,
    recv_log: Vec<String>,
    recv_progress: Option<(u64, u64)>,
    recv_started_at: Option<Instant>,
    recv_session_files: u32,
    last_received: Option<String>,
    dest_dir: Arc<Mutex<PathBuf>>,
    // Pairing
    paired: Option<PairedPeer>,
    incoming: Option<IncomingPair>,
    outgoing: Option<OutgoingPair>,
    pair_error: Option<String>,
    peers: Vec<Peer>,
    disco_rx: Option<ChanRx<Result<Vec<Peer>, String>>>,
    disco_error: Option<String>,
    last_disco: Instant,
    manual_addr: String,
    // Sending
    picked: Vec<PickedPath>,
    send_state: SendState,
    // Post-pairing speed test
    speed: SpeedState,
    speed_rx: Option<ChanRx<Result<f64, String>>>,
    // Auto-connect over cable: off after a deliberate disconnect.
    auto_connect: bool,
    last_auto: Option<Instant>,
}

impl App {
    fn new() -> Self {
        let my_name = hostname();
        let (link_tx, link_rx) = channel();
        thread::spawn(move || loop {
            if link_tx.send(link::detect_links()).is_err() {
                return;
            }
            thread::sleep(Duration::from_secs(3));
        });

        let dest_dir = Arc::new(Mutex::new(
            config::load()
                .dest_dir
                .unwrap_or_else(|| dirs_download().join("caybul-inbox")),
        ));
        let accepted: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

        // Start the always-on receiver: it answers pair requests and accepts
        // transfers from paired devices.
        let (msg_tx, recv_rx) = channel::<RecvMsg>();
        {
            let event_tx = Mutex::new(msg_tx.clone());
            let prompt_tx = msg_tx.clone();
            let fatal_tx = msg_tx.clone();
            let r = receiver::Receiver {
                name: my_name.clone(),
                port: DEFAULT_PORT,
                dest: Arc::clone(&dest_dir),
                accepted: Arc::clone(&accepted),
                pair_handler: Arc::new(move |peer: &str, code: &str| {
                    let (reply_tx, reply_rx) = channel();
                    if prompt_tx
                        .send(RecvMsg::PairPrompt {
                            name: peer.to_string(),
                            code: code.to_string(),
                            reply: reply_tx,
                        })
                        .is_err()
                    {
                        return false;
                    }
                    reply_rx
                        .recv_timeout(Duration::from_secs(115))
                        .unwrap_or(false)
                }),
            };
            thread::spawn(move || {
                let result = r.run(move |ev| {
                    let _ = event_tx.lock().unwrap().send(RecvMsg::Event(ev));
                });
                if let Err(e) = result {
                    let _ = fatal_tx.send(RecvMsg::Fatal(format!("{e:#}")));
                }
            });
        }

        Self {
            my_name,
            links: Vec::new(),
            link_rx,
            accepted,
            recv_rx,
            listen_port: DEFAULT_PORT,
            recv_log: Vec::new(),
            recv_progress: None,
            recv_started_at: None,
            recv_session_files: 0,
            last_received: None,
            dest_dir,
            paired: None,
            incoming: None,
            outgoing: None,
            pair_error: None,
            peers: Vec::new(),
            disco_rx: None,
            disco_error: None,
            last_disco: Instant::now() - Duration::from_secs(60),
            manual_addr: String::new(),
            picked: Vec::new(),
            send_state: SendState::Idle,
            speed: SpeedState::Unknown,
            speed_rx: None,
            auto_connect: true,
            last_auto: None,
        }
    }

    /// Cable peers connect by themselves — the plugged-in cable is the
    /// consent. Lower device name initiates so both sides don't race.
    fn maybe_auto_connect(&mut self) {
        if !self.auto_connect
            || self.paired.is_some()
            || self.outgoing.is_some()
            || self.incoming.is_some()
        {
            return;
        }
        if let Some(t) = self.last_auto {
            if t.elapsed() < Duration::from_secs(10) {
                return;
            }
        }
        let me_port = self.listen_port;
        let links = &self.links;
        let mut candidate: Option<(Peer, u8)> = None;
        for p in &self.peers {
            let ip = p.addr.ip();
            let is_self = p.addr.port() == me_port
                && (ip.is_loopback() || links.iter().any(|l| l.ips.contains(&ip)));
            if is_self {
                continue;
            }
            let via = via_info(links, ip);
            if !via.is_cable {
                continue;
            }
            if candidate.as_ref().map(|(_, r)| via.rank < *r).unwrap_or(true) {
                candidate = Some((p.clone(), via.rank));
            }
        }
        let Some((peer, _)) = candidate else { return };
        if pretty_name(&self.my_name).to_lowercase() >= pretty_name(&peer.name).to_lowercase() {
            return; // the other side initiates
        }
        self.last_auto = Some(Instant::now());
        self.start_pair(peer.addr, pretty_name(&peer.name));
    }

    /// Kick off the automatic post-pairing throughput measurement. `delay`
    /// staggers the accepter side so both probes don't overlap.
    fn start_speed_test(&mut self, delay: Duration) {
        let Some(peer) = self.paired.clone() else {
            return;
        };
        let (tx, rx) = channel();
        self.speed = SpeedState::Testing;
        self.speed_rx = Some(rx);
        thread::spawn(move || {
            thread::sleep(delay);
            let res = caybul_core::transfer::speed::probe(peer.addr, &peer.token)
                .map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
    }

    // ---------------- background plumbing ----------------

    fn drain_channels(&mut self) {
        while let Ok(links) = self.link_rx.try_recv() {
            self.links = links;
        }
        let mut msgs = Vec::new();
        while let Ok(m) = self.recv_rx.try_recv() {
            msgs.push(m);
        }
        for m in msgs {
            self.handle_recv_msg(m);
        }
        if let Some(rx) = &self.disco_rx {
            if let Ok(res) = rx.try_recv() {
                match res {
                    Ok(peers) => {
                        self.peers = peers;
                        self.disco_error = None;
                    }
                    Err(e) => self.disco_error = Some(e),
                }
                self.disco_rx = None;
                self.last_disco = Instant::now();
            }
        }
        // Outgoing pairing finished?
        let outcome = if let Some(o) = &self.outgoing {
            o.rx.try_recv().ok()
        } else {
            None
        };
        if let Some(result) = outcome {
            let addr = self.outgoing.as_ref().map(|o| o.addr);
            let was_cancelled = self
                .outgoing
                .as_ref()
                .map(|o| o.was_cancelled)
                .unwrap_or(false);
            self.outgoing = None;
            if was_cancelled {
                return;
            }
            match result {
                Ok(pr) => {
                    // Register the token the peer will use to send to us.
                    self.accepted
                        .lock()
                        .unwrap()
                        .insert(pr.their_send_token.clone());
                    self.paired = Some(PairedPeer {
                        name: pr.peer_name.clone(),
                        addr: addr.unwrap(),
                        token: pr.send_token,
                    });
                    self.pair_error = None;
                    self.push_log(format!("Paired with \"{}\"", pr.peer_name));
                    self.start_speed_test(Duration::ZERO);
                }
                Err(e) => self.pair_error = Some(e),
            }
        }
        // Speed test finished?
        let speed_result = if let Some(rx) = &self.speed_rx {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(result) = speed_result {
            self.speed_rx = None;
            self.speed = match result {
                Ok(mbps) => SpeedState::Done(mbps),
                Err(_) => SpeedState::Failed,
            };
        }
        // Send finished?
        let finished = if let SendState::Running { rx, .. } = &self.send_state {
            rx.try_recv().ok()
        } else {
            None
        };
        if let Some(result) = finished {
            self.send_state = match result {
                Ok(report) => {
                    let mut lines: Vec<String> = report
                        .files
                        .iter()
                        .map(|f| {
                            if f.ok {
                                format!("Sent {}", f.rel_path)
                            } else {
                                format!("FAILED {} — {}", f.rel_path, f.message)
                            }
                        })
                        .collect();
                    if report.bytes_skipped > 0 {
                        lines.push(format!(
                            "{} skipped (already there from an earlier run)",
                            human_bytes(report.bytes_skipped)
                        ));
                    }
                    lines.push(format!(
                        "{} in {:.1?} — {:.0} Mbit/s",
                        human_bytes(report.bytes_sent),
                        report.elapsed,
                        report.throughput_mbps()
                    ));
                    SendState::Finished {
                        ok: report.all_ok(),
                        lines,
                    }
                }
                Err(e) => SendState::Finished {
                    ok: false,
                    lines: vec![format!("✗ {e}")],
                },
            };
        }
    }

    fn handle_recv_msg(&mut self, msg: RecvMsg) {
        match msg {
            RecvMsg::PairPrompt { name, code, reply } => {
                self.incoming = Some(IncomingPair {
                    name,
                    code,
                    reply: Some(reply),
                });
            }
            RecvMsg::Event(ev) => match ev {
                receiver::Event::Listening { port, .. } => {
                    self.listen_port = port;
                }
                receiver::Event::Paired {
                    peer_name,
                    peer_addr,
                    send_token,
                } => {
                    // The other device initiated; we accepted. If they have a
                    // listener we are now paired both ways.
                    if let Some(addr) = peer_addr {
                        if self.paired.is_none() {
                            self.paired = Some(PairedPeer {
                                name: peer_name.clone(),
                                addr,
                                token: send_token,
                            });
                            // Stagger so the two sides' probes don't overlap.
                            self.start_speed_test(Duration::from_secs(3));
                        }
                    }
                    self.push_log(format!("Connected to {}", pretty_name(&peer_name)));
                }
                receiver::Event::Unpaired => {
                    if self.paired.is_some() {
                        self.auto_connect = false;
                        self.paired = None;
                        self.send_state = SendState::Idle;
                        self.speed = SpeedState::Unknown;
                        self.speed_rx = None;
                        self.push_log("The other device disconnected.".to_string());
                    }
                }
                receiver::Event::SessionStarted {
                    sender,
                    files,
                    total_bytes,
                    resumed_bytes,
                } => {
                    self.recv_progress = Some((resumed_bytes, total_bytes));
                    self.recv_started_at = Some(Instant::now());
                    self.recv_session_files = 0;
                    self.push_log(format!(
                        "Receiving {files} file{} ({}) from {}…",
                        if files == 1 { "" } else { "s" },
                        human_bytes(total_bytes),
                        pretty_name(&sender)
                    ));
                }
                receiver::Event::Progress {
                    done_bytes,
                    total_bytes,
                } => {
                    self.recv_progress = Some((done_bytes, total_bytes));
                }
                receiver::Event::FileCompleted { rel_path, bytes } => {
                    self.recv_session_files += 1;
                    self.push_log(format!("Received {rel_path} ({})", human_bytes(bytes)));
                }
                receiver::Event::SessionEnded { ok, .. } => {
                    self.push_log(if ok {
                        "All files received.".to_string()
                    } else {
                        "Transfer interrupted — it will pick up where it left off.".to_string()
                    });
                    if ok && self.recv_session_files > 0 {
                        let n = self.recv_session_files;
                        self.last_received = Some(format!(
                            "Received {n} file{}",
                            if n == 1 { "" } else { "s" }
                        ));
                    }
                    self.recv_progress = None;
                    self.recv_started_at = None;
                }
            },
            RecvMsg::Fatal(_) => {
                self.push_log("Something went wrong — please restart Caybul.".to_string());
            }
        }
    }

    fn push_log(&mut self, line: String) {
        self.recv_log.push(line);
        if self.recv_log.len() > 300 {
            self.recv_log.drain(..100);
        }
    }

    fn maybe_discover(&mut self) {
        if self.paired.is_none()
            && self.outgoing.is_none()
            && self.disco_rx.is_none()
            && self.last_disco.elapsed() > Duration::from_secs(4)
        {
            let (tx, rx) = channel();
            self.disco_rx = Some(rx);
            thread::spawn(move || {
                let res =
                    discovery::discover(Duration::from_millis(1500)).map_err(|e| e.to_string());
                let _ = tx.send(res);
            });
        }
    }

    fn start_pair(&mut self, addr: SocketAddr, label: String) {
        let code: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cancel: Arc<Mutex<CancelSlot>> = Arc::new(Mutex::new(CancelSlot::Empty));
        let (tx, rx) = channel();
        let code2 = Arc::clone(&code);
        let cancel2 = Arc::clone(&cancel);
        let my_name = self.my_name.clone();
        let listen_port = self.listen_port;
        thread::spawn(move || {
            let res = pair::request_pair(
                addr,
                &my_name,
                listen_port,
                |handle| {
                    let mut slot = cancel2.lock().unwrap();
                    // If the user hit Cancel before the connection existed,
                    // abort right away.
                    if matches!(*slot, CancelSlot::Cancelled) {
                        handle.cancel();
                    } else {
                        *slot = CancelSlot::Ready(handle);
                    }
                },
                |c| {
                    *code2.lock().unwrap() = Some(c.to_string());
                },
            )
            .map_err(|e| format!("{e:#}"));
            let _ = tx.send(res);
        });
        self.pair_error = None;
        self.outgoing = Some(OutgoingPair {
            peer_label: label,
            addr,
            code,
            cancel,
            was_cancelled: false,
            rx,
        });
    }

    fn start_send(&mut self) {
        let Some(peer) = self.paired.clone() else {
            return;
        };
        let paths: Vec<PathBuf> = self.picked.iter().map(|p| p.path.clone()).collect();
        let streams = link::best_link()
            .map(|l| link::recommended_streams(l.kind))
            .unwrap_or(4);
        let done = Arc::new(AtomicU64::new(0));
        let total = Arc::new(AtomicU64::new(0));
        let (tx, rx) = channel();
        {
            let done = Arc::clone(&done);
            let total = Arc::clone(&total);
            let my_name = self.my_name.clone();
            thread::spawn(move || {
                let d = Arc::clone(&done);
                let t = Arc::clone(&total);
                let res = sender::send(
                    peer.addr,
                    &paths,
                    &my_name,
                    &peer.token,
                    streams,
                    move |dn, tt| {
                        d.store(dn, Ordering::Relaxed);
                        t.store(tt, Ordering::Relaxed);
                    },
                )
                .map_err(|e| format!("{e:#}"));
                let _ = tx.send(res);
            });
        }
        self.send_state = SendState::Running {
            done,
            total,
            started: Instant::now(),
            rx,
        };
    }

    fn add_path(&mut self, path: PathBuf) {
        if self.picked.iter().any(|p| p.path == path) {
            return;
        }
        let meta = std::fs::metadata(&path).ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let bytes = if is_dir {
            dir_size(&path)
        } else {
            meta.map(|m| m.len())
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let label = if is_dir { format!("{name}/") } else { name };
        self.picked.push(PickedPath { path, label, bytes });
    }

    // ---------------- UI ----------------

    /// "Received files go to <path>  [Change…] [Show]" — used on both screens.
    fn dest_row(&mut self, ui: &mut egui::Ui) {
        let current = self.dest_dir.lock().unwrap().clone();
        ui.horizontal(|ui| {
            ui.weak("Received files go to");
            ui.label(tilde(&current));
            if ui.button("Change…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&current).pick_folder() {
                    *self.dest_dir.lock().unwrap() = dir.clone();
                    config::save(&config::Config {
                        dest_dir: Some(dir),
                    });
                }
            }
            if ui.button("Show").clicked() {
                let _ = std::fs::create_dir_all(&current);
                open_folder(&current);
            }
        });
    }

    fn link_banner(&self, ui: &mut egui::Ui) {
        let has_tb = self
            .links
            .iter()
            .any(|l| l.kind == LinkKind::ThunderboltOrUsb4);
        let has_cable = has_tb
            || self
                .links
                .iter()
                .any(|l| l.kind == LinkKind::UsbLink || l.kind == LinkKind::Ethernet);
        let (color, text) = if has_tb {
            (egui::Color32::from_rgb(90, 200, 250), "Fast cable connected")
        } else if has_cable {
            (egui::Color32::from_rgb(110, 210, 180), "Cable connected")
        } else {
            (egui::Color32::from_rgb(240, 200, 90), "No cable — using Wi-Fi")
        };
        status_dot(ui, color);
        ui.colored_label(color, text);
    }

    fn incoming_pair_panel(&mut self, ui: &mut egui::Ui) {
        let mut decision: Option<bool> = None;
        if let Some(inc) = &self.incoming {
            egui::Frame::group(ui.style())
                .fill(ui.style().visuals.extreme_bg_color)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(format!("{} wants to connect.", pretty_name(&inc.name)));
                    ui.horizontal(|ui| {
                        ui.heading(&inc.code);
                        ui.weak("— the other computer shows this same code");
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Accept").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("Decline").clicked() {
                            decision = Some(false);
                        }
                    });
                });
            ui.add_space(8.0);
        }
        if let Some(accept) = decision {
            if let Some(mut inc) = self.incoming.take() {
                if let Some(reply) = inc.reply.take() {
                    let _ = reply.send(accept);
                }
            }
        }
    }

    fn pairing_screen(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connect to a device");
        ui.weak(if self.auto_connect {
            "Open Caybul on your other computer. Devices joined by a cable connect on their own."
        } else {
            "Pick a device to connect."
        });
        ui.add_space(8.0);

        if let Some(out) = &mut self.outgoing {
            let code = out.code.lock().unwrap().clone();
            let mut cancelled = false;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(format!("Connecting to {}…", out.peer_label));
                match code {
                    Some(c) => {
                        ui.horizontal(|ui| {
                            ui.heading(c);
                            ui.label("— press Accept on the other computer");
                        });
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Contacting…");
                        });
                    }
                }
                if ui.button("Cancel").clicked() {
                    cancelled = true;
                }
            });
            if cancelled {
                out.was_cancelled = true;
                let mut slot = out.cancel.lock().unwrap();
                match std::mem::replace(&mut *slot, CancelSlot::Cancelled) {
                    CancelSlot::Ready(handle) => handle.cancel(),
                    _ => {}
                }
                drop(slot);
                self.outgoing = None;
                self.last_auto = Some(Instant::now());
            }
            ui.add_space(8.0);
            return;
        }

        if self.pair_error.is_some() {
            ui.colored_label(
                egui::Color32::from_rgb(230, 130, 100),
                "Couldn't connect. Make sure Caybul is open on the other computer, then try again.",
            );
            ui.add_space(6.0);
        }

        let mut to_pair: Option<(SocketAddr, String)> = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(120.0);
            let me_port = self.listen_port;
            let links = &self.links;
            // Hide our own beacons, then keep ONE row per device — the same
            // device is heard once per network path; show only its best.
            let mut best: Vec<(String, Peer, ViaInfo)> = Vec::new();
            for p in self.peers.iter() {
                let ip = p.addr.ip();
                let is_self = p.addr.port() == me_port
                    && (ip.is_loopback() || links.iter().any(|l| l.ips.contains(&ip)));
                if is_self {
                    continue;
                }
                let via = via_info(links, ip);
                let name = pretty_name(&p.name);
                match best.iter_mut().find(|(n, _, _)| *n == name) {
                    Some(entry) => {
                        if via.rank < entry.2.rank {
                            entry.1 = p.clone();
                            entry.2 = via;
                        }
                    }
                    None => best.push((name, p.clone(), via)),
                }
            }
            best.sort_by(|a, b| a.2.rank.cmp(&b.2.rank).then(a.0.cmp(&b.0)));

            if best.is_empty() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Looking for devices…");
                });
            }
            for (name, p, via) in best {
                ui.horizontal(|ui| {
                    status_dot(ui, via.color);
                    ui.label(&name);
                    ui.colored_label(via.color, via.label);
                    if ui.button("Connect").clicked() {
                        self.auto_connect = true;
                        to_pair = Some((p.addr, name.clone()));
                    }
                });
            }
        });

        ui.add_space(6.0);
        egui::CollapsingHeader::new("Advanced")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Connect by address:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.manual_addr)
                            .hint_text("e.g. 169.254.90.148")
                            .desired_width(180.0),
                    );
                    if ui.button("Connect").clicked() {
                        match resolve_addr(&self.manual_addr) {
                            Ok(addr) => to_pair = Some((addr, self.manual_addr.clone())),
                            Err(e) => self.pair_error = Some(e.to_string()),
                        }
                    }
                });
                ui.weak(format!("This computer: “{}”", self.my_name));
            });

        ui.add_space(6.0);
        self.dest_row(ui);

        if let Some((addr, label)) = to_pair {
            self.start_pair(addr, label);
        }
    }

    fn transfer_screen(&mut self, ui: &mut egui::Ui) {
        let peer = self.paired.clone().unwrap();
        let via = via_info(&self.links, peer.addr.ip());
        ui.horizontal(|ui| {
            status_dot(ui, egui::Color32::from_rgb(120, 220, 120));
            ui.colored_label(
                egui::Color32::from_rgb(120, 220, 120),
                format!("Connected to {}", pretty_name(&peer.name)),
            );
            ui.colored_label(via.color, format!("over {}", via.label));
            match &self.speed {
                SpeedState::Testing => {
                    ui.spinner();
                    ui.weak("checking speed…");
                }
                SpeedState::Done(mbps) => {
                    let text = if *mbps >= 1000.0 {
                        format!("· {:.1} Gbit/s", mbps / 1000.0)
                    } else {
                        format!("· {mbps:.0} Mbit/s")
                    };
                    ui.strong(text);
                }
                _ => {}
            }
            if ui.button("Disconnect").clicked() {
                // Tell the other device so it disconnects too (best-effort).
                let addr = peer.addr;
                let token = peer.token.clone();
                thread::spawn(move || pair::send_unpair(addr, &token));
                self.accepted.lock().unwrap().clear();
                self.auto_connect = false;
                self.paired = None;
                self.send_state = SendState::Idle;
                self.speed = SpeedState::Unknown;
                self.speed_rx = None;
                return;
            }
        });
        if self.paired.is_none() {
            return;
        }
        if !via.is_cable && !matches!(via.kind, Some(LinkKind::Loopback)) {
            ui.colored_label(
                egui::Color32::from_rgb(240, 200, 90),
                "You're connected over Wi-Fi. For faster transfers, plug a cable between the \
                 computers, disconnect, and connect again.",
            );
        }
        ui.add_space(10.0);

        // ---- Send ----
        ui.strong("Send");
        ui.horizontal(|ui| {
            if ui.button("Add files…").clicked() {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    for f in files {
                        self.add_path(f);
                    }
                }
            }
            if ui.button("Add folder…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.add_path(dir);
                }
            }
            if !self.picked.is_empty() && ui.button("Clear").clicked() {
                self.picked.clear();
            }
        });
        let mut remove: Option<usize> = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(90.0);
            if self.picked.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.weak("Drop files or folders here");
                });
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("picked")
                    .max_height(130.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for (i, p) in self.picked.iter().enumerate() {
                            ui.horizontal(|ui| {
                                if ui.small_button("x").clicked() {
                                    remove = Some(i);
                                }
                                ui.label(&p.label);
                                if let Some(b) = p.bytes {
                                    ui.weak(human_bytes(b));
                                }
                            });
                        }
                    });
            }
        });
        if let Some(i) = remove {
            self.picked.remove(i);
        }

        match &self.send_state {
            SendState::Running {
                done,
                total,
                started,
                ..
            } => {
                let d = done.load(Ordering::Relaxed);
                let t = total.load(Ordering::Relaxed);
                let frac = if t > 0 { d as f32 / t as f32 } else { 0.0 };
                let rate = d as f64 * 8.0 / started.elapsed().as_secs_f64().max(0.001) / 1e6;
                ui.add(
                    egui::ProgressBar::new(frac)
                        .text(format!(
                            "{} / {} — {:.0} Mbit/s",
                            human_bytes(d),
                            human_bytes(t),
                            rate
                        ))
                        .animate(true),
                );
            }
            SendState::Finished { ok, lines } => {
                for line in lines {
                    ui.label(line);
                }
                if !*ok {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 130, 100),
                        "Something interrupted the transfer — press Send again and it will \
                         pick up where it left off.",
                    );
                }
            }
            SendState::Idle => {}
        }
        ui.add_space(4.0);
        let sending = matches!(self.send_state, SendState::Running { .. });
        let button = egui::Button::new(
            egui::RichText::new(if sending { "Sending…" } else { "Send" }).size(15.0),
        )
        .min_size(egui::vec2(110.0, 30.0));
        if ui
            .add_enabled(!self.picked.is_empty() && !sending, button)
            .clicked()
        {
            self.start_send();
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(4.0);

        // ---- Receive ----
        ui.strong("Receive");
        self.dest_row(ui);
        if let Some((done, total)) = self.recv_progress {
            let frac = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            let rate = self
                .recv_started_at
                .map(|t| done as f64 * 8.0 / t.elapsed().as_secs_f64().max(0.001) / 1e6)
                .unwrap_or(0.0);
            ui.add(
                egui::ProgressBar::new(frac)
                    .text(format!(
                        "{} / {} — {:.0} Mbit/s",
                        human_bytes(done),
                        human_bytes(total),
                        rate
                    ))
                    .animate(true),
            );
        }
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(90.0);
            egui::ScrollArea::vertical()
                .id_salt("log")
                .stick_to_bottom(true)
                .max_height(180.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if self.recv_log.is_empty() {
                        ui.weak("Activity will appear here.");
                    }
                    for line in &self.recv_log {
                        ui.label(line);
                    }
                });
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_channels();
        self.maybe_discover();
        self.maybe_auto_connect();

        // Drag & drop from Finder/Explorer (only useful once paired).
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        for p in dropped {
            self.add_path(p);
        }

        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(10.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Caybul");
                    ui.separator();
                    self.link_banner(ui);
                });
            });

        // Incoming-transfer banner, visible no matter which screen is up —
        // a receive must never happen invisibly.
        let show_banner =
            self.paired.is_none() && (self.recv_progress.is_some() || self.last_received.is_some());
        if show_banner {
            egui::TopBottomPanel::bottom("activity")
                .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(10.0))
                .show(ctx, |ui| {
                    if let Some((done, total)) = self.recv_progress {
                        let frac = if total > 0 {
                            done as f32 / total as f32
                        } else {
                            0.0
                        };
                        ui.label("Receiving files…");
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .text(format!("{} / {}", human_bytes(done), human_bytes(total)))
                                .animate(true),
                        );
                    } else if let Some(msg) = self.last_received.clone() {
                        ui.horizontal(|ui| {
                            status_dot(ui, egui::Color32::from_rgb(120, 220, 120));
                            ui.label(msg);
                            if ui.button("Show").clicked() {
                                let dest = self.dest_dir.lock().unwrap().clone();
                                open_folder(&dest);
                            }
                            if ui.small_button("x").clicked() {
                                self.last_received = None;
                            }
                        });
                    }
                });
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(14.0))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.incoming_pair_panel(ui);
                        if self.paired.is_none() {
                            self.pairing_screen(ui);
                        } else {
                            self.transfer_screen(ui);
                        }
                    });
            });

        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

/// How we'd physically reach a given peer IP, in words a non-technical user
/// understands: "cable", "Wi-Fi", or "network". Thunderbolt, the Mac-to-Mac
/// USB link, and a direct Ethernet cable are all just "cable" — macOS names
/// the same wire differently on each machine, and users don't care.
struct ViaInfo {
    label: &'static str,
    color: egui::Color32,
    kind: Option<LinkKind>,
    rank: u8,
    is_cable: bool,
}

fn via_info(links: &[Link], ip: std::net::IpAddr) -> ViaInfo {
    match link::route_link(links, ip) {
        Some(l) => {
            let (label, color, rank, is_cable) = match l.kind {
                LinkKind::ThunderboltOrUsb4 => (
                    "fast cable",
                    egui::Color32::from_rgb(90, 200, 250),
                    0,
                    true,
                ),
                LinkKind::UsbLink | LinkKind::Ethernet => {
                    ("cable", egui::Color32::from_rgb(110, 210, 180), 1, true)
                }
                LinkKind::Loopback => ("this computer", egui::Color32::GRAY, 3, false),
                LinkKind::Other => ("network", egui::Color32::GRAY, 4, false),
                LinkKind::WiFi => ("Wi-Fi", egui::Color32::from_rgb(240, 200, 90), 5, false),
                LinkKind::Vpn => ("network", egui::Color32::GRAY, 6, false),
            };
            ViaInfo {
                label,
                color,
                kind: Some(l.kind),
                rank,
                is_cable,
            }
        }
        None => ViaInfo {
            label: "network",
            color: egui::Color32::GRAY,
            kind: None,
            rank: 9,
            is_cable: false,
        },
    }
}

/// Small colored circle drawn by the app itself — immune to font problems.
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

/// "rahuls-MacBook-Air.local" -> "rahuls-MacBook-Air"
fn pretty_name(name: &str) -> String {
    name.split('.').next().unwrap_or(name).to_string()
}

fn resolve_addr(text: &str) -> anyhow::Result<SocketAddr> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "enter an address first");
    if let Ok(a) = text.parse::<SocketAddr>() {
        return Ok(a);
    }
    format!("{text}:{DEFAULT_PORT}")
        .to_socket_addrs()
        .map_err(|_| anyhow::anyhow!("cannot resolve \"{text}\""))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("cannot resolve \"{text}\""))
}

fn dir_size(path: &PathBuf) -> Option<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![path.clone()];
    let mut visited = 0;
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).ok()?;
        for e in entries.flatten() {
            visited += 1;
            if visited > 50_000 {
                return None; // too big to size synchronously
            }
            let Ok(meta) = e.metadata() else { continue };
            if meta.is_file() {
                total += meta.len();
            } else if meta.is_dir() {
                stack.push(e.path());
            }
        }
    }
    Some(total)
}

/// "/Users/rahul/Downloads/x" -> "~/Downloads/x"
fn tilde(p: &std::path::Path) -> String {
    #[allow(deprecated)]
    if let Some(home) = std::env::home_dir() {
        if let Ok(rest) = p.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    p.display().to_string()
}

fn dirs_download() -> PathBuf {
    #[allow(deprecated)]
    std::env::home_dir()
        .map(|h| h.join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}
