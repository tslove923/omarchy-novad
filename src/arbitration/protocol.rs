//! Wire protocol for multi-instance wake-word arbitration -- see
//! `docs/design-notes/multi-instance-wake-arbitration.md`.

use serde::{Deserialize, Serialize};

/// Bumped only on a wire-incompatible change. An announcement carrying
/// a different version is logged as "peer seen" but never arbitrated
/// against, so a mixed-version household just fails open into "no
/// peers" behavior for that pair instead of crashing either side.
pub const PROTO_VERSION: u8 = 1;

/// mDNS service type this project advertises/browses for peer
/// discovery -- see `crate::arbitration::discovery`. The service-name
/// label ("novad-wake", excluding the leading `_` and the
/// `._udp.local.` suffix) has to stay <= 15 bytes -- `mdns-sd` enforces
/// this (RFC 6763's traditional limit); "novad-arbitration" (17 bytes)
/// was tried first and rejected at registration time, logged from
/// inside the crate's own background thread rather than returned as an
/// error from `ServiceInfo::new`/`register` -- found live.
pub const SERVICE_TYPE: &str = "_novad-wake._udp.local.";

/// One instance's account of hearing the wake word: sent as soon as a
/// detection fires, to every known peer (mDNS-discovered or
/// statically configured). Every instance runs the identical `beats`
/// comparison over the announcements it collects, so there's no
/// "you won" message -- each side derives its own outcome from the
/// same facts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WakeAnnounce {
    pub proto_version: u8,
    /// Random per-process -- a deterministic tiebreak key and a
    /// log-correlation id, never a long-lived identity anything
    /// persists against.
    pub instance_id: uuid::Uuid,
    /// Human-readable only -- used for the "handed off to X" losing-
    /// instance notification and log lines, never compared in
    /// `beats`.
    pub hostname: String,
    /// Arbitration only ever happens between announcements for the
    /// same wakeword -- see `beats`.
    pub wakeword: String,
    /// The detector's own combined confidence (`Detection::score`).
    pub score: f32,
    /// Normalized 0.0-1.0 loudness (`Detection::rms`) -- the primary
    /// arbitration signal; see `beats`'s doc comment for why.
    pub rms: f32,
    /// Self-asserted, from this machine's own
    /// `config::ArbitrationConfig::priority`. Carried on the wire
    /// (not looked up from a local peer table) so both sides of a
    /// `beats` comparison are guaranteed to agree on what it is.
    pub priority: i32,
}

/// Loudness has to differ by more than this to decide the comparison
/// outright -- otherwise two detections of the same real utterance,
/// picked up by near-identical mic hardware, would end up arbitrating
/// on measurement noise rather than a real difference.
const RMS_EPSILON: f32 = 0.02;
/// Same idea for the detector's own confidence score, used as the
/// second-tier signal.
const SCORE_EPSILON: f32 = 0.01;

/// `true` if `mine` should win against `theirs`. Symmetric and total:
/// every instance evaluating the same pair of announcements reaches
/// the same answer, which is what lets arbitration be leaderless (no
/// coordinator, no election protocol).
///
/// Order of signals: RMS loudness first (the best proxy for "who the
/// speaker actually addressed" -- wake-word confidence is deliberately
/// volume-invariant by training, so two machines that both clearly
/// heard a clean "hey jarvis" can land close to 1.0 regardless of
/// distance), then confidence as a tiebreak, then self-asserted
/// `priority` (a human's explicit "this machine should win close
/// calls," deliberately placed below the two real per-utterance
/// signals rather than above them), then `instance_id` as a final,
/// arbitrary-but-deterministic tiebreak so every instance still
/// agrees even on a genuine exact tie.
pub fn beats(mine: &WakeAnnounce, theirs: &WakeAnnounce) -> bool {
    if (mine.rms - theirs.rms).abs() > RMS_EPSILON {
        mine.rms > theirs.rms
    } else if (mine.score - theirs.score).abs() > SCORE_EPSILON {
        mine.score > theirs.score
    } else if mine.priority != theirs.priority {
        mine.priority > theirs.priority
    } else {
        mine.instance_id < theirs.instance_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announce(rms: f32, score: f32, priority: i32, id: u128) -> WakeAnnounce {
        WakeAnnounce {
            proto_version: PROTO_VERSION,
            instance_id: uuid::Uuid::from_u128(id),
            hostname: "test".to_string(),
            wakeword: "hey_jarvis".to_string(),
            score,
            rms,
            priority,
        }
    }

    #[test]
    fn higher_rms_wins_outright() {
        let loud = announce(0.5, 0.5, 0, 1);
        let quiet = announce(0.1, 0.99, 0, 2);
        assert!(beats(&loud, &quiet));
        assert!(!beats(&quiet, &loud));
    }

    #[test]
    fn close_rms_falls_through_to_score() {
        let a = announce(0.50, 0.95, 0, 1);
        let b = announce(0.51, 0.80, 0, 2); // within RMS_EPSILON of a
        assert!(beats(&a, &b)); // a's higher score wins the tiebreak
    }

    #[test]
    fn close_rms_and_score_falls_through_to_priority() {
        let a = announce(0.50, 0.90, 10, 1);
        let b = announce(0.51, 0.905, 0, 2); // within both epsilons
        assert!(beats(&a, &b));
        assert!(!beats(&b, &a));
    }

    #[test]
    fn total_tie_falls_through_to_instance_id() {
        let a = announce(0.5, 0.9, 0, 1);
        let b = announce(0.5, 0.9, 0, 2);
        assert!(beats(&a, &b)); // lower instance_id wins
        assert!(!beats(&b, &a));
    }

    #[test]
    fn beats_is_symmetric_for_every_pair() {
        let pairs = [
            (announce(0.9, 0.9, 0, 1), announce(0.1, 0.9, 0, 2)),
            (announce(0.5, 0.5, 5, 1), announce(0.5, 0.5, 5, 2)),
            (announce(0.5, 0.99, 0, 1), announce(0.5, 0.1, 0, 2)),
        ];
        for (a, b) in pairs {
            // Never both-win, never both-lose.
            assert_ne!(beats(&a, &b), beats(&b, &a));
        }
    }
}
