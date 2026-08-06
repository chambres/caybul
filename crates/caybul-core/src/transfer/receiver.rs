//! Receiver: listens for a sender, accepts a manifest, receives verified
//! chunks over multiple parallel TCP streams, and finalizes files.
//!
//! Resume: while a file is incomplete it lives as `<name>.caybulpart` next to
//! a `<name>.caybulmeta` JSON recording which chunks are already present. If
//! a transfer is interrupted, re-sending skips those chunks.

use crate::discovery;
use crate::pair::pair_code;
use crate::protocol::*;
use crate::util::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::BufReader;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub enum Event {
    Listening {
        port: u16,
        name: String,
    },
    /// A pair request was accepted. `peer_addr` is where the peer's own
    /// listener lives (None if it has none, e.g. a CLI sender);
    /// `send_token` is what WE would present when sending to them.
    Paired {
        peer_name: String,
        peer_addr: Option<SocketAddr>,
        send_token: String,
    },
    /// The paired device disconnected from its side.
    Unpaired,
    SessionStarted {
        sender: String,
        files: usize,
        total_bytes: u64,
        resumed_bytes: u64,
    },
    FileCompleted {
        rel_path: String,
        bytes: u64,
    },
    /// Emitted as verified chunks land (roughly once per 4 MiB).
    Progress {
        done_bytes: u64,
        total_bytes: u64,
    },
    SessionEnded {
        ok: bool,
        message: String,
    },
}

type EventFn = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Serialize, Deserialize)]
struct PartMeta {
    size: u64,
    chunk_size: u64,
    bitmap_hex: String,
}

struct FileProgress {
    file: Option<File>,
    bitmap: Vec<u8>,
    done_chunks: u64,
    finalized: bool,
    dirty: u32,
}

struct FileState {
    entry: FileEntry,
    final_path: PathBuf,
    part_path: PathBuf,
    meta_path: PathBuf,
    total_chunks: u64,
    prog: Mutex<FileProgress>,
}

struct Session {
    token: String,
    chunk_size: u64,
    files: Vec<Arc<FileState>>,
    total_bytes: u64,
    received_bytes: AtomicU64,
}

/// Everything a connection handler needs, shared across threads.
struct Ctx {
    active: Mutex<Option<Arc<Session>>>,
    dest: Arc<Mutex<PathBuf>>,
    name: String,
    accepted: Arc<Mutex<HashSet<String>>>,
    pair_handler: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>,
    on_event: EventFn,
}

pub struct Receiver {
    pub name: String,
    pub port: u16,
    /// Destination folder for received files. Shared so the app can change
    /// it at runtime; each incoming session reads it when it starts.
    pub dest: Arc<Mutex<PathBuf>>,
    /// Valid send tokens. Shared so the app can also register tokens issued
    /// to it by peers it paired with (the symmetric direction).
    pub accepted: Arc<Mutex<HashSet<String>>>,
    /// Called with (peer_name, code) when a pair request arrives; blocks
    /// until the user decides. Return true to accept.
    pub pair_handler: Arc<dyn Fn(&str, &str) -> bool + Send + Sync>,
}

impl Receiver {
    /// Run forever: beacon + accept pairing and transfer sessions.
    pub fn run(self, on_event: impl Fn(Event) + Send + Sync + 'static) -> anyhow::Result<()> {
        let on_event: EventFn = Arc::new(on_event);
        // Dual-stack (v4 + v6) so peers on IPv6-only links (Mac-to-Mac USB-C)
        // can connect too. Fall back to an ephemeral port if the default is
        // taken (second instance); the beacon carries the real one.
        let listener = bind_dual(self.port)
            .or_else(|_| bind_dual(0))
            .or_else(|_| TcpListener::bind(("0.0.0.0", self.port)))
            .or_else(|_| TcpListener::bind(("0.0.0.0", 0)))?;
        let port = listener.local_addr()?.port();

        // Beacon loop so senders can find us with zero config.
        {
            let name = self.name.clone();
            thread::spawn(move || loop {
                let _ = discovery::beacon_once(&name, port);
                thread::sleep(Duration::from_secs(1));
            });
        }
        on_event(Event::Listening {
            port,
            name: self.name.clone(),
        });

        let ctx = Arc::new(Ctx {
            active: Mutex::new(None),
            dest: Arc::clone(&self.dest),
            name: self.name.clone(),
            accepted: Arc::clone(&self.accepted),
            pair_handler: Arc::clone(&self.pair_handler),
            on_event,
        });

        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let ctx = Arc::clone(&ctx);
            thread::spawn(move || {
                let _ = handle_conn(stream, ctx);
            });
        }
        Ok(())
    }
}

