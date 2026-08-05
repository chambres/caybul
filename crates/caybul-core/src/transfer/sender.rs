//! Sender: builds a manifest from files/directories, handshakes with the
//! receiver, then pushes verified chunks over N parallel TCP streams.

use crate::protocol::*;
use crate::util::*;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_CHUNK_ATTEMPTS: u32 = 3;

#[derive(Debug)]
pub struct FileResult {
    pub rel_path: String,
    pub ok: bool,
    pub message: String,
}

#[derive(Debug)]
pub struct SendReport {
    pub receiver_name: String,
    pub files: Vec<FileResult>,
    pub bytes_sent: u64,
    pub bytes_skipped: u64,
    pub elapsed: Duration,
}

impl SendReport {
    pub fn all_ok(&self) -> bool {
        self.files.iter().all(|f| f.ok)
    }
    pub fn throughput_mbps(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64().max(0.001);
        (self.bytes_sent as f64 * 8.0) / secs / 1e6
    }
}

struct Job {
    file_idx: u32,
    chunk_idx: u64,
    attempts: u32,
}

struct Shared {
    addr: SocketAddr,
    token: String,
    chunk_size: u64,
    sources: Vec<PathBuf>,
    sizes: Vec<u64>,
    queue: Mutex<Vec<Job>>,
    sent_bytes: AtomicU64,
    failures: Mutex<Vec<String>>,
}

/// Send `paths` (files and/or directories) to a receiver. `pair_token` must
/// come from a completed pairing with that receiver.
/// `on_progress(done_bytes, total_bytes)` is called as chunks are acked.
pub fn send(
    addr: SocketAddr,
    paths: &[PathBuf],
    sender_name: &str,
    pair_token: &str,
    streams: u32,
    on_progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> anyhow::Result<SendReport> {
    let manifest = build_manifest(paths)?;
    anyhow::ensure!(!manifest.is_empty(), "nothing to send");

    let entries: Vec<FileEntry> = manifest.iter().map(|(e, _)| e.clone()).collect();
    let sources: Vec<PathBuf> = manifest.iter().map(|(_, p)| p.clone()).collect();
    let sizes: Vec<u64> = entries.iter().map(|e| e.size).collect();

    // Control connection + handshake.
    let control = connect_cable_aware(addr, Duration::from_secs(10))?;
    control.set_nodelay(true)?;
    let mut control_writer = control.try_clone()?;
    let mut control_reader = BufReader::new(control);
    write_msg(
        &mut control_writer,
        &ControlMsg::Hello {
            version: PROTOCOL_VERSION,
            sender_name: sender_name.to_string(),
            pair_token: pair_token.to_string(),
            chunk_size: CHUNK_SIZE,
            files: entries.clone(),
        },
    )?;
    let (receiver_name, token, resume) = match read_msg(&mut control_reader)? {
        ControlMsg::Accept {
            receiver_name,
            token,
            resume,
        } => (receiver_name, token, resume),
        ControlMsg::Reject { reason } => anyhow::bail!("receiver rejected transfer: {reason}"),
        other => anyhow::bail!("unexpected reply to Hello: {other:?}"),
    };

    // Build the chunk work queue, skipping chunks the receiver already has.
    let mut have: HashMap<u32, Vec<u8>> = HashMap::new();
    for r in resume {
        if let Ok(bm) = hex_decode(&r.have_bitmap_hex) {
            have.insert(r.file_idx, bm);
        }
    }
    let mut queue: Vec<Job> = Vec::new();
    let mut total_pending: u64 = 0;
    let mut skipped: u64 = 0;
    for (idx, entry) in entries.iter().enumerate() {
        let chunks = if entry.size == 0 {
            0
        } else {
            (entry.size + CHUNK_SIZE - 1) / CHUNK_SIZE
        };
        let bm = have.get(&(idx as u32));
        for c in 0..chunks {
            let len = (entry.size - c * CHUNK_SIZE).min(CHUNK_SIZE);
            if bm.map(|b| bitmap_get(b, c)).unwrap_or(false) {
                skipped += len;
            } else {
                total_pending += len;
                queue.push(Job {
                    file_idx: idx as u32,
                    chunk_idx: c,
                    attempts: 0,
                });
            }
        }
    }
    // Largest chunks last so the tail of the transfer drains evenly; order is
    // otherwise irrelevant.
    queue.reverse();

    let shared = Arc::new(Shared {
        addr,
        token,
        chunk_size: CHUNK_SIZE,
        sources,
        sizes,
        queue: Mutex::new(queue),
        sent_bytes: AtomicU64::new(0),
        failures: Mutex::new(Vec::new()),
    });
    let on_progress = Arc::new(on_progress);

    let started = Instant::now();
    let n_streams = streams.max(1).min(16);
    let mut handles = Vec::new();
    for _ in 0..n_streams {
        let shared = Arc::clone(&shared);
        let on_progress = Arc::clone(&on_progress);
        handles.push(thread::spawn(move || worker(shared, total_pending, on_progress)));
    }
    for h in handles {
        let _ = h.join();
    }

    // Anything left in the queue means every stream died with work pending.
    {
        let queue = shared.queue.lock().unwrap();
        if !queue.is_empty() {
            let failures = shared.failures.lock().unwrap();
            anyhow::bail!(
                "transfer incomplete: {} chunks undelivered ({}); partial data is kept on the receiver — run the same send again to resume",
                queue.len(),
                failures.last().cloned().unwrap_or_else(|| "connection lost".into())
            );
        }
    }

    // Finalize over the control connection.
    write_msg(&mut control_writer, &ControlMsg::AllDone)?;
    let mut results: Vec<FileResult> = entries
        .iter()
        .map(|e| FileResult {
            rel_path: e.rel_path.clone(),
            ok: false,
            message: "no report from receiver".into(),
        })
        .collect();
    loop {
        match read_msg(&mut control_reader)? {
            ControlMsg::FileDone {
                file_idx,
                ok,
                message,
            } => {
                if let Some(r) = results.get_mut(file_idx as usize) {
                    r.ok = ok;
                    r.message = message;
                }
            }
            ControlMsg::AllDone => break,
            _ => continue,
        }
    }

    Ok(SendReport {
        receiver_name,
        files: results,
        bytes_sent: shared.sent_bytes.load(Ordering::Relaxed),
        bytes_skipped: skipped,
        elapsed: started.elapsed(),
    })
}

fn worker(
    shared: Arc<Shared>,
    total_bytes: u64,
    on_progress: Arc<dyn Fn(u64, u64) + Send + Sync>,
) {
    let result = worker_inner(&shared, total_bytes, &on_progress);
    if let Err(e) = result {
        shared.failures.lock().unwrap().push(e.to_string());
    }
}

fn worker_inner(
    shared: &Shared,
    total_bytes: u64,
    on_progress: &Arc<dyn Fn(u64, u64) + Send + Sync>,
) -> anyhow::Result<()> {
    // Fast exit if there is no work (pure-resume send).
    if shared.queue.lock().unwrap().is_empty() {
        return Ok(());
    }

    let stream = connect_cable_aware(shared.addr, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    write_msg(
        &mut writer,
        &ControlMsg::DataAuth {
            token: shared.token.clone(),
        },
    )?;
    match read_msg(&mut reader)? {
        ControlMsg::DataAuthOk => {}
        other => anyhow::bail!("data stream auth failed: {other:?}"),
    }

    let mut open_files: HashMap<u32, File> = HashMap::new();
    let mut buf = vec![0u8; shared.chunk_size as usize];

    loop {
        let job = match shared.queue.lock().unwrap().pop() {
            Some(j) => j,
            None => return Ok(()),
        };
        let size = shared.sizes[job.file_idx as usize];
        let off = job.chunk_idx * shared.chunk_size;
        let len = (size - off).min(shared.chunk_size) as usize;

        let file = match open_files.entry(job.file_idx) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(File::open(&shared.sources[job.file_idx as usize])?)
            }
        };
        read_exact_at(file, &mut buf[..len], off)?;
        let hash_hex = blake3::hash(&buf[..len]).to_hex().to_string();

        let send_result = (|| -> anyhow::Result<bool> {
            write_msg(
                &mut writer,
                &ControlMsg::ChunkHeader {
                    file_idx: job.file_idx,
                    chunk_idx: job.chunk_idx,
                    len: len as u32,
                    hash_hex,
                },
            )?;
            use std::io::Write;
            writer.write_all(&buf[..len])?;
            writer.flush()?;
            match read_msg(&mut reader)? {
                ControlMsg::ChunkAck { ok, .. } => Ok(ok),
                other => anyhow::bail!("unexpected reply to chunk: {other:?}"),
            }
        })();

        match send_result {
            Ok(true) => {
                let done = shared
                    .sent_bytes
                    .fetch_add(len as u64, Ordering::Relaxed)
                    + len as u64;
                on_progress(done, total_bytes);
            }
            Ok(false) => {
                // Receiver saw a corrupt chunk; retry a bounded number of times.
                requeue(shared, job, "chunk rejected by receiver")?;
            }
            Err(e) => {
                // Connection problem: put the job back and let this stream die;
                // other streams (or a re-run) will pick it up.
                let _ = requeue(shared, job, &e.to_string());
                return Err(e);
            }
        }
    }
}

