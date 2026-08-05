//! Caybul core: cable link detection, peer discovery, and the transfer engine.
//!
//! v1 scope (deliberately plain and simple):
//! - Links that already present as an IP interface: Thunderbolt / USB4
//!   host-to-host bridges, direct Ethernet cables, and ordinary LAN links.
//! - Cable capability readout: negotiated max speed, and whether the
//!   connection is USB4.
//! - Multi-stream chunked TCP transfer with blake3 verification and resume.

pub mod discovery;
pub mod link;
pub mod pair;
pub mod protocol;
pub mod transfer;
pub mod util;
