//! Multi-instance wake-word arbitration -- lets more than one
//! omarchy-novad instance on the same network coordinate so exactly
//! one of them answers a given "hey jarvis," instead of every machine
//! within earshot independently starting its own session. See
//! `docs/design-notes/multi-instance-wake-arbitration.md` for the full
//! design this implements.
//!
//! `Arbitrator::new` returns `None` (not an error) when
//! `config::ArbitrationConfig::enabled` is `false` -- `main.rs::run_detect`
//! holds `Option<Arbitrator>` and the gate at the detection site is a
//! single `match`/`if let` that falls straight through to today's
//! dispatch with no socket ever opened when it's `None`, exactly the
//! "zero-impact when off" guarantee the design doc leads with.

mod discovery;
pub mod protocol;
mod transport;

use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::Duration;

pub use protocol::WakeAnnounce;

use crate::config::{ArbitrationConfig, DiscoveryMode};
use crate::wake::detector::Detection;

/// One machine's worth of arbitration state -- the mDNS discovery
/// handle (if `discovery = "mdns"`), the receiving socket, and this
/// instance's own stable identity/config for stamping into every
/// announcement it sends.
pub struct Arbitrator {
    instance_id: uuid::Uuid,
    hostname: String,
    priority: i32,
    window: Duration,
    socket: UdpSocket,
    discovery: Option<discovery::Discovery>,
    static_peers: Vec<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArbitrationOutcome {
    /// Either no peers exist to arbitrate against, or this instance's
    /// own announcement beat everyone else's -- proceed exactly as
    /// today.
    Proceed,
    /// A peer's announcement beat this instance's -- back off. Carries
    /// the winner's hostname for the losing-instance notification (see
    /// `popup::PopupPhase::HandedOff`).
    BackOff { winner_hostname: String },
}

impl Arbitrator {
    /// Builds the arbitrator and, if `discovery = "mdns"`, starts
    /// advertising this instance and browsing for peers. Returns
    /// `Ok(None)` when disabled -- see the module doc comment. A real
    /// error here (e.g. the socket bind failing because the port's
    /// already in use) is surfaced rather than silently disabled,
    /// since a user who explicitly turned this on deserves to know it
    /// didn't actually start.
    pub fn new(cfg: &ArbitrationConfig) -> anyhow::Result<Option<Self>> {
        if !cfg.enabled {
            return Ok(None);
        }

        let hostname = hostname();
        let socket = UdpSocket::bind(("0.0.0.0", cfg.port))
            .map_err(|e| anyhow::anyhow!("arbitration: failed to bind UDP port {}: {e}", cfg.port))?;

        let discovery = match cfg.discovery {
            DiscoveryMode::Mdns => {
                let mut d = discovery::Discovery::start(cfg.port, &hostname, &cfg.bind_interface)
                    .map_err(|e| anyhow::anyhow!("arbitration: mDNS setup failed: {e}"))?;
                // Give the very first detection after a fresh start a
                // fighting chance of already knowing about peers that
                // were already running -- see `Discovery::warm_up`'s
                // doc comment.
                d.warm_up(Duration::from_millis(1000));
                let seen = d.peers();
                tracing::info!("[arbitration] mDNS warm-up found {} peer(s): {seen:?}", seen.len());
                Some(d)
            }
            DiscoveryMode::Static => None,
        };

        let static_peers = resolve_static_peers(&cfg.peers, cfg.port);

        Ok(Some(Self {
            instance_id: uuid::Uuid::new_v4(),
            hostname,
            priority: cfg.priority,
            window: Duration::from_millis(cfg.window_ms),
            socket,
            discovery,
            static_peers,
        }))
    }

    /// Runs one arbitration round for `detection` on `wakeword`:
    /// announces it to every known peer, waits (bounded, with an
    /// early short-circuit once every known peer has reported in),
    /// and decides whether this instance should proceed.
    fn current_peers(&mut self) -> Vec<SocketAddr> {
        let mut peers = self.static_peers.clone();
        if let Some(d) = self.discovery.as_mut() {
            peers.extend(d.peers());
        }
        peers.sort();
        peers.dedup();
        peers
    }

    pub fn arbitrate(&mut self, detection: &Detection, wakeword: &str) -> ArbitrationOutcome {
        let peers = self.current_peers();
        if peers.is_empty() {
            // Nothing to arbitrate against -- see "No peers found (the
            // default case)" in the design doc. No packet sent, no
            // wait incurred beyond this check.
            return ArbitrationOutcome::Proceed;
        }

        let mine = WakeAnnounce {
            proto_version: protocol::PROTO_VERSION,
            instance_id: self.instance_id,
            hostname: self.hostname.clone(),
            wakeword: wakeword.to_string(),
            score: detection.score,
            rms: detection.rms,
            priority: self.priority,
        };

        transport::send_announce(&self.socket, &mine, &peers);
        let winner = transport::listen_for_window(&self.socket, self.window, mine, &peers);

        if winner.instance_id == self.instance_id {
            ArbitrationOutcome::Proceed
        } else {
            tracing::info!(
                "[arbitration] lost to {} (rms={:.3} score={:.3}) -- backing off",
                winner.hostname,
                winner.rms,
                winner.score
            );
            ArbitrationOutcome::BackOff {
                winner_hostname: winner.hostname,
            }
        }
    }
}

/// This machine's own hostname, for `WakeAnnounce::hostname` and mDNS
/// service naming. Falls back to "novad-<pid>" if `hostname(1)` can't
/// be read for some reason (containerized/minimal environment) --
/// purely a display label, so a degraded fallback here doesn't affect
/// arbitration correctness at all.
fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("novad-{}", std::process::id()))
}

/// Resolves `[arbitration].peers` entries ("host:port" or "ip:port",
/// with a bare host/ip defaulting to `default_port`) to socket
/// addresses, skipping (with a warning) anything that doesn't resolve
/// -- a peer that's mistyped or temporarily unreachable shouldn't
/// crash arbitration for every other peer.
fn resolve_static_peers(peers: &[String], default_port: u16) -> Vec<SocketAddr> {
    peers
        .iter()
        .filter_map(|entry| {
            let with_port = if entry.contains(':') {
                entry.clone()
            } else {
                format!("{entry}:{default_port}")
            };
            match with_port.to_socket_addrs().map(|mut a| a.next()) {
                Ok(Some(addr)) => Some(addr),
                Ok(None) => {
                    tracing::warn!(
                        "[arbitration] configured peer {entry:?} did not resolve to any address"
                    );
                    None
                }
                Err(e) => {
                    tracing::warn!("[arbitration] couldn't resolve configured peer {entry:?}: {e}");
                    None
                }
            }
        })
        .collect()
}
