# Conversation flow: clean-sheet redesign

Status: implemented (all 7 phases from [Rough implementation
phasing](#rough-implementation-phasing)), not yet tested live. Covers
`src/converse.rs`,
`src/conversation/mod.rs`, `src/tts/mod.rs`, and the two panel QML
files (`quickshell/OpenClawConversation.qml`,
`plugin/ConversationPanel.qml`) -- everything that runs once an
utterance is routed to OpenClaw (`Intent::External`/`Intent::Coding`,
see `pipeline.rs`'s `router::is_external_handoff` branch).

## Why redo it

The current flow (`2e92f98`, `ac86ccb`, `ea29429`) is the result of
two rounds of live patching on top of an originally-dictated spec, and
it shows: it works, but it's a *chat app with a microphone bolted on*,
not a voice assistant. Concretely:

- **Every turn needs a mouse.** Idle waits for a "Record" click.
  A transcript then sits in an editable box waiting for a "Confirm"
  click (or Enter, but that still means focusing a text field with the
  mouse first most of the time). Nothing about a normal turn can be
  done by voice/keyboard alone except the talking itself.
- **The confirm box is a trust-nothing gate, every single time.**
  Useful when ASR gets something wrong, but voxtype's auto-stop and
  transcription are solid now (see `[[voxtype-sliding-window-hallucination]]`-
  era fixes) -- treating *every* transcript as guilty until proven
  innocent is the wrong default cost/benefit once that's true.
- **The TL;DR marker is a hack leaking into every reply.** OpenClaw is
  asked to prefix a machine-parseable `"TL;DR:"` line so
  `split_tldr()` can scrape it back out. It works, but it's an
  obviously bolted-on instruction, fragile to formatting drift, and
  it's the wrong shape of ask -- a real assistant doesn't write a memo
  and then have a second process extract the elevator pitch.
- **Nothing is interruptible.** `tts::speak()` blocks the whole loop
  until every sentence has played through `paplay`. You cannot start
  talking again until Jarvis finishes its sentence, cannot cancel a
  reply you've heard enough of, and cannot interrupt "Thinking..." if
  you realize mid-wait that you asked the wrong thing.
- **It can't end itself.** Forget to click Stop and the daemon sits
  inside `converse::run` forever -- which, because wake-word detection
  and the conversation loop share one blocking call in `run_detect`
  (`Trigger::VoxtypeDictation => pipeline::run_session(...)`, which
  itself calls `converse::run` and doesn't return until the
  conversation ends), means **"hey jarvis" stops working at all**
  until someone notices and stops it. A real assistant conversation
  has a sense of its own idleness.
- **The only feedback is a colored dot and a label.** No audio cues at
  all -- every state transition is silent unless you're looking at a
  docked side panel.

None of this is a bug in the sense of "wrong code" -- `converse.rs` is
carefully written and well-tested for what it's trying to be. It's the
wrong shape: a manually-driven review-then-send chat client, when what
was wanted is a spoken assistant.

## Design goals

1. **Voice + one hardware key is the whole interface.** The panel
   becomes an optional glanceable log, not where the interaction
   happens. A full conversation should be possible with zero clicks.
2. **Trust the transcript by default.** Auto-stop already means the
   turn is over; sending it should follow immediately, the same way a
   person doesn't pause a conversation to silently reread what they
   just said before letting the other person respond. Correction
   happens the way humans do it -- say something else, or type a
   correction into the always-present chat box -- not through a modal
   gate in front of every turn.
3. **Barge-in.** Pressing the talk key at any point -- mid-"Thinking",
   mid-sentence of a spoken reply -- cancels whatever's playing/running
   and starts listening immediately. This is the single most
   "Jarvis" feature on this list and the current design has none of
   it.
4. **The loop knows when it's over.** An idle timeout ends the
   conversation gracefully and hands control back to wake-word
   detection, instead of silently wedging it.
5. **Explicitly not full-auto relisten.** `2e92f98`'s finding stands:
   voxtype firing itself up right after every reply, unprompted, was
   worse than the click-to-talk it replaced. The fix for "too much
   friction" is a fast physical trigger, not removing the trigger.

## Non-goals (this pass)

- **Wake-word re-entry mid-conversation** (saying "hey jarvis" again
  instead of a hotkey to start the next turn). Architecturally
  blocked today: `run_detect`'s mic-capture loop is fully synchronous
  with `pipeline::run_session`/`converse::run` -- no audio reaches the
  wake-word detector at all while a conversation is running (see
  `main.rs:719-744`, the `for samples in rx` loop; deliberately, so
  the recording's own audio can't self-trigger). Making the detector
  run concurrently, and arbitrating mic ownership between it and
  voxtype's own capture, is a real concurrency project on its own.
  Flagged as a natural follow-up, scoped out here -- see
  [Open questions](#open-questions).
- Multi-speaker / diarization, changing the LLM summarizer, changing
  OpenClaw transport (gateway WebSocket streaming stays as-is).

## The interaction model

```
                    ┌─────────────┐
        talk key ──▶│  Listening  │── silence/auto-stop ──▶ send immediately
                    └─────────────┘        (no review gate)
                          ▲  │
              talk key    │  │ talk key (barge-in cancels + restarts)
           (barge-in)     │  ▼
                    ┌─────────────┐
                    │  Thinking   │── reply arrives ──▶┐
                    └─────────────┘                    │
                          ▲                             ▼
                          │                       ┌─────────────┐
              talk key (barge-in cancels) ◀────────│  Speaking   │
                                                    └─────────────┘
                                                          │
                                            no talk key within
                                            idle-timeout window
                                                          ▼
                                                  conversation ends,
                                              wake-word listening resumes
```

Every arrow above is reachable without touching the mouse. The panel,
if open, mirrors all of this; it just isn't required for any of it.

### Trigger: a talk key, not click-to-record

Replace the "Record" button with a global hotkey. Rather than this
project inventing its own press/hold convention from scratch, mirror
voxtype's own `[hotkey] mode` (`src/config/hotkey.rs`'s
`ActivationMode`, already `PushToTalk` (hold) or `Toggle` (press
once, press again) over there) -- same two names, same choice,
so the "talk key" *feels* like the same input primitive as voxtype's
own dictation hotkey instead of a second, differently-behaved control
living right next to it. Add a matching `ActivationMode` choice to
`ConverseConfig`/`config.toml`'s new `[converse]` section (default
`push_to_talk`, matching voxtype's own default) rather than hardcoding
one shape.

Both modes are just Hyprland-side keybind recipes on top of the
existing `ConversationAction::Listen`/`StopListening` control-socket
actions (`conversation/mod.rs`) -- no new IPC needed for
`push_to_talk`:

```lua
-- push_to_talk: press starts, release stops -- Hyprland's bindr
-- (release-triggered) covers the "release" half directly, no
-- key-down/key-up plumbing of our own required.
o.bind("SUPER + comma", "Talk to Jarvis", "omarchy-novad converse listen")
o.bindr("SUPER + comma", "Stop talking", "omarchy-novad converse stop-listening")
```

`toggle` mode needs one new CLI entry point, `omarchy-novad converse
talk` -- a single bind that starts listening if idle and stops if
already listening (checks current phase via the same state file the
panel reads, then dispatches to the existing `Listen`/`StopListening`
action):

```lua
o.bind("SUPER + comma", "Talk to Jarvis (toggle)", "omarchy-novad converse talk")
```

Either way, the panel's Record button becomes one more caller of the
same actions, same as today -- see
[keep Record as fallback](#open-questions).

Crucially, the **same key works in every phase**, not just idle:
- Idle → starts listening (today's `Listen` action, unchanged).
- Thinking/Speaking → **barge-in**: cancel the in-flight handoff or
  playback, then start listening. New behavior (see
  [Barge-in](#barge-in)).
- Listening → no-op if held/already listening; on the toggle variant,
  a second tap ends the recording early (today's `StopListening`,
  unchanged).

### Auto-send, no confirm gate

Remove `ConversationPhase::Confirming` and `wait_for_review` as a
*blocking* step. The moment `listen_interruptibly` returns a non-empty
transcript, it goes straight to the handoff -- same as
`ConverseConfig`'s existing `Thinking` transition, just without the
`Confirming` phase in between.

What happens to the editable box? It stays, but changes role: from a
"nothing proceeds until you act on this" gate to a "here's exactly
what was heard, in case you want to fix it" toast. Concretely:
- The transcript is sent immediately in the background.
- The panel (if open) shows the outgoing bubble right away, same as
  any chat app -- not a pending/unsent state.
- If voxtype produced empty/near-empty output (nothing said, or pure
  noise), don't send at all -- silently return to idle, same as
  today's `ListenOutcome::Text(t) if t.is_empty() => continue
  'session`.
- The always-present chat box (already in `plugin/ConversationPanel.qml`)
  remains the correction mechanism: type the fixed version and send it
  as an ordinary follow-up turn if the transcript came out wrong. No
  special "edit-the-last-turn" plumbing needed -- OpenClaw already
  gets the fix in conversational context ("no, I meant X") the same
  way a human correction works.

This is the single biggest behavior change here and the most
reversible-feeling one to get wrong, so it's called out explicitly in
[Open questions](#open-questions) rather than assumed.

### Barge-in

New capability: an in-flight `Thinking` or `Speaking` phase can be
cancelled by the talk key.

- **During `Speaking`**: `tts::speak()` currently blocks
  `converse::run`'s thread directly, with no way to interrupt it.
  Move it to a background thread the same way
  `run_handoff_with_progress` already runs the OpenClaw call on one
  (`converse.rs:322-362` is the existing pattern to copy) -- speak on
  a thread, watch `control_rx` on the loop thread meanwhile. Cancelling
  means: kill the in-flight `paplay` child (needs `tts::speak` to
  return a handle/cancellation token instead of blocking to
  completion -- a small `tts` module rework, see
  [Data model changes](#data-model-changes)) and stop queuing further
  sentences.
- **During `Thinking`**: `run_handoff_with_progress` already runs the
  handoff on a background thread and polls a channel
  (`converse.rs:322`) -- it just doesn't currently also watch
  `control_rx`. Add that: a `Listen` arriving mid-poll drops the
  handoff thread's result on the floor (the WebSocket call itself
  isn't cancelled server-side, but the client stops waiting on it --
  same "abort promptly, no flush required" contract
  `cancel_streaming_to_idle` uses on the voxtype side of this exact
  problem) and transitions straight to `Listening`.
- Either way, barge-in discards whatever was in flight rather than
  queuing it -- if you talk over Jarvis, you meant to redirect it, not
  queue a second reply behind the first.

### Spoken-response strategy: drop the TL;DR hack

Replace `TLDR_INSTRUCTION`'s "write a TL;DR line, then the full
response" ask with a length-based split instead of a
format-and-parse-back one:

- The system/handoff prompt asks OpenClaw to **respond
  conversationally, the way it would speak the answer out loud** --
  not "write a report and also give me a one-liner." This is the
  actual "Jarvis" register: OpenClaw *is* the voice, not a document
  generator being summarized after the fact.
- If the reply comes back short (a rough length/sentence-count
  threshold -- tune once live, start around ~2-3 sentences / ~40
  words), speak it verbatim; it's already conversational.
- If it's long (code, multi-paragraph explanation, a real report),
  fall back to exactly what `summarize_for_speech` already does today
  (the local-LLM condensing call) -- that mechanism is fine, it's only
  the *primary* path (parsing a marker line out of every reply) that's
  being replaced.
- `full_response` (shown in the panel) is always OpenClaw's complete,
  unedited reply either way -- no more stripping a TL;DR line out of
  the displayed body (`split_tldr`'s body-reconstruction logic goes
  away entirely, along with the markdown-tolerant marker parsing --
  all of it was only needed to peel the hack back off).

Net effect: one fewer moving part (`split_tldr` deleted), no
instruction that has to survive markdown/formatting drift, and OpenClaw
is asked for the thing that's actually wanted instead of a document to
post-process.

### The panel becomes a log, not a control surface

With Record and Confirm/Send both gone from the critical path, the
panel's job shrinks to: show what's happening, let you glance back at
a past turn, and offer non-voice fallbacks (typing, Stop) for when
voice genuinely isn't the right tool (open-plan office, a meeting).
Concretely:

- Header: title + Stop (unchanged).
- Status row: same pulsing-dot + phase label, but the label set drops
  "Confirming" and adds nothing new state-wise -- see
  [State machine](#state-machine-changes).
- No more Record / Stop Listening buttons in the header -- the talk
  key replaces both. (Keep them as a fallback for anyone without the
  keybind set up, greyed into a secondary "..." affordance rather than
  primary chrome? -- **flagged as an open question**, see below.)
- Transcript list: unchanged chat-bubble layout: turns append as they
  complete, streaming footer while OpenClaw is composing.
- Bottom bar: just the always-present typed chat box now (no
  conditional confirm box swapping in above it) -- one input surface,
  always the same one.

### Audio cues

Three short, distinct chimes (a few hundred ms each, synthesized once
and cached as static WAV assets rather than round-tripping through
Kokoro every time):
- **Listen-start** -- on entering `Listening` (talk key pressed).
  Confirms the mic is live without needing to look at anything --
  this is the walkie-talkie "go ahead" beep.
- **Sent** -- the instant a transcript is confirmed non-empty and
  handed to `Thinking`. Tells you it heard something and is now
  working, distinct from silence meaning "did that even register?"
- **Reply-ready** -- a soft cue right as `Speaking` starts, so if
  you've looked away from the screen entirely you know a reply landed
  even before the first synthesized word plays (there's already a
  measured ~0.7s time-to-first-audio gap per `tts/mod.rs`'s doc
  comment -- a cue fills that gap rather than leaving it silent).

No cue on `Thinking` itself (silence during thinking is fine/expected)
and no cue on conversation end (the panel disappearing, or the
idle-timeout log line, is enough).

### Idle auto-end

`converse::run`'s `wait_for_listen_or_stop` currently blocks on
`rx.recv()` with no timeout at all. Change to `rx.recv_timeout(IDLE_TIMEOUT)`
(a new `ConverseConfig` field, default something like 3-5 minutes --
tune live) and treat a timeout exactly like `WaitOutcome::Stop`: end
the conversation, write `active: false`, return -- which unblocks
`run_detect`'s `for samples in rx` loop and restores plain wake-word
listening. Optionally speak a short "still there? going idle" cue (or
just the existing chime set) a few seconds before the timeout fires,
so it's not a silent rug-pull -- **flagged as an open question** on
whether that's wanted or overkill.

This only needs to guard the *idle* wait (waiting for the next talk
key press) -- `wait_for_review` is being removed entirely (see
[Auto-send](#auto-send-no-confirm-gate)), and `Thinking`/`Speaking`
already have their own natural end conditions (the handoff returning,
playback finishing) plus the new barge-in exit.

## State machine changes

| Phase | Today | Redesign |
|---|---|---|
| (idle, no phase) | Wait forever for `Listen`/`SendText`/`Stop` | Wait up to `idle_timeout` for `Listen`/`SendText`/`Stop`/barge-in-N/A (nothing to barge in on); timeout ⇒ end conversation |
| `Listening` | Wait forever for transcript or `StopListening`/`Stop` | Unchanged, plus: talk-key-while-listening (toggle variant) ends it early same as `StopListening` |
| `Confirming` | **Removed.** Blocking review gate, no timeout | *(deleted)* |
| `Thinking` | Poll handoff channel + tick elapsed seconds | Same, **plus** watch `control_rx` concurrently; `Listen` ⇒ abandon handoff result, go to `Listening` |
| `Speaking` | Blocking `tts::speak()` call | Runs on a background thread; loop watches `control_rx` concurrently; `Listen` ⇒ kill playback, go to `Listening` |

`ConversationPhase::Confirming` and every piece of state that only
existed to support it (`pending_text`'s "awaiting confirmation"
meaning, `ReviewOutcome`, `wait_for_review`) go away. `pending_text`
either gets deleted outright or repurposed as "last transcript heard"
for the toast-style display mentioned above (implementation detail,
not a design fork).

## Data model changes

`src/conversation/mod.rs`:
- `ConversationPhase`: drop `Confirming`.
- `ConversationState`: drop `pending_text` (or repurpose as
  `last_heard: Option<String>`, informational only, not gating
  anything -- naming/keeping it is an implementation call).
- No new fields needed for barge-in itself (it's a control-flow
  change, not new state), but `ConverseConfig` gains `idle_timeout:
  Duration` (or `_secs: u64` to match the rest of this crate's config
  style).

`src/conversation/mod.rs`'s `ConversationAction`:
- `Confirm`/`Reject` variants: **removed** (no more review step to
  answer).
- `Listen`/`StopListening`/`SendText`/`Stop`: unchanged in shape, but
  `Listen` now does double duty as "start listening" *and*, implicitly
  via the phase it's received in, "barge in and start listening."

`src/tts/mod.rs`:
- `speak()` needs to stop blocking to completion unconditionally.
  Smallest change: keep it blocking as the default entry point for
  every *other* caller (there may not be any today, but keep the
  simple API available), and add a cancellable variant --
  e.g. `speak_cancellable(text, cfg, cancel: &AtomicBool) -> Result<()>`
  that checks `cancel` between sentences and, when set, kills the
  current `paplay` child immediately (needs `play()` to return the
  `Child` or take a kill-signal rather than `wait()`-ing unconditionally)
  instead of proceeding to the next sentence. `converse.rs` is the only
  caller that needs the cancellable form.

## Decisions made

- **Auto-send by default**: yes -- send the moment voxtype auto-stops,
  no confirm gate. See [Auto-send](#auto-send-no-confirm-gate).
- **Talk-key shape**: not a single hardcoded choice -- mirrors
  voxtype's own `[hotkey] mode` naming (`push_to_talk`/`toggle`), but
  as implemented it's purely a choice of *which Hyprland bind recipe*
  to set up, not a config value the daemon reads: `push_to_talk` binds
  the existing `converse listen`/`converse stop-listening` to a key's
  press/release directly (Hyprland's `bind`/`bindr`, no new code
  needed); `toggle` uses the one new CLI entry point,
  `omarchy-novad converse talk` (`conversation::talk`), which checks
  the daemon's own current phase and dispatches to `listen` or
  `stop-listening` accordingly. No `[converse]` config field was
  added for this in the end -- nothing in the code would ever branch
  on it (the daemon doesn't need to know which bind shape the user
  picked), so it would have been unread config, the exact kind of
  thing worth avoiding. Both recipes are in README's "Talk-key bind"
  section. See [Trigger](#trigger-a-talk-key-not-click-to-record) for
  the original reasoning (still correct on the *why*, just not the
  config-field detail).
- **Wake-word re-entry**: punted, stays a non-goal of this pass (see
  [Non-goals](#non-goals-this-pass)) -- revisit as its own project if
  the talk key still feels like friction once this ships.

## Open questions

Settled during implementation:

1. **Record/Stop-Listening buttons**: kept in the panel as-is (not
   demoted to a secondary affordance as originally leaned toward --
   they're still primary header buttons). Fine as a fallback for
   anyone without a keybind configured; revisit the demotion once
   there's a real keybind-vs-panel usage split to design around.
2. **Idle-timeout value**: 240s (4 minutes) default
   (`converse::DEFAULT_IDLE_TIMEOUT_SECS`), overridable via
   `converse start --idle-timeout-secs`. No pre-timeout audio warning
   -- tune both live if 4 minutes turns out wrong in practice.

## Rough implementation phasing

Not a hard sequencing requirement, but a sane order if this gets built
incrementally rather than as one large change:

1. Delete `Confirming`/auto-send (the behavior change with the most
   value and the least new mechanism -- no threading changes needed).
2. Drop the TL;DR hack for the length-based split (self-contained,
   `converse.rs` + prompt text only).
3. Idle auto-end (small, isolated -- one `recv_timeout` change).
4. Barge-in on `Thinking` (control_rx watched alongside the existing
   channel poll -- no `tts` changes needed yet).
5. Barge-in on `Speaking` + the `tts` cancellable rework (the most
   invasive single piece -- do it last once everything else is
   settled).
6. Audio cues (purely additive, can land anywhere, good candidate for
   first *or* last since it never blocks on anything else here).
7. Panel QML updates to match (drop the confirm box, drop/demote the
   Record buttons) -- do this alongside whichever backend step first
   removes the phase it was rendering, not as a separate pass, so the
   UI is never mid-flight out of sync with the daemon's actual phase
   set.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