fn requeue(shared: &Shared, mut job: Job, why: &str) -> anyhow::Result<()> {
    job.attempts += 1;
    anyhow::ensure!(
        job.attempts < MAX_CHUNK_ATTEMPTS,
        "chunk {}:{} failed {} times: {why}",
        job.file_idx,
        job.chunk_idx,
        job.attempts
    );
    shared.queue.lock().unwrap().push(job);
    Ok(())
}

/// Expand files and directories into (manifest entry, absolute source path).
fn build_manifest(paths: &[PathBuf]) -> anyhow::Result<Vec<(FileEntry, PathBuf)>> {
    let mut out: Vec<(FileEntry, PathBuf)> = Vec::new();
    for p in paths {
        let p = fs::canonicalize(p)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", p.display()))?;
        let meta = fs::metadata(&p)?;
        if meta.is_file() {
            let name = p
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("bad file name: {}", p.display()))?
                .to_string_lossy()
                .into_owned();
            push_unique(&mut out, name, meta.len(), p.clone())?;
        } else if meta.is_dir() {
            let base = p
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("bad directory name: {}", p.display()))?
                .to_string_lossy()
                .into_owned();
            walk_dir(&p, &base, &mut out)?;
        }
    }
    Ok(out)
}

fn walk_dir(
    dir: &Path,
    rel_prefix: &str,
    out: &mut Vec<(FileEntry, PathBuf)>,
) -> anyhow::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = format!("{rel_prefix}/{name}");
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_file() {
            push_unique(out, rel, meta.len(), path)?;
        } else if meta.is_dir() {
            walk_dir(&path, &rel, out)?;
        }
        // Symlinks and special files are skipped in v1.
    }
    Ok(())
}

fn push_unique(
    out: &mut Vec<(FileEntry, PathBuf)>,
    rel_path: String,
    size: u64,
    source: PathBuf,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !out.iter().any(|(e, _)| e.rel_path == rel_path),
        "duplicate name in selection: {rel_path}"
    );
    out.push((FileEntry { rel_path, size }, source));
    Ok(())
}