/// IPv6 socket with v6-only disabled: accepts both v4 and v6 clients.
fn bind_dual(port: u16) -> anyhow::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    use std::net::Ipv6Addr;
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    let _ = socket.set_only_v6(false);
    let addr: SocketAddr = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port);
    socket.bind(&addr.into())?;
    socket.listen(64)?;
    Ok(socket.into())
}

fn handle_conn(stream: TcpStream, ctx: Arc<Ctx>) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let peer_ip = stream
        .peer_addr()
        .ok()
        .map(|a| crate::util::canonical_addr(a).ip());
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::with_capacity(1 << 20, stream);
    let on_event = &ctx.on_event;

    match read_msg(&mut reader)? {
        ControlMsg::PairRequest {
            version,
            name,
            nonce_hex,
            listen_port,
        } => {
            if version != PROTOCOL_VERSION {
                write_msg(
                    &mut writer,
                    &ControlMsg::PairReject {
                        reason: format!(
                            "protocol version mismatch (ours {PROTOCOL_VERSION}, theirs {version})"
                        ),
                    },
                )?;
                return Ok(());
            }
            let my_nonce = new_token();
            write_msg(
                &mut writer,
                &ControlMsg::PairChallenge {
                    name: ctx.name.clone(),
                    nonce_hex: my_nonce.clone(),
                },
            )?;
            let code = pair_code(&nonce_hex, &my_nonce);
            // A direct cable is consent: auto-accept. Everything else asks.
            let auto = peer_ip.map(crate::link::is_direct_cable_peer).unwrap_or(false);
            let accept = auto || (ctx.pair_handler)(&name, &code);
            if !accept {
                write_msg(
                    &mut writer,
                    &ControlMsg::PairReject {
                        reason: "declined on the other device".into(),
                    },
                )?;
                return Ok(());
            }
            let token_for_requester = new_token();
            let token_for_accepter = new_token();
            ctx.accepted
                .lock()
                .unwrap()
                .insert(token_for_requester.clone());
            write_msg(
                &mut writer,
                &ControlMsg::PairAccept {
                    token_for_requester,
                    token_for_accepter: token_for_accepter.clone(),
                },
            )?;
            let peer_addr = match (peer_ip, listen_port) {
                (Some(ip), p) if p > 0 => Some(SocketAddr::new(ip, p)),
                _ => None,
            };
            on_event(Event::Paired {
                peer_name: name,
                peer_addr,
                send_token: token_for_accepter,
            });
            Ok(())
        }
        ControlMsg::Unpair { pair_token } => {
            // Only a currently-paired device may unpair us (the token proves
            // it). Single-pairing model: drop everything.
            let valid = ctx.accepted.lock().unwrap().remove(&pair_token);
            if valid {
                ctx.accepted.lock().unwrap().clear();
                on_event(Event::Unpaired);
            }
            Ok(())
        }
        ControlMsg::SpeedProbe { pair_token, len } => {
            if !ctx.accepted.lock().unwrap().contains(&pair_token) {
                write_msg(
                    &mut writer,
                    &ControlMsg::Reject {
                        reason: "not paired".into(),
                    },
                )?;
                return Ok(());
            }
            anyhow::ensure!(len <= 512 * 1024 * 1024, "probe too large");
            write_msg(&mut writer, &ControlMsg::SpeedProbeOk)?;
            let mut remaining = len as usize;
            let mut buf = vec![0u8; 1 << 20];
            use std::io::Read;
            while remaining > 0 {
                let take = remaining.min(buf.len());
                reader.read_exact(&mut buf[..take])?;
                remaining -= take;
            }
            write_msg(&mut writer, &ControlMsg::SpeedProbeDone)?;
            Ok(())
        }
        ControlMsg::Hello {
            version,
            sender_name,
            pair_token,
            chunk_size,
            files,
        } => {
            if !ctx.accepted.lock().unwrap().contains(&pair_token) {
                write_msg(
                    &mut writer,
                    &ControlMsg::Reject {
                        reason: "not paired — pair with this device first".into(),
                    },
                )?;
                return Ok(());
            }
            if version != PROTOCOL_VERSION {
                write_msg(
                    &mut writer,
                    &ControlMsg::Reject {
                        reason: format!("protocol version mismatch (ours {PROTOCOL_VERSION}, theirs {version})"),
                    },
                )?;
                return Ok(());
            }
            if chunk_size == 0 || chunk_size > MAX_FRAME as u64 {
                write_msg(
                    &mut writer,
                    &ControlMsg::Reject {
                        reason: "unacceptable chunk size".into(),
                    },
                )?;
                return Ok(());
            }
            {
                let guard = ctx.active.lock().unwrap();
                if guard.is_some() {
                    drop(guard);
                    write_msg(
                        &mut writer,
                        &ControlMsg::Reject {
                            reason: "another transfer is already in progress".into(),
                        },
                    )?;
                    return Ok(());
                }
            }

            let dest_now = ctx.dest.lock().unwrap().clone();
            let session = match build_session(&dest_now, chunk_size, &files) {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    write_msg(
                        &mut writer,
                        &ControlMsg::Reject {
                            reason: format!("could not prepare destination: {e}"),
                        },
                    )?;
                    return Ok(());
                }
            };

            let mut resume = Vec::new();
            let mut resumed_bytes: u64 = 0;
            let mut total_bytes: u64 = 0;
            for (idx, f) in session.files.iter().enumerate() {
                total_bytes += f.entry.size;
                let prog = f.prog.lock().unwrap();
                resumed_bytes += prog.done_chunks.saturating_mul(chunk_size).min(f.entry.size);
                resume.push(ResumeInfo {
                    file_idx: idx as u32,
                    have_bitmap_hex: hex_encode(&prog.bitmap),
                });
            }

            *ctx.active.lock().unwrap() = Some(Arc::clone(&session));
            write_msg(
                &mut writer,
                &ControlMsg::Accept {
                    receiver_name: ctx.name.clone(),
                    token: session.token.clone(),
                    resume,
                },
            )?;
            on_event(Event::SessionStarted {
                sender: sender_name,
                files: files.len(),
                total_bytes,
                resumed_bytes,
            });

            // Control loop: wait for AllDone (or disconnect).
            let result = control_loop(&mut reader, &mut writer, &session, on_event);
            *ctx.active.lock().unwrap() = None;
            if result.is_err() {
                on_event(Event::SessionEnded {
                    ok: false,
                    message: "sender disconnected; partial files kept for resume".into(),
                });
            }
            Ok(())
        }
        ControlMsg::DataAuth { token } => {
            let session = { ctx.active.lock().unwrap().clone() };
            let Some(session) = session.filter(|s| s.token == token) else {
                write_msg(
                    &mut writer,
                    &ControlMsg::Reject {
                        reason: "bad or expired token".into(),
                    },
                )?;
                return Ok(());
            };
            write_msg(&mut writer, &ControlMsg::DataAuthOk)?;
            data_loop(&mut reader, &mut writer, &session, on_event)
        }
        _ => Ok(()),
    }
}

