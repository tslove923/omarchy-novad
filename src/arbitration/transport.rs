//! UDP transport for the arbitration protocol -- unicast to every
//! known peer (mDNS-discovered or statically configured), not
//! broadcast. See the design doc's "Transport and port" section for
//! why unicast is the better choice once a peer table exists at all.

use std::collections::HashSet;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use super::protocol::{beats, WakeAnnounce, PROTO_VERSION};

const MAX_PACKET_SIZE: usize = 1024;
/// Same announcement sent this many times in quick succession, to each
/// peer, so a single dropped UDP packet doesn't cost a peer the whole
/// round -- see the design doc's "Reliability over unacknowledged UDP".
const REPEAT_COUNT: usize = 3;
const REPEAT_INTERVAL: Duration = Duration::from_millis(40);

/// Sends `announce` to every address in `peers`, `REPEAT_COUNT` times
/// each. Best-effort: a send failure to one peer (e.g. host
/// unreachable) is logged and doesn't stop the others.
pub fn send_announce(socket: &UdpSocket, announce: &WakeAnnounce, peers: &[SocketAddr]) {
    let payload = match serde_json::to_vec(announce) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("[arbitration] failed to encode announcement: {e}");
            return;
        }
    };
    for _ in 0..REPEAT_COUNT {
        for peer in peers {
            if let Err(e) = socket.send_to(&payload, peer) {
                tracing::debug!("[arbitration] send to {peer} failed: {e}");
            }
        }
        std::thread::sleep(REPEAT_INTERVAL);
    }
}

/// Listens for competing announcements until `window` elapses (or,
/// when `expected_peers` is non-empty, until every one of them has
/// been heard from at least once -- the early short-circuit from the
/// design doc's "Timing" section). Returns the best (per `beats`)
/// announcement seen, which is `mine` itself if nothing beat it.
///
/// De-duplicates by `instance_id` so the 3x repeat above doesn't get
/// evaluated multiple times, and silently ignores anything with a
/// mismatched `proto_version` or `wakeword` -- see `beats`'s doc
/// comment on why those never affect the outcome.
pub fn listen_for_window(
    socket: &UdpSocket,
    window: Duration,
    mine: WakeAnnounce,
    expected_peers: &[SocketAddr],
) -> WakeAnnounce {
    let deadline = Instant::now() + window;
    let mut best = mine;
    let mut heard_from: HashSet<uuid::Uuid> = HashSet::new();
    let expected_count = expected_peers.len();

    let mut buf = [0u8; MAX_PACKET_SIZE];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        if let Err(e) = socket.set_read_timeout(Some(remaining)) {
            tracing::debug!("[arbitration] set_read_timeout failed: {e}");
            break;
        }
        match socket.recv_from(&mut buf) {
            Ok((n, _from)) => {
                let Ok(announce) = serde_json::from_slice::<WakeAnnounce>(&buf[..n]) else {
                    continue; // malformed/foreign packet on this port -- ignore, don't error
                };
                if announce.proto_version != PROTO_VERSION {
                    tracing::debug!(
                        "[arbitration] ignoring peer {} on mismatched proto_version {}",
                        announce.hostname,
                        announce.proto_version
                    );
                    continue;
                }
                if announce.wakeword != best.wakeword {
                    continue; // different wake word -- never arbitrated against
                }
                if !heard_from.insert(announce.instance_id) {
                    continue; // duplicate of an already-seen repeat
                }
                if beats(&announce, &best) {
                    best = announce;
                }
                if expected_count > 0 && heard_from.len() >= expected_count {
                    break; // heard from every configured peer -- no need to keep waiting
                }
            }
            Err(_) => break, // timeout
        }
    }
    best
}
