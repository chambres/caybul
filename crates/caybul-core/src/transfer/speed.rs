//! Post-pairing throughput probe: push a fixed burst of junk bytes to the
//! paired peer (which discards them) and time it. Gives the app a real
//! measured speed for the link right after pairing.

use crate::protocol::*;
use crate::util::connect_cable_aware;
use std::io::{BufReader, Write};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub const PROBE_BYTES: u64 = 32 * 1024 * 1024;

/// Measure throughput to a paired peer. Returns Mbit/s.
pub fn probe(addr: SocketAddr, pair_token: &str) -> anyhow::Result<f64> {
    let stream = connect_cable_aware(addr, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);

    write_msg(
        &mut writer,
        &ControlMsg::SpeedProbe {
            pair_token: pair_token.to_string(),
            len: PROBE_BYTES,
        },
    )?;
    match read_msg(&mut reader)? {
        ControlMsg::SpeedProbeOk => {}
        ControlMsg::Reject { reason } => anyhow::bail!("probe rejected: {reason}"),
        other => anyhow::bail!("unexpected probe reply: {other:?}"),
    }

    let chunk = vec![0u8; 1 << 20];
    let started = Instant::now();
    let mut sent: u64 = 0;
    while sent < PROBE_BYTES {
        let take = ((PROBE_BYTES - sent) as usize).min(chunk.len());
        writer.write_all(&chunk[..take])?;
        sent += take as u64;
    }
    writer.flush()?;
    match read_msg(&mut reader)? {
        ControlMsg::SpeedProbeDone => {}
        other => anyhow::bail!("unexpected probe reply: {other:?}"),
    }
    let secs = started.elapsed().as_secs_f64().max(1e-6);
    Ok(sent as f64 * 8.0 / secs / 1e6)
}