fn control_loop(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
    session: &Session,
    on_event: &EventFn,
) -> anyhow::Result<()> {
    loop {
        match read_msg(reader)? {
            ControlMsg::AllDone => {
                let mut all_ok = true;
                for (idx, f) in session.files.iter().enumerate() {
                    let prog = f.prog.lock().unwrap();
                    let ok = prog.finalized;
                    if !ok {
                        all_ok = false;
                    }
                    let message = if ok {
                        "complete".to_string()
                    } else {
                        format!("incomplete ({}/{} chunks)", prog.done_chunks, f.total_chunks)
                    };
                    drop(prog);
                    write_msg(
                        writer,
                        &ControlMsg::FileDone {
                            file_idx: idx as u32,
                            ok,
                            message,
                        },
                    )?;
                }
                write_msg(writer, &ControlMsg::AllDone)?;
                on_event(Event::SessionEnded {
                    ok: all_ok,
                    message: if all_ok {
                        "all files received and verified".into()
                    } else {
                        "session ended with incomplete files (kept for resume)".into()
                    },
                });
                return Ok(());
            }
            _ => continue,
        }
    }
}

fn data_loop(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
    session: &Session,
    on_event: &EventFn,
) -> anyhow::Result<()> {
    loop {
        let msg = match read_msg(reader) {
            Ok(m) => m,
            Err(_) => return Ok(()), // stream closed
        };
        let ControlMsg::ChunkHeader {
            file_idx,
            chunk_idx,
            len,
            hash_hex,
        } = msg
        else {
            return Ok(());
        };
        anyhow::ensure!((len as usize) <= MAX_FRAME, "chunk payload too large");
        let mut payload = vec![0u8; len as usize];
        reader.read_exact(&mut payload)?;

        let Some(state) = session.files.get(file_idx as usize) else {
            write_msg(
                writer,
                &ControlMsg::ChunkAck {
                    file_idx,
                    chunk_idx,
                    ok: false,
                },
            )?;
            continue;
        };

        let ok = receive_chunk(session, state, chunk_idx, &payload, &hash_hex, on_event)
            .unwrap_or(false);
        write_msg(
            writer,
            &ControlMsg::ChunkAck {
                file_idx,
                chunk_idx,
                ok,
            },
        )?;
    }
}

