# Multi-instance wake-word arbitration

Status: **implemented** (`src/arbitration/`), verified live between two
real machines on the same LAN (`omarchy-novad arbitration test`
produced a correct PROCEED/BACK OFF split by RMS loudness, mDNS
discovery confirmed working both directions). Covers `src/wake/`,
`src/pipeline.rs`, and `main.rs`'s `run_detect`/`Command::Detect` --
everything between a wake-word detection firing and the standalone
pipeline (record → transcribe → classify → route → popup, see
`pipeline.rs::run_session`) starting.

**Implementation notes** (where the shipped code differs from this
document, or what it found along the way):
- The `mdns-sd` crate needs `.enable_addr_auto()` explicitly -- the
  `()`/empty-string address argument to `ServiceInfo::new` documented
  in the crate itself means "no addresses," not "auto-detect," which
  silently produced an unannounceable service with no addresses at
  all and no error. Also needed a service host label distinct from the
  system's own `<hostname>.local.` (already owned by `avahi-daemon`)
  to avoid a same-name probe conflict that also failed silently -- see
  `discovery.rs`'s comments for both.
- **Discovered-peer catalog (config.toml auto-append) was not built.**
  [Open questions](#open-questions) below already flagged the TOML-
  append mechanism as unresolved; given that, Phase 1-3 (RMS,
  discovery, core arbitration + priority + the `HandedOff` popup) shipped
  without it. Peers are visible via `RUST_LOG=info` logging and
  `omarchy-novad arbitration test` today, not as a persisted,
  user-editable list. Worth a follow-up if that visibility matters in
  practice.
- **`Config::ArbitrationConfig::bind_interface` matters more than the
  original draft implied** -- found live on a multi-homed desktop
  (several libvirt bridge interfaces alongside its real Wi-Fi NIC):
  `mdns-sd`'s automatic interface selection isn't reliable enough to
  skip configuring this on a machine with more than one active
  interface. Wired via `ServiceDaemon::disable_interface`/
  `enable_interface`, not sketched in the original protocol section.
- Added `omarchy-novad arbitration test` (not in the original phasing)
  -- runs one real arbitration round with a synthetic detection against
  whatever peers are actually reachable, which is what made live
  cross-machine verification possible without needing to say the wake
  word on two machines in the same instant. Recommended first stop for
  "is this actually working" on a new pair of machines.

**Revision note:** the first draft recommended plain UDP broadcast as
both the discovery and arbitration transport, and left cross-subnet
reach, tie-break priority, and losing-instance feedback as open
questions. This revision replaces broadcast discovery with mDNS/Avahi
(see [Discovery](#discovery-recommendation-and-alternatives)), adds a
self-asserted per-machine priority plus an auto-populated peer catalog
(see [Config surface](#config-surface)), and adds a subtle
handed-off-to notification for losing instances (see
[Losing-instance feedback](#losing-instance-feedback)), per feedback on
the first draft. Arbitration itself (the actual `WakeAnnounce` exchange
once peers are known) is now unicast rather than broadcast either way --
see [Transport and port](#transport-and-port) for why that was already
the better choice independent of the discovery mechanism swap.

## Problem statement

The user runs `omarchy-novad` on more than one machine on the same LAN
at once (today: a laptop and a desktop; more machines are plausible
later). Each instance independently runs its own mic capture + NPU
wake-word detector (`wake::detector::Detector`, fed from `run_detect`'s
`cpal` stream). When "hey jarvis" is said once, every machine within
earshot hears it and fires its own `Detection` at roughly the same
moment, and today each one independently starts its own full pipeline
session: its own `voxtype record start`, its own popup, and — for an
OpenClaw handoff — its own TTS reply. Concretely, this means one
utterance can produce: two overlapping microphone recordings both
trying to capture the same speech (each competing with the other's
own playback and the user's voice), two popups on two different
screens, and potentially two spoken replies talking over each other.
None of that is a bug in any one instance -- each is behaving
correctly in isolation, exactly as a single-machine setup should. The
bug is that there's no mechanism above the per-instance level that
knows there's more than one listener in the room at all.

This document proposes a way for instances to discover each other on
the LAN and, on each wake-word detection, run a short, bounded
arbitration so exactly one instance proceeds and the rest silently
back off.

## Design goals

1. **Zero-impact when off or alone.** This is the single most
   important constraint. Single-machine is and will stay the common
   case; the feature defaults off, and even when on, a machine with no
   peers actually present must behave *exactly* like today -- same
   latency, same code path, no new failure modes. See
   [No peers found](#no-peers-found-the-default-case).
2. **Bounded added latency when it does run.** Arbitrating among real
   peers necessarily adds *some* wait (you can't know you're the
   loudest until you've had a chance to hear from everyone else), but
   it has to stay well under what a user would perceive as "this got
   slower" -- hundreds of milliseconds, not seconds.
3. **No shared clock required.** Wall-clock timestamps and multi-
   machine time sync (NTP drift, suspend/resume clock jumps, no PTP on
   a home LAN) are a fragile foundation for "who was first" or "is
   this the same round" reasoning. The design should need none of it.
4. **Decentralized, leaderless.** No coordinator process, no election
   protocol beyond "everyone independently computes the same max and
   agrees" -- simpler to implement, and there's no single point that
   losing a machine breaks.
5. **Fail toward "acts like before," not toward silence.** Packet loss
   or a dead peer should never mean *both* machines act (the problem
   this is solving) but should also, ideally, not mean the utterance
   is silently dropped with no recourse at all -- see
   [Winner failure](#winner-failure) for how far this document takes
   that guarantee (not very far, deliberately -- see the reasoning
   there).

## Non-goals (this pass)

- **VPN/overlay-network topologies** (e.g. a machine reachable only
  over Tailscale). mDNS-based discovery (see below) gets this design
  further across real-world home networks than the original broadcast-
  only draft did -- multi-AP mesh systems commonly reflect mDNS across
  bands/VLANs for Chromecast/AirPlay/Sonos compatibility, and once a
  peer's address is known at all (mDNS or a manually configured static
  entry), arbitration itself is plain routed unicast, which doesn't
  care about subnet boundaries the way broadcast does -- but an overlay
  network like Tailscale is still out of scope for *discovery*: nothing
  here does mDNS-over-WireGuard or DNS-SD relay across it. A Tailscale-
  only machine can still participate via a manually configured `peers`
  entry using its Tailscale address (see
  [Config surface](#config-surface)); it just won't be auto-discovered.
- **Automatic peer failover as a first-class, fully-relied-on
  guarantee.** A lightweight mechanism is sketched (Phase 2) but not
  recommended for the initial ship -- see
  [Winner failure](#winner-failure).
- **Splitting one utterance's work across machines** (e.g. one records
  while another speaks the reply). Out of scope entirely -- the goal is
  "exactly one instance runs its normal, complete, unmodified pipeline
  end to end," not a distributed pipeline.
- **Changing anything about single-instance behavior of the pipeline
  itself.** This sits entirely upstream of `pipeline::run_session`; it
  either calls it once or doesn't call it at all.

## What signal is actually available today

Worth stating plainly, since it constrains the design: at the moment
`wake::detector::Detector::process` fires, the only per-detection
signal that exists today is the wake-word model's own confidence
score -- `Detection { score, primary_score, verifier_score, .. }` in
`wake/detector.rs`. `score` is the value the patience filter already
gates on (`p2` if the primary-stage score `p1 >= 0.5`, else `p1`
itself). **There is no volume/RMS signal computed anywhere in the
current audio path** -- `AudioFeatures` only ever produces mel/
embedding features for the NPU models, never a loudness figure.
Raw `i16` samples do briefly exist in `run_detect`'s `chunk` buffer
right before `listener.feed(&chunk)` is called, though, so adding RMS
is a small, local, cheap addition (a few lines, `O(chunk_len)`, only
computed on the chunk that actually triggers a detection -- not per
frame) rather than a new capture path. See
[Arbitration score](#arbitration-score-what-to-arbitrate-on) for why
this document recommends adding it rather than arbitrating on
confidence alone.

## The architecture

```
   Machine A (laptop)                      Machine B (desktop)
  ┌─────────────────────┐                 ┌─────────────────────┐
  │ Detector::process    │                 │ Detector::process    │
  │   → Detection(score,  │                 │   → Detection(score,  │
  │      rms)             │                 │      rms)             │
  └──────────┬───────────┘                 └──────────┬───────────┘
             │ unicast (mDNS-resolved                  │ unicast (mDNS-resolved
             │ or configured peers)                    │ or configured peers)
             │ WakeAnnounce  ───────────────────────►  │
             │  ◄─────────────────────────  WakeAnnounce
             ▼                                          ▼
  ┌─────────────────────┐                 ┌─────────────────────┐
  │ wait up to           │                 │ wait up to           │
  │ arbitration_window,   │                 │ arbitration_window,   │
  │ track best score seen │                 │ track best score seen │
  └──────────┬───────────┘                 └──────────┬───────────┘
             │                                          │
      mine > best seen?                          mine > best seen?
             │yes                                       │no
             ▼                                          ▼
   pipeline::run_session()                    back off silently
   (unchanged from today)                    (listener.reset(), continue)
```

Every instance runs the *identical* algorithm concurrently and
symmetrically -- there's no message that says "you won" or "you lost";
each side derives its own outcome from the same announced facts. This
is the key property that avoids needing a coordinator or a shared
clock: it's a distributed max computation, not a negotiation.

### Discovery: recommendation and alternatives

**Recommended: mDNS/Avahi**, replacing the broadcast-is-discovery
approach from the first draft. Each instance advertises a service (name
sketched below) via mDNS on startup and browses for the same service
type to build a live peer table (hostname → last-known address),
independent of and well before any actual wake-word detection happens.
This is a real, if small, change in shape from the original "no peer
table, no staleness" design -- worth being explicit about the tradeoff:

- **What it buys:** proper hostnames for free (every `WakeAnnounce`
  already carried a `hostname` field for logging, but mDNS makes
  *discovery itself* hostname-driven, which both feeds the peer catalog
  in [Config surface](#config-surface) and is what makes
  [losing-instance feedback](#losing-instance-feedback) ("handled by
  trevor-desktop") straightforward instead of an afterthought); no
  manual peer-list upkeep as more machines join the household (the
  thing a pure-static design would have made annoying); and, per the
  user's actual ask, meaningfully better reach than broadcast on real
  home networks -- see the cross-subnet honesty note just below.
- **What it costs:** a peer table that can go stale (a machine that's
  been off for a week needs its mDNS records to expire and be
  re-announced on return -- standard mDNS TTL/goodbye-packet behavior,
  not something this design has to invent) and, if implemented via the
  `mdns-sd` crate (recommended -- see below), one new dependency where
  the broadcast-only draft had zero.

**Does mDNS actually cross subnets?** Partially, and it's worth being
precise about *why*, since this was the direct ask: mDNS is, by
default, exactly as subnet-local as UDP broadcast was -- it's multicast
to `224.0.0.251`, not routed by IP routers any more than
`255.255.255.255` is. What actually changes the picture:
1. **Many real home/mesh networks already reflect mDNS across
   segments.** Multi-AP mesh systems (eero, Google Wifi/Nest,
   UniFi with mDNS reflection enabled, etc.) commonly bridge mDNS
   between bands/VLANs specifically so Chromecast/AirPlay/Sonos-style
   discovery works across the whole house -- if the user's network
   already does this for other devices, it does it for this too, for
   free, no extra config. `avahi-daemon` itself also supports acting as
   a reflector (`enable-reflector=yes` in `avahi-daemon.conf`) on a
   Linux box that happens to sit between two segments, if the user
   controls that box.
2. **Once a peer's address is known at all -- mDNS-resolved or
   manually configured -- arbitration traffic itself is unicast** (see
   [Transport](#transport-and-port)), which is ordinary routed IP
   traffic and does not care about subnet boundaries the way broadcast
   does. So the actual cross-subnet gap this design has is narrower
   than "arbitration doesn't work across subnets" -- it's specifically
   "a peer on a segment mDNS can't reach won't be *auto*-discovered,"
   which the static `peers` list (kept, see
   [Config surface](#config-surface)) closes by hand for that one
   machine.

Net answer to "does mDNS cover this": it gets meaningfully further than
broadcast did on the networks this is likely to actually run on, but
it's not a guarantee independent of the user's specific router/mesh
setup -- flagged as something to just try on the real hardware rather
than assumed, same spirit as the timing constants below.

**Implementation approach: the `mdns-sd` crate**, not shelling out to
`avahi-browse`/`avahi-publish`. Reasoning: `mdns-sd` is a small,
actively-maintained pure-Rust mDNS/DNS-SD implementation that does its
own multicast I/O -- it does **not** require `avahi-daemon` to be
installed or running at all, which sidesteps "what if Avahi isn't
enabled on this Omarchy install" as a failure mode entirely (worth
actually checking whether `avahi-daemon.service` ships enabled on
Omarchy before assuming it does). Shelling out to `avahi-browse
--parsable -t` and parsing its pipe-delimited stdout (this project's
usual "shell out rather than add a dependency" convention, used
elsewhere for e.g. the OpenClaw device-identity SQLite fallback) was
seriously considered, but browsing needs a *long-running*, incrementally
-event-emitting subprocess (not a one-shot command whose output gets
parsed and thrown away), which is a meaningfully worse fit for this
project's synchronous `run_detect` loop than a library call -- and
`avahi-publish` would need its own permanently-running child process
per instance just to keep the advertisement alive. One new dependency
buys a real reduction in moving parts here; recommended.

Service type: `_novad-arbitration._udp.local.`, TXT record carrying
`proto_version` (so a mismatched-version peer can be recognized and
skipped even before any `WakeAnnounce` exchange, not just after).

An optional **static peer list** (`[arbitration].peers` in
config.toml) is kept alongside mDNS, not replaced by it, for the two
reasons the first draft already identified plus the cross-subnet case
above:
1. **A peer mDNS genuinely can't reach** (no reflection on the user's
   network, or a deliberately different segment) -- the manual escape
   hatch.
2. **Known peer count enables an early short-circuit.** Whether a peer
   was learned via mDNS or configured by hand, once an instance has
   heard from every peer it currently knows about, it doesn't need to
   keep waiting out the rest of the arbitration window -- see
   [Timing](#timing). mDNS discovery actually makes this short-circuit
   *more* useful than the first draft's broadcast-only design did,
   since the peer table now usually isn't empty even before a
   `discovery = "static"` opt-in.

**Alternatives considered:**

- **UDP multicast** (join a multicast group like `239.255.x.x`) as a
  lighter-weight, no-new-dependency alternative to full mDNS/DNS-SD.
  Would still need to invent its own announce/TTL/goodbye-packet
  scheme from scratch to get a peer table with hostnames at all (mDNS's
  actual value-add here), so it ends up recreating a worse version of
  what mDNS already provides for free -- not recommended.
- **Pure static config, no discovery at all.** Simplest possible
  implementation. Rejected as the default for the same reason the
  first draft rejected it (editing config.toml on every machine every
  time a new instance joins the household), but remains fully
  supported as `discovery = "static"` for a network where mDNS
  reflection genuinely doesn't reach and the user prefers a fully
  manual, deterministic setup over relying on it.

### Arbitration score: what to arbitrate on

Recommendation: **RMS loudness as the primary signal, confidence score
as a tiebreaker, instance id as a final deterministic tiebreaker.**

Reasoning: openWakeWord-style detectors (see `wake/mod.rs`'s doc
comment on training) are trained to be reasonably volume-invariant on
purpose -- robustness to quiet/loud speech is a feature, not a bug, for
the detector's actual job (deciding "was the wake word said at all").
That's the wrong property for *this* job: two instances that both
clearly heard a clean "hey jarvis" can easily both land near 0.99
confidence regardless of which one the speaker was standing closer to,
giving little to arbitrate on. RMS/loudness, by contrast, degrades
much more continuously with distance and is a much closer proxy for
"which mic did the speaker actually intend to address" -- the same
intuition a person uses ("whoever heard it loudest was probably being
spoken to").

This isn't free of its own problem, though, and it's worth stating
plainly: **RMS also reflects each machine's independent mic gain/AGC
settings, not distance alone.** A machine with a hotter mic gain can
out-score a genuinely closer machine. Confidence score partially
compensates here (it's less gain-sensitive by design), which is why
it's kept as a tiebreak rather than dropped -- but a fully correct fix
(per-machine gain calibration) is out of scope. Flagged again in
[Open questions](#open-questions).

A fourth, optional signal sits above the floating-point comparisons:
**self-asserted priority** (`[arbitration].priority` in config, see
[Config surface](#config-surface)) -- a plain integer each machine sets
for itself ("the desktop should win close calls") and stamps into
every `WakeAnnounce` it sends. It's placed *between* confidence and the
final `instance_id` tiebreak, not above RMS/confidence entirely:
loudness and detector confidence are real, per-utterance signals about
who was actually being spoken to, and a human-assigned priority
shouldn't override "the other machine clearly heard this one, mine
barely registered" -- it's meant to settle the already-rare case where
RMS and confidence were both too close to call, not to let a
low-signal detection win outright because its machine is configured as
"important."

Concretely:

```rust
/// Normalized 0.0-1.0 (sample RMS / i16::MAX), computed once over the
/// audio chunk that made `Detector::process` return `Some(Detection)`
/// -- not per-frame, so this adds no meaningful cost to the hot loop.
const RMS_EPSILON: f32 = 0.02;
const SCORE_EPSILON: f32 = 0.01;

/// `true` if `mine` should win against `theirs`. Symmetric and total:
/// every instance evaluating the same pair of announcements reaches
/// the same answer, which is what lets this be leaderless. `priority`
/// is compared directly off each announcement (self-asserted, carried
/// on the wire -- see `WakeAnnounce` below), not looked up from a local
/// table, so both sides are guaranteed to agree on what each other's
/// priority actually is without needing a synced peer-priority table.
fn beats(mine: &WakeAnnounce, theirs: &WakeAnnounce) -> bool {
    if (mine.rms - theirs.rms).abs() > RMS_EPSILON {
        mine.rms > theirs.rms
    } else if (mine.score - theirs.score).abs() > SCORE_EPSILON {
        mine.score > theirs.score
    } else if mine.priority != theirs.priority {
        mine.priority > theirs.priority
    } else {
        // True floating-point ties (and equal, or both-default,
        // priority) are unlikely with real audio, but not impossible --
        // an arbitrary-but-deterministic, symmetric final tiebreak so
        // every instance still agrees.
        mine.instance_id < theirs.instance_id
    }
}
```

## Protocol details

### Message shapes

One UDP datagram per message, JSON-encoded (via `serde`/`serde_json`,
both already project dependencies -- no new serialization crate
needed). Small enough (well under any realistic MTU) that fragmentation
isn't a concern.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WakeAnnounce {
    /// Bumped only on a wire-incompatible change; unknown/mismatched
    /// versions are ignored rather than erroring, so a mixed-version
    /// household (one machine updated, one not) just fails open into
    /// "no peers" behavior for that pair instead of crashing either
    /// side.
    proto_version: u8,
    /// Random per-process (or persisted in the runtime dir so it's
    /// stable across restarts, unforced either way -- see
    /// "Instance identity" below). Purely a deterministic tiebreak key
    /// and a log-correlation id; never needs to mean anything to a
    /// human.
    instance_id: uuid::Uuid,
    /// Human-readable only -- machine hostname, for log lines and any
    /// future debug tooling. Never used in comparison logic.
    hostname: String,
    /// Which wake-word model fired -- arbitration only happens between
    /// announcements for the *same* wakeword (see
    /// "Cross-wakeword isolation" below); a mismatched wakeword is
    /// still logged as "peer seen" but never affects the outcome.
    wakeword: String,
    /// The detector's own combined confidence (`Detection.score`).
    score: f32,
    /// See "Arbitration score" above.
    rms: f32,
    /// Self-asserted, from this machine's own `[arbitration].priority`
    /// -- see "Arbitration score" above and "Config surface" below.
    /// Carried on the wire (not looked up from a local peer table) so
    /// both sides of a `beats()` comparison are guaranteed to agree on
    /// what it is.
    priority: i32,
}

// Phase 2 only -- see "Winner failure."
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WakeClaim {
    proto_version: u8,
    instance_id: uuid::Uuid,
    wakeword: String,
}
```

Example `WakeAnnounce` on the wire:

```json
{
  "proto_version": 1,
  "instance_id": "6f2a8e2e-6e9b-4b3a-9b0a-6e2b6f2a8e2e",
  "hostname": "trevor-laptop",
  "wakeword": "hey_jarvis",
  "score": 0.94,
  "rms": 0.31,
  "priority": 0
}
```

No `round_id`, no `timestamp`. This is deliberate -- see the next
section.

### Why no shared clock, no round id

A naive design would stamp each announcement with a wall-clock
timestamp and/or a round-correlation id, then reason about "which
announcements belong to the same utterance" by comparing timestamps
within some tolerance. Both pieces turn out to be unnecessary:

- **No round id needed for correlation.** Physically, a spoken wake
  word is a single few-hundred-millisecond event; the odds of two
  *unrelated* "hey jarvis" utterances landing within a few hundred
  milliseconds of each other on the same household LAN are
  essentially zero. So instead of explicitly correlating announcements
  into a round, each instance just opens a short window measured from
  its own local detection instant (`Instant::now()`, not `SystemTime`)
  and treats *any* same-wakeword announcement it receives during that
  window as "about this utterance." No shared identifier required.
- **No wall-clock timestamp needed for ordering.** The arbitration
  question is never "who detected it first" -- it's "who scored
  highest." Nothing in the algorithm needs two machines to agree on
  what time it is, which sidesteps NTP drift, suspend/resume clock
  jumps, and every other clock-skew failure mode a "highest timestamp
  wins" scheme would inherit. Each machine's window is purely local
  and relative; peers never need to be told when *they* were heard,
  only how loud/confident the detection was.

The one place real-world timing still matters is how *long* the
window needs to be to reliably catch a genuinely-simultaneous peer's
announcement despite each machine's detection firing at a slightly
different local instant -- covered next.

### Timing

```rust
/// Default arbitration window. See reasoning below.
const DEFAULT_WINDOW_MS: u64 = 400;
```

Reasoning, tied to actual constants elsewhere in this codebase:

- `wake::audio_features::CHUNK_SAMPLES` = 1280 samples @ 16kHz = 80ms
  per chunk. Because each machine's mic capture stream free-runs
  independently, two instances hearing the *same* physical utterance
  can have their chunk boundaries out of phase by up to a full chunk
  -- up to ~80ms of pure buffering skew between "instance A's
  detection fired" and "instance B's detection fired," even with
  identical hardware.
- `wake::detector::DEFAULT_PATIENCE` = 3 -- the patience filter
  requires 3 consecutive above-threshold frames before firing at all,
  meaning detection itself only ever fires after audio classified as
  wake-word for a minimum ~240ms; instances with different
  inference speeds (this NPU-accelerated per-frame step is measured
  in the low single-digit milliseconds per `run_detect`'s own comment
  on why inference is kept off the realtime audio callback -- not the
  dominant term, but a laptop NPU and a desktop NPU are not guaranteed
  identical) can cross that 3-frame threshold at slightly different
  moments even for byte-identical audio.
- LAN UDP delivery itself is normally low-single-digit milliseconds,
  occasionally spiking under Wi-Fi contention -- assume up to ~20ms of
  slack for this plus OS scheduling jitter.

Summing generously: ~80ms (chunk phase) + ~50ms (inference-speed
difference, generous) + ~20ms (network/scheduling) ≈ 150ms worst-case
realistic skew between two instances' local windows for the same
utterance. `400ms` gives roughly 2.5x margin over that estimate while
staying well under the point a user would perceive a pause before
their command starts being heard. This is a starting point for live
tuning, not a value treated as final -- flagged in
[Open questions](#open-questions), same spirit as the conversation-flow
doc's `idle_timeout` being tuned live rather than fixed up front.

**Early short-circuit when peers are known:** if the current peer table
(mDNS-discovered peers plus anything in `[arbitration].peers`) is
non-empty, an instance can stop waiting as soon as it has received one
announcement from every peer it currently knows about, rather than
always waiting the full window. This is one of the concrete practical
benefits of switching discovery to mDNS (see
[Discovery](#discovery-recommendation-and-alternatives)): the first
draft's broadcast-only design had no peer table at all outside of
manually configured `peers`, so this short-circuit essentially never
applied unless the user had done that setup by hand; with mDNS,
`discovery = "mdns"` populates a real peer table on its own, so the
common case gets the latency win for free. A machine with zero known
peers (fresh install, nothing else on the network yet) still always
spends the full window -- there's no way to know "nobody's out there"
faster than waiting to find out, which is exactly the behavior
[No peers found](#no-peers-found-the-default-case) already covers.

**Reliability over unacknowledged UDP:** rather than a single
fire-and-forget packet, each `WakeAnnounce` is sent 3 times in quick
succession (e.g. at +0ms, +40ms, +80ms) with the same content;
receivers de-duplicate by `(instance_id, wakeword)` within the window.
This is cheap (three ~150-byte UDP sends) and meaningfully reduces the
chance that a single dropped packet causes a peer to never learn about
a competing detection at all -- see
[Packet loss](#packet-loss) for the residual risk this doesn't fully
close.

### Cross-wakeword isolation

Two instances configured for different wake words (hypothetically
"hey jarvis" on one machine, a different stock phrase on another) must
never arbitrate against each other -- each `WakeAnnounce` carries
`wakeword`, and an instance only ever compares its own detection
against announcements carrying the *same* `wakeword`; a mismatched one
is logged (useful for confirming peers are actually being heard at
all) but never affects `beats()`.

### Instance identity

`instance_id` (a `Uuid`) is generated once and can be either purely
in-memory (regenerated every process start -- simplest, and sufficient
since it's only ever used as a tiebreak key and a log-correlation id,
never as a long-lived identity anything persists against) or persisted
to a small file under the existing runtime dir (`$XDG_RUNTIME_DIR/
omarchy-novad/instance_id`) if stable-across-restarts log correlation
turns out to matter in practice. Recommendation: start in-memory-only
(one line, `Uuid::new_v4()` at `run_detect` startup) -- persistence is
a trivial addition later if it's missed, not a decision worth blocking
on now.

### Transport and port

Plain UDP over `std::net::UdpSocket` -- no new dependency, and
deliberately *not* built on `tokio` even though this crate already
depends on it (for `omarchy-novad serve`'s axum server): `run_detect`'s
whole loop today is fully synchronous (`cpal` → `mpsc::channel` →
blocking `for samples in rx`), and arbitration's own wait is naturally
expressed the same way -- send, then loop
`socket.recv_from()` with `set_read_timeout` shrinking toward the
window deadline, collecting matching announcements. Pulling in an
async runtime just for this would be a bigger architectural change
than the feature itself and would fight the existing synchronous
detect-loop model rather than fit it.

UDP is the right transport on its merits too, not just for the
lack of new dependencies: no connection-setup handshake to pay for
inside a sub-second timing budget, and the message itself is naturally
"fire and forget, duplicates and drops both tolerable" -- exactly
UDP's shape, and exactly *not* what TCP's ordered/reliable-stream
guarantees are for.

**Unicast, not broadcast, once peers are known.** The first draft sent
`WakeAnnounce` to the LAN broadcast address; this revision sends it via
plain unicast to every address in the current peer table (mDNS-
resolved plus any manually configured `peers`), one send per known
peer, still 3x-repeated per peer for the same reliability reasoning as
before. This is strictly better now that a peer table exists at all
(see [Discovery](#discovery-recommendation-and-alternatives)): it
reaches routed peers on other subnets that broadcast never could, it
doesn't depend on `SO_BROADCAST`/computing a broadcast address at all,
and it doesn't put packets on the wire for every host on the segment
that isn't even running omarchy-novad. The one case this *doesn't*
cover on its own is a peer neither mDNS nor config knows about yet --
but that's a discovery gap, not something broadcast-as-arbitration-
transport would have actually fixed either (an unknown peer wasn't
going to be arbitrated against regardless of transport).

- **Port**: `51530` (arbitrary, in the dynamic/private range, unlikely
  to collide with anything else on a home LAN) for both mDNS-service
  resolution and the actual unicast arbitration traffic, configurable
  via `[arbitration].port`.
- **Firewall note** (operational, not code): a local firewall
  (`ufw`/`firewalld`) may need an explicit allow rule for inbound UDP
  on this port (and, separately, for mDNS's own `5353/udp` if that
  isn't already open for other reasons) for peers to reach each other
  -- worth a README callout when this ships, not something the daemon
  can fix for the user.

## Failure and edge-case handling

### No peers found (the default case)

This is the case that matters most. Two layers guarantee it costs
nothing:

1. `[arbitration].enabled = false` (the default) means the arbitration
   module is never constructed at all -- `run_detect` holds
   `arbitration: Option<Arbitrator>` and it's simply `None`; the gate
   at the detection site (see
   [Where this hooks in](#where-this-hooks-into-the-pipeline)) is a
   single `match`/`if let` that falls straight through to today's
   `match &trigger { .. }` call with no socket ever opened, no packet
   ever sent, no wait ever incurred.
2. Even with `enabled = true`, if the peer table is empty (nothing
   discovered via mDNS yet, nothing in `peers`) and no announcement
   arrives, the outcome after `window_ms` elapses
   is "no competing announcement seen ⇒ I win by default" -- correct
   behavior, but note this path *does* still cost the full
   `window_ms` wait, unlike case 1. A single physical machine that
   turns the feature on gratuitously (nothing to arbitrate against,
   ever) would pay a latency tax for no benefit -- worth a doc/README
   note that `enabled` should stay `false` unless there's an actual
   second instance on the network, not turned on "just in case."

### A peer that's slow or drops off mid-arbitration

No special handling needed beyond the window itself -- "slow" and
"dropped off" both just look like "no announcement arrived from that
peer within the window" to everyone else, which is already the
steady-state case for `enabled = true` with no peers at all. Nothing
distinguishes "network partition" from "not currently running" from
"genuinely didn't hear the wake word" -- deliberately, since building
that distinction would need a liveness/heartbeat channel this design
otherwise has no reason to maintain.

### Packet loss

The 3x-repeat within the window (see [Timing](#timing)) handles the
common case (one or two packets dropped, not all three, on an
otherwise-healthy LAN). The residual, honestly-stated risk: if *every*
copy of the relevant announcement is lost in *both* directions between
two instances that both genuinely detected the same utterance, neither
learns about the other, and both independently conclude they're the
only listener and proceed -- i.e., for that one utterance, exactly
today's pre-arbitration behavior. This is a real but rare failure mode
of unacknowledged UDP gossip; closing it fully would need acked,
retried delivery (meaningfully more protocol complexity, and a bigger
latency budget to allow for retries) for a failure this infrequent on
a healthy home LAN. Not recommended to build ahead of evidence it's
actually a problem in practice.

### Winner failure

The hardest edge case, and the one this document takes the most
conservative position on. If the winning instance itself fails after
arbitration decides it won but before (or during) the pipeline session
-- silence-detection wedge, crash, network drop, the machine
literally losing power -- should a peer take over?

**Recommendation for the initial ship: no automatic failover.** A lost
winner means that one utterance gets no response, exactly as if no
instance had heard the wake word at all -- the user says "hey jarvis"
again. Reasoning: building real failover needs a liveness signal (the
sketch below), and a *false positive* on that liveness check (peer
concludes the winner died because a single UDP packet confirming it's
alive was dropped, when the winner is in fact fine and already
recording) reintroduces the exact double-recording problem this whole
feature exists to prevent, just relocated to a rarer trigger. Given
how narrow the actual failure window is (a full daemon crash between
"decided to record" and "recording audibly starts" is a small slice of
time, and a genuinely crashed daemon needs process supervision/restart
regardless of anything this feature does), the cost/benefit favors
"rare miss, easily recovered by the user just repeating themselves"
over added complexity with a real false-positive risk.

**Sketch for a possible Phase 2, if dogfooding shows this is actually
a nuisance in practice:** the winner sends a single `WakeClaim`
message the moment it actually begins recording (i.e. right as
`pipeline::run_session` starts, not at arbitration-decision time --
proceeding into the pipeline and being *reachable enough to send a
packet* are close enough to prove liveness for this purpose). Losing
instances, instead of discarding all state the instant their own
window closes, hold onto "who I lost to and by what score" for one
additional short `claim_timeout_ms` (generous relative to the
arbitration window itself -- on the order of 1.5-2s, since it's
bounding "did the winner even start," not adding to normal-case
latency for anyone). If no `WakeClaim` arrives from the presumed
winner in that time, the next-highest-scoring peer that saw itself
lose promotes itself and proceeds. Cap promotion at one hop (don't
chain indefinitely if the promoted instance also fails to claim) --
beyond that, a second miss is better treated as "something's
structurally wrong on this network," not smoothed over indefinitely.
This is deliberately **not** part of the Phase 1 recommendation --
see [Open questions](#open-questions).

### Clock skew

Not applicable by construction -- see
[Why no shared clock, no round id](#why-no-shared-clock-no-round-id).
Worth restating here only to confirm it was considered and
deliberately designed around, not overlooked.

### What "back off" concretely means

For the losing instance(s), backing off means: **the detection is
simply never handed to `trigger`.** Concretely, in `run_detect`'s loop,
the branch that would call `pipeline::run_session(&pipeline_cfg)` (or
`run_shell(cmd)` for a `Trigger::OneShotCommand`) is skipped entirely;
`listener.reset()` still runs (so the detector's internal state
doesn't carry stale embedding history into the next wake-word
listen), and the loop continues. This gets "must not start recording,
must not speak, must not show its popup/panel" for free, with no
explicit suppression logic needed anywhere else: `PopupState` is only
ever written from inside `pipeline::run_session` (see `pipeline.rs`'s
`popup::write_state` calls) and `crate::converse::run`'s TTS path is
only ever reached from inside that same call -- if `run_session` is
never invoked, none of that machinery runs, full stop. The popup
simply never appears on a losing machine; there is no "hide it after
showing it" step to get right.

### Losing-instance feedback

The first draft left this fully silent and flagged it as an open
question. Resolved: a losing instance shows a brief, subtle, distinct-
from-the-normal-popup notification naming the winner -- e.g. "Heard by
trevor-desktop" -- rather than staying silent or reusing the full
confirm/dictation card. Reasoning: fully silent is actively confusing
the first few times a user says the wake word near a losing machine
and nothing at all happens there (was it not heard? is the mic
broken?) -- some acknowledgment that "yes, this machine heard you too,
it's just not the one answering" is worth a small amount of UI, as
long as it doesn't look like the real popup (which would incorrectly
suggest this machine is also doing something) and doesn't linger.

Mechanically, this reuses the existing `PopupState`/`PopupCard.qml`
machinery rather than inventing a second notification system, with one
new phase:

- A new `PopupPhase::HandedOff` variant, `text` carrying the winner's
  `hostname` straight from the `WakeAnnounce` that beat this instance
  (already on hand -- no extra lookup).
- `run_detect`'s arbitration-lost branch writes this state (in place
  of the `tracing::info!`-only stub in the earlier sketch) instead of
  calling `pipeline::run_session`, then a short self-clearing timer
  (on the order of 2s -- meaningfully shorter than `Ready`'s own
  dismiss-on-timeout, since this is acknowledgment, not content the
  user needs time to read) resets it back to `PopupPhase::Idle`.
- `PopupCard.qml` renders `HandedOff` with a visibly smaller/quieter
  treatment than every other phase's card -- no confirm/deny buttons,
  no animated border, muted text -- specifically so it reads as
  "FYI" rather than "something is happening here too."

This is additive to the arbitration protocol itself (no wire-format
change -- `hostname` was already on `WakeAnnounce`) and can be built or
skipped independently of everything else in this document.

## Config surface

Follows this crate's existing per-feature-section pattern (`[detect]`,
`[popup]`, `[chime]` in `src/config.rs`) -- a plain, `#[serde(default)]`
struct with `impl Default`, not required to have any CLI flag
equivalent (matching `TtsConfig`/`ChimeConfig`, which are config-file-
only; this is a set-once network setting, not something worth
per-invocation flag plumbing in `main.rs`).

```toml
[arbitration]
# Off by default -- single-machine users are unaffected either way,
# but this also guards against the wasted per-detection wait described
# in "No peers found" if turned on with nothing to arbitrate against.
enabled = false

# UDP port used for both mDNS-resolved and manually configured peers'
# arbitration traffic.
port = 51530

# Optional: pin mDNS advertise/browse and outgoing arbitration sends to
# one interface on a multi-homed machine (e.g. "wlan0"). Empty = let
# the OS pick.
bind_interface = ""

# "mdns" (default): discover peers automatically via mDNS/DNS-SD (see
# "Discovery"), no peer list required. "static": skip mDNS entirely,
# talk only to `peers` -- for a network where mDNS reflection genuinely
# doesn't reach and a fully manual setup is preferred.
discovery = "mdns"

# This machine's own tie-break weight, self-asserted and stamped into
# every WakeAnnounce this instance sends -- NOT a per-peer table (see
# "Arbitration score": priority is compared directly off the two
# announcements being weighed, so both sides always agree on what it
# is, with no local lookup to get out of sync). Only matters in the
# already-rare case where RMS and confidence are both too close to
# call. Set higher on a machine that should win those close calls --
# e.g. 10 on the desktop, leave the laptop at the default (0).
priority = 0

# Explicit peer addresses ("hostname:port" or "ip:port"), for a peer
# mDNS can't reach, or the sole peer source when discovery = "static".
# Every instance actually seen (via mDNS or already listed here) also
# gets appended to this list automatically the first time it's heard
# from -- see "Discovered-peer catalog" below -- so after a machine has
# been running for a while, this fills in as a readable record of every
# peer it's ever heard from, editable by hand at any time.
peers = []

# How long to wait for competing announcements before deciding.
window_ms = 400
```

`ArbitrationConfig` struct shape to match:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ArbitrationConfig {
    pub enabled: bool,
    pub port: u16,
    pub bind_interface: String,
    pub discovery: DiscoveryMode, // enum: Mdns | Static
    pub priority: i32,
    pub peers: Vec<String>,
    pub window_ms: u64,
}

impl Default for ArbitrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 51530,
            bind_interface: String::new(),
            discovery: DiscoveryMode::Mdns,
            priority: 0,
            peers: Vec::new(),
            window_ms: 400,
        }
    }
}
```

Added to `Config` the same way `chime`/`popup` are:
```rust
#[serde(default)]
pub arbitration: ArbitrationConfig,
```

### Discovered-peer catalog

The direct ask this addresses: "when novad discovers other devices it
puts them in the config." A newly-seen peer (mDNS-resolved, or heard
from directly for the first time in a `WakeAnnounce` exchange) gets a
new `peers` entry appended to `config.toml` automatically, once, so the
list becomes a visible, growing record of every machine this instance
has ever actually heard from -- not something the user has to
pre-populate by hand for auto-discovery to work at all.

Two things worth being deliberate about, since "the daemon rewrites
your config file" is exactly the kind of thing that goes wrong quietly
if done carelessly:

- **Append-only, never a full rewrite.** This project's `toml` crate
  usage elsewhere is read-only (`config::load()` deserializes once at
  startup); nothing here should start round-tripping the user's config
  through a TOML *writer*, which would risk silently dropping comments
  or reordering/reformatting content the user hand-edited. Instead, a
  first-seen peer's line is appended as raw text to the end of the
  file (`std::fs::OpenOptions::new().append(true)`), inside the
  existing `[arbitration]` table if config.toml's `peers = []` is the
  last thing in that table already, or as its own clearly-commented
  follow-up assignment otherwise (TOML tolerates a key being reassigned
  further down the same table, so a naive "just append another `peers
  += [...]`-style line" isn't actually valid TOML -- the real
  implementation needs to either parse-and-emit just the `peers` array
  specifically, leaving everything else in the file untouched, or track
  known peers in a small sidecar file instead and treat `config.toml`'s
  own `peers` as pure user input merged in at load time. Left as an
  implementation-phase decision, not resolved here -- see
  [Open questions](#open-questions)).
- **A peer already present in `peers` (hostname match) is never
  touched again.** Auto-discovery only ever *adds* a never-before-seen
  hostname; it never edits an existing entry's priority-adjacent
  comment or reorders anything, so a value the user has already hand-
  tuned is permanent until they change it themselves.

Newly-appended entries get a short comment noting they were
auto-discovered (e.g. `# auto-discovered 2026-09-12`), so the file
stays readable as "which of these did I add vs. which showed up on
their own."

## Where this hooks into the pipeline

Exactly one gate point, in `main.rs::run_detect`'s mic-feed loop --
today:

```rust
if let Some(detection) = listener.feed(&chunk)? {
    println!("\n[omarchy-novad] Wake word detected! score={:.3}", detection.score);
    match &trigger {
        Trigger::VoxtypeDictation => pipeline::run_session(&pipeline_cfg),
        Trigger::OneShotCommand(cmd) => run_shell(cmd),
    }
    listener.reset();
}
```

Becomes (sketch, not final code):

```rust
if let Some(detection) = listener.feed(&chunk)? {
    println!("\n[omarchy-novad] Wake word detected! score={:.3}", detection.score);

    let should_proceed = match arbitrator.as_mut() {
        None => true, // feature off -- identical to today, no socket touched
        Some(a) => a.arbitrate(&detection) == ArbitrationOutcome::Proceed,
    };

    if should_proceed {
        match &trigger {
            Trigger::VoxtypeDictation => pipeline::run_session(&pipeline_cfg),
            Trigger::OneShotCommand(cmd) => run_shell(cmd),
        }
    } else {
        tracing::info!("[arbitration] lost to a peer instance -- staying silent");
    }
    listener.reset();
}
```

Both `Trigger` variants are gated uniformly -- the point is "one
physical utterance, one instance acts," regardless of what that
instance's configured action is (standalone pipeline, OmaPilot
handoff, or an arbitrary custom command).

Two things worth calling out about this placement:

- **The winning instance also waits.** Arbitration has to run (and be
  waited out, in whole or via the short-circuit) *before* dispatching
  to `trigger` even for the eventual winner -- it can't know it won
  until either the window elapses or every known peer has reported in.
  This means the `enabled = true`-with-real-peers case adds up to
  `window_ms` of latency even to the machine that ultimately proceeds.
  Accepted cost of turning the feature on, bounded and short (~400ms
  default); zero cost when off or alone, per
  [No peers found](#no-peers-found-the-default-case).
- **This blocks the mic-feed loop for the wait's duration**, exactly
  like `pipeline::run_session` already does for a session's entire
  multi-second duration (see `pipeline.rs`'s own module doc: "Wake-word
  detection itself is naturally paused for the session's duration
  since the caller doesn't feed it more audio until this returns").
  Arbitration's wait is the same shape of blocking, just far shorter --
  no new concurrency model needed, consistent with how this loop
  already works today.

## Suggested module layout

Mirrors the existing per-feature module pattern (`src/wake/`,
`src/router/`):

```
src/arbitration/
  mod.rs        // Arbitrator, ArbitrationOutcome, arbitrate()
  protocol.rs   // WakeAnnounce / WakeClaim structs + (de)serialization
  discovery.rs  // mdns-sd advertise/browse, the live peer table
  transport.rs  // UdpSocket setup, unicast send/recv to known peers
  peers.rs      // config.toml peers list <-> peer table merge, the
                // append-on-first-seen catalog write (Phase 4)
```

## Rough implementation phasing

1. **Instrumentation only.** Add `rms` to `Detection` (compute it in
   `run_detect`'s chunk-feed loop, on the triggering chunk only), log
   it alongside `score` on every detection. No networking at all yet.
   Purpose: validate on the user's actual laptop+desktop pair that RMS
   is actually a usefully discriminating signal between the two
   machines' real mic/room setup *before* building the protocol around
   it -- cheap, zero-risk, and directly informs whether
   [Arbitration score](#arbitration-score-what-to-arbitrate-on)'s
   weighting needs adjusting.
2. **Discovery.** Add the `mdns-sd` dependency, mDNS advertise +
   browse for `_novad-arbitration._udp.local.`, building an in-memory
   peer table (hostname → address) -- no arbitration protocol yet, just
   confirm the two real machines actually see each other reliably
   (including after a sleep/wake or a machine coming back after being
   off), and settle the open question of whether the user's actual
   router/mesh reflects mDNS across whatever segments the laptop and
   desktop land on (see
   [Discovery](#discovery-recommendation-and-alternatives)'s honesty
   note on this) before anything depends on it working.
3. **Core arbitration MVP.** `ArbitrationConfig`, `priority` and its
   place in `beats()`, the `arbitration` module (unicast transport to
   the Phase 2 peer table, `WakeAnnounce` only, no `WakeClaim`), wired
   into the single gate point in `run_detect`. Static `peers` supported
   (`discovery = "static"`, or alongside mDNS) with the short-circuit
   from [Timing](#timing). No failover -- a lost winner is just a
   missed turn, same as not being heard at all. No losing-instance UI
   yet either (log-only, same as the first draft's sketch). Ship this
   and dogfood across the two real machines for a while before deciding
   what Phase 4 needs to contain.
4. **Discovered-peer catalog + losing-instance feedback.** The
   append-to-`config.toml` mechanism from
   [Discovered-peer catalog](#discovered-peer-catalog) (including
   resolving the TOML-append implementation question flagged there),
   and the `PopupPhase::HandedOff` toast from
   [Losing-instance feedback](#losing-instance-feedback). Both are
   additive to the wire protocol and safely deferrable past the MVP --
   grouped here because both are "make it pleasant to actually live
   with day to day" rather than "make it correct."
5. **Robustness hardening**, informed by Phase 3/4 dogfooding rather
   than built speculatively: malformed/foreign/unknown-version packet
   handling (ignore, don't error), the `WakeClaim` failover sketch *if*
   lost winners prove to be an actual nuisance in practice.
6. **Polish.** Combine RMS + confidence with tunable weights if the
   fixed epsilon-based tiebreak (see
   [Arbitration score](#arbitration-score-what-to-arbitrate-on)) proves
   too coarse live; a debug surface (e.g. `omarchy-novad arbitration
   status` or similar) showing the last few arbitration rounds and
   their outcomes, for troubleshooting "why did the wrong machine
   win"; README section covering setup (enabling on both machines,
   firewall note from [Transport](#transport-and-port)).

## Open questions

Resolved by the first feedback pass:

- ~~RMS vs. confidence weighting~~ -- accepted as a reasoned starting
  point; Phase 1 stays instrumentation-only precisely so this can be
  tuned against real data before the protocol locks it in. No design
  change needed, just confirmed as the plan.
- ~~Is winner-failover worth building at all~~ -- **deferred, not
  decided against.** Put on the back burner: not part of any phase
  above, no `WakeClaim` work planned until/unless real dogfooding shows
  a lost winner is actually a recurring nuisance (see
  [Winner failure](#winner-failure) for the reasoning that was already
  leaning this way).
- ~~`window_ms` tuning~~ -- accepted as a starting point, same as RMS
  weighting; needs live tuning on the real hardware pair, not a design
  change.
- ~~Tie-break rule~~ -- resolved as self-asserted per-machine
  `priority`, wired into `beats()` and `config.toml` -- see
  [Arbitration score](#arbitration-score-what-to-arbitrate-on) and
  [Config surface](#config-surface).
- ~~Losing-instance feedback~~ -- resolved as a subtle `HandedOff`
  toast naming the winner -- see
  [Losing-instance feedback](#losing-instance-feedback).
- ~~Cross-subnet discovery~~ -- addressed by switching discovery to
  mDNS, with an honest caveat about what it does and doesn't guarantee
  -- see [Discovery](#discovery-recommendation-and-alternatives).

Still open:

1. **The `[arbitration].peers` append mechanism.** Flagged in
   [Discovered-peer catalog](#discovered-peer-catalog): appending to
   `config.toml` without a full TOML round-trip (to avoid disturbing
   the user's own formatting/comments elsewhere in the file) needs an
   actual implementation decision -- targeted append of just the
   `peers` array, vs. tracking discovered peers in a separate sidecar
   file and merging `config.toml`'s `peers` in as user-provided input
   at load time. Both are workable; this document doesn't pick one.
2. **Default `priority` for an auto-discovered (not yet hand-tuned)
   peer.** `0` (this document's default) means a freshly-discovered
   peer starts tied with every other default-priority machine --
   reasonable, but worth confirming that's actually the desired
   out-of-the-box behavior once there are 3+ machines in the picture
   rather than just the laptop/desktop pair this was designed against.
3. **Whether `avahi-daemon` ships enabled on the user's actual Omarchy
   install(s).** `mdns-sd` doesn't need it (see
   [Discovery](#discovery-recommendation-and-alternatives)), so this
   only matters if some *other* reason emerges to prefer shelling out
   to the system Avahi instead of the pure-Rust crate -- not expected
   to matter for the recommended approach, listed for completeness.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
