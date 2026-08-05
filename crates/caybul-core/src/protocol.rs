use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

pub const PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_PORT: u16 = 45821;
pub const DISCOVERY_PORT: u16 = 45820;
/// Fixed chunk size for v1. Carried in the handshake so it can change later
/// without breaking resume metadata.
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;
/// Sanity cap on any single frame/payload we will read.
pub const MAX_FRAME: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    /// Relative path with '/' separators, e.g. "photos/2026/img.jpg".
    pub rel_path: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ResumeInfo {
    pub file_idx: u32,
    /// Bitmap (hex) of chunks the receiver already has from a previous run.
    pub have_bitmap_hex: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ControlMsg {
    /// Ask the other device to pair. `listen_port` is the requester's own
    /// listener (0 if it has none), so the accepter can send back later.
    PairRequest {
        version: u32,
        name: String,
        nonce_hex: String,
        listen_port: u16,
    },
    /// Immediate reply so both sides can derive and display the same code.
    PairChallenge {
        name: String,
        nonce_hex: String,
    },
    /// Sent after the accepter's user confirms. Two tokens: one for the
    /// requester to send with, one for the accepter to send back with.
    PairAccept {
        token_for_requester: String,
        token_for_accepter: String,
    },
    PairReject {
        reason: String,
    },
    /// Sent when a user disconnects: proves identity with the pair token so
    /// the other device drops the pairing too.
    Unpair {
        pair_token: String,
    },
    /// Post-pairing throughput probe: after Ok, `len` junk bytes follow on
    /// the same connection; the receiver discards them and replies Done.
    SpeedProbe {
        pair_token: String,
        len: u64,
    },
    SpeedProbeOk,
    SpeedProbeDone,
    Hello {
        version: u32,
        sender_name: String,
        pair_token: String,
        chunk_size: u64,
        files: Vec<FileEntry>,
    },
    Accept {
        receiver_name: String,
        token: String,
        resume: Vec<ResumeInfo>,
    },
    Reject {
        reason: String,
    },
    DataAuth {
        token: String,
    },
    DataAuthOk,
    /// Followed on the wire by exactly `len` raw payload bytes.
    ChunkHeader {
        file_idx: u32,
        chunk_idx: u64,
        len: u32,
        hash_hex: String,
    },
    ChunkAck {
        file_idx: u32,
        chunk_idx: u64,
        ok: bool,
    },
    FileDone {
        file_idx: u32,
        ok: bool,
        message: String,
    },
    AllDone,
}

pub fn write_msg<W: Write>(w: &mut W, msg: &ControlMsg) -> anyhow::Result<()> {
    let buf = serde_json::to_vec(msg)?;
    w.write_all(&(buf.len() as u32).to_be_bytes())?;
    w.write_all(&buf)?;
    w.flush()?;
    Ok(())
}

pub fn read_msg<R: Read>(r: &mut R) -> anyhow::Result<ControlMsg> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "control frame too large ({len} bytes)");
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}