fn receive_chunk(
    session: &Session,
    state: &FileState,
    chunk_idx: u64,
    payload: &[u8],
    hash_hex: &str,
    on_event: &EventFn,
) -> anyhow::Result<bool> {
    let chunk_size = session.chunk_size;
    if chunk_idx >= state.total_chunks {
        return Ok(false);
    }
    let mut prog = state.prog.lock().unwrap();
    if bitmap_get(&prog.bitmap, chunk_idx) {
        // Duplicate (e.g. retry after a lost ack): we already have it.
        return Ok(true);
    }
    if blake3::hash(payload).to_hex().to_string() != hash_hex {
        return Ok(false);
    }
    let Some(file) = prog.file.as_ref() else {
        return Ok(false);
    };
    write_all_at(file, payload, chunk_idx * chunk_size)?;
    bitmap_set(&mut prog.bitmap, chunk_idx);
    prog.done_chunks += 1;
    prog.dirty += 1;

    let done_bytes = session
        .received_bytes
        .fetch_add(payload.len() as u64, Ordering::Relaxed)
        + payload.len() as u64;
    on_event(Event::Progress {
        done_bytes,
        total_bytes: session.total_bytes,
    });

    let complete = prog.done_chunks == state.total_chunks;
    if prog.dirty >= 32 || complete {
        persist_meta(state, &prog, chunk_size);
        prog.dirty = 0;
    }
    if complete && !prog.finalized {
        // Close the handle before rename so this also works on Windows.
        if let Some(f) = prog.file.take() {
            let _ = f.sync_all();
        }
        fs::rename(&state.part_path, &state.final_path)?;
        let _ = fs::remove_file(&state.meta_path);
        prog.finalized = true;
        on_event(Event::FileCompleted {
            rel_path: state.entry.rel_path.clone(),
            bytes: state.entry.size,
        });
    }
    Ok(true)
}

fn persist_meta(state: &FileState, prog: &FileProgress, chunk_size: u64) {
    let meta = PartMeta {
        size: state.entry.size,
        chunk_size,
        bitmap_hex: hex_encode(&prog.bitmap),
    };
    if let Ok(json) = serde_json::to_vec(&meta) {
        let _ = fs::write(&state.meta_path, json);
    }
}

