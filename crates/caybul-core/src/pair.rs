//! Pairing: before any transfer, the two devices exchange nonces, both
//! display the same 6-digit code, and the receiving user confirms. Only then
//! are send tokens issued — the receiver rejects any transfer without one.
//!
//! Pairing is symmetric: one accept issues tokens in both directions, so
//! either machine can send afterwards. Tokens live for the app session.

use crate::protocol::*;
use crate::util::{connect_cable_aware, new_token};
use std::io::BufReader;
use std::net::SocketAddr;
use std::time::Duration;

/// How long the requester waits for the other user to press Accept.
pub const PAIR_ACCEPT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct PairResult {
    pub peer_name: String,
    /// Token we present when sending to the peer.
    pub send_token: String,
    /// Token the peer will present when sending to us — register it with our
    /// own receiver.
    pub their_send_token: String,
}

/// Derive the shared 6-digit confirmation code from both nonces.
pub fn pair_code(nonce_a: &str, nonce_b: &str) -> String {
    let h = blake3::hash(format!("caybul-pair:{nonce_a}:{nonce_b}").as_bytes());
    let n = u32::from_le_bytes(h.as_bytes()[..4].try_into().unwrap()) % 1_000_000;
    format!("{n:06}")
}

/// Tell the peer we're disconnecting so it drops the pairing too.
/// Best-effort: if the peer is unreachable, the local disconnect still holds.
pub fn send_unpair(addr: SocketAddr, send_token: &str) {
    let Ok(stream) = connect_cable_aware(addr, Duration::from_secs(5)) else {
        return;
    };
    let mut writer = stream;
    let _ = write_msg(
        &mut writer,
        &ControlMsg::Unpair {
            pair_token: send_token.to_string(),
        },
    );
}

/// Lets the UI abort an in-flight pairing attempt: shutting the socket down
/// makes the blocked `request_pair` call return immediately.
pub struct CancelHandle {
    stream: std::net::TcpStream,
}

impl CancelHandle {
    pub fn cancel(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

/// Initiate pairing with the device at `addr`. `on_connect` fires once the
/// connection exists (keep the handle to offer a Cancel button); `on_code`
/// is called with the confirmation code as soon as it is known (display
/// it!); the call then blocks until the other user accepts, declines, or
/// the attempt is cancelled.
pub fn request_pair(
    addr: SocketAddr,
    my_name: &str,
    my_listen_port: u16,
    on_connect: impl FnOnce(CancelHandle),
    on_code: impl FnOnce(&str),
) -> anyhow::Result<PairResult> {
    let stream = connect_cable_aware(addr, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    on_connect(CancelHandle {
        stream: stream.try_clone()?,
    });
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let nonce = new_token();
    write_msg(
        &mut writer,
        &ControlMsg::PairRequest {
            version: PROTOCOL_VERSION,
            name: my_name.to_string(),
            nonce_hex: nonce.clone(),
            listen_port: my_listen_port,
        },
    )?;
    let (peer_name, their_nonce) = match read_msg(&mut reader)? {
        ControlMsg::PairChallenge { name, nonce_hex } => (name, nonce_hex),
        ControlMsg::PairReject { reason } => anyhow::bail!("pairing rejected: {reason}"),
        other => anyhow::bail!("unexpected pairing reply: {other:?}"),
    };
    on_code(&pair_code(&nonce, &their_nonce));

    stream.set_read_timeout(Some(PAIR_ACCEPT_TIMEOUT))?;
    match read_msg(&mut reader) {
        Ok(ControlMsg::PairAccept {
            token_for_requester,
            token_for_accepter,
        }) => Ok(PairResult {
            peer_name,
            send_token: token_for_requester,
            their_send_token: token_for_accepter,
        }),
        Ok(ControlMsg::PairReject { reason }) => anyhow::bail!("pairing declined: {reason}"),
        Ok(other) => anyhow::bail!("unexpected pairing reply: {other:?}"),
        Err(_) => anyhow::bail!("no answer from the other device (pairing timed out)"),
    }
}