fn build_session(dest: &Path, chunk_size: u64, files: &[FileEntry]) -> anyhow::Result<Session> {
    fs::create_dir_all(dest)?;
    let mut states = Vec::with_capacity(files.len());
    for entry in files {
        let rel = sanitize_rel_path(&entry.rel_path)?;
        let mut final_path = dest.join(&rel);
        if let Some(parent) = final_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let total_chunks = if entry.size == 0 {
            0
        } else {
            (entry.size + chunk_size - 1) / chunk_size
        };

        let mut part_path = part_name(&final_path);
        let mut meta_path = meta_name(&final_path);

        // Resume if a matching partial exists; otherwise pick a fresh name if
        // the destination already has a file with this name.
        let resume_meta = load_meta(&meta_path)
            .filter(|m| m.size == entry.size && m.chunk_size == chunk_size)
            .filter(|_| part_path.exists());

        if resume_meta.is_none() && final_path.exists() {
            final_path = uniquify(&final_path);
            part_path = part_name(&final_path);
            meta_path = meta_name(&final_path);
        }

        if entry.size == 0 {
            fs::write(&final_path, b"")?;
            states.push(Arc::new(FileState {
                entry: entry.clone(),
                final_path,
                part_path,
                meta_path,
                total_chunks,
                prog: Mutex::new(FileProgress {
                    file: None,
                    bitmap: Vec::new(),
                    done_chunks: 0,
                    finalized: true,
                    dirty: 0,
                }),
            }));
            continue;
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&part_path)?;
        file.set_len(entry.size)?;

        let (bitmap, done_chunks) = match &resume_meta {
            Some(m) => {
                let bm = hex_decode(&m.bitmap_hex).unwrap_or_default();
                let mut bm = bm;
                bm.resize(bitmap_len(total_chunks), 0);
                let done = (0..total_chunks).filter(|i| bitmap_get(&bm, *i)).count() as u64;
                (bm, done)
            }
            None => (vec![0u8; bitmap_len(total_chunks)], 0),
        };

        states.push(Arc::new(FileState {
            entry: entry.clone(),
            final_path,
            part_path,
            meta_path,
            total_chunks,
            prog: Mutex::new(FileProgress {
                file: Some(file),
                bitmap,
                done_chunks,
                finalized: false,
                dirty: 0,
            }),
        }));
    }
    let total_bytes: u64 = files.iter().map(|e| e.size).sum();
    let resumed_bytes: u64 = states
        .iter()
        .map(|s| {
            let p = s.prog.lock().unwrap();
            (p.done_chunks * chunk_size).min(s.entry.size)
        })
        .sum();
    Ok(Session {
        token: new_token(),
        chunk_size,
        files: states,
        total_bytes,
        received_bytes: AtomicU64::new(resumed_bytes),
    })
}

fn load_meta(path: &Path) -> Option<PartMeta> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn part_name(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".caybulpart");
    PathBuf::from(s)
}

fn meta_name(final_path: &Path) -> PathBuf {
    let mut s = final_path.as_os_str().to_owned();
    s.push(".caybulmeta");
    PathBuf::from(s)
}

/// "photo.jpg" -> "photo (1).jpg", "photo (2).jpg", ...
fn uniquify(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned());
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    for n in 1..10_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() && !part_name(&candidate).exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// Reject absolute paths and traversal; normalize separators.
fn sanitize_rel_path(rel: &str) -> anyhow::Result<PathBuf> {
    let rel = rel.replace('\\', "/");
    let mut out = PathBuf::new();
    for part in rel.split('/') {
        match part {
            "" | "." => continue,
            ".." => anyhow::bail!("path traversal in manifest: {rel}"),
            p if p.contains(':') => anyhow::bail!("invalid path component: {p}"),
            p => out.push(p),
        }
    }
    anyhow::ensure!(out.components().count() > 0, "empty path in manifest");
    Ok(out)
}
