# omarchy-novad

NPU-accelerated wake-word voice assistant for Omarchy. Rust port of
[nova-npu](https://github.com/tslove923/nova-npu) (MIT), hardcoded for
this desktop environment rather than kept general — hence the name.

```
"hey jarvis" → voxtype records → auto-stops on silence → transcribes
  → classifies intent (local model) → routes locally, or hands off
  to OpenClaw for anything needing real reasoning → Quickshell popup
  shows the whole thing, themed from your active Omarchy colors
```

## Status

Working end to end on real hardware (Intel NPU + iGPU): wake-word
detection, dictation via [voxtype](https://github.com/peteonrails/voxtype)
(a personal fork — see [Requirements](#requirements)), local intent
classification, a scoped command router (app launch, web search/open,
volume/brightness, MPRIS media control, Home Assistant, BlueBubbles
messaging, sandboxed terminal commands), and OpenClaw handoff for
anything else. Not yet a real Omarchy shell plugin — see
[Roadmap](#roadmap).

## Architecture

| Component | What it does |
|---|---|
| `omarchy-novad detect` | Listens for a wake word via a 3-stage openWakeWord ONNX pipeline on the NPU, then runs the pipeline below on each detection |
| `src/pipeline.rs` | One session per detection: start voxtype recording → wait for it to auto-stop and transcribe → classify → route → drive the popup |
| `src/classify/` | Intent classification against an OpenAI-compatible endpoint (`omarchy-novad serve`, see below) |
| `src/router/` | Local handlers for the intents that don't need real reasoning: `app_launcher`, `web`, `system_control`, `media_control`, `home_assistant` (see [Home Assistant setup](#home-assistant-setup)), `bluebubbles` (see [BlueBubbles setup](#bluebubbles-setup)), `terminal` (sandboxed, confirm-gated), `openclaw` (external handoff) |
| `src/config.rs` | Persistent config file (`~/.config/omarchy-novad/config.toml`) — see [Configuration](#configuration) |
| `src/popup/` | Popup state machine + Unix-socket control channel; also where the confirm/approve round-trip lives (`PopupPhase::Confirming`, `PopupAction::Approve`/`Deny`) for anything that shouldn't execute straight from a voice command — see [Confirmation flow](#confirmation-flow) |
| `omarchy-novad serve` | Hosts a local OpenVINO GenAI model behind an OpenAI-compatible `/v1/chat/completions` API — this is what `classify` talks to |
| `quickshell/` | The popup UI (Quickshell/QML), themed live from `~/.local/state/omarchy/current/theme/colors.toml` |

Two triggers are supported for what a detected wake word does
(`--on-detect`):

- **`voxtype`** (the default): the standalone pipeline above.
- **`omapilot`**: hands the detection straight to
  [OmaPilot](https://github.com/spencerbull/omapilot)'s own
  `voiceToggle` instead — OmaPilot owns the rest of the flow. Useful
  if you'd rather have OmaPilot's own assistant conversation than this
  project's local-classify-then-route pipeline. Anything else passed
  to `--on-detect` runs as a literal shell command.

## Requirements

- **Intel NPU/iGPU** with OpenVINO GenAI installed and on
  `LD_LIBRARY_PATH` (this crate builds with `runtime-linking`, so the
  OpenVINO shared libraries are dlopen'd at startup, not linked at
  build time). This project was built against the OpenVINO GenAI SDK
  install at `~/.local/share/openvino-genai-sdk/`.
- **[voxtype](https://github.com/peteonrails/voxtype)**, built with
  `--features openvino-whisper`, on `PATH` as `voxtype`. This project
  currently targets a personal fork/branch with a few fixes ahead of
  upstream — external-trigger silence auto-stop, and a sliding-window
  streaming engine (rebuilt on top of
  [#547](https://github.com/peteonrails/voxtype/pull/547)'s OpenVINO
  backend rather than duplicating one) with an experimental
  `streaming_revision_mode` (types immediately, corrects via backspace
  if wrong, instead of waiting for two ticks to agree — see that PR's
  branch and voxtype's own `docs/CONFIGURATION.md` for the current
  state; growing-mode revision has a known live bug being tracked
  there, sliding mode is solid). Point the systemd unit / `voxtype
  daemon` invocation at that build.
- **`uv`** (for `omarchy-novad setup wake-model`, which fetches
  openWakeWord + ONNX into a throwaway env to convert a wake-word
  model — nothing installs persistently).
- **[OpenClaw](https://openclaw.ai)** (optional — only needed for the
  `EXTERNAL`/`CODING` handoff; everything else works without it). See
  [OpenClaw setup](#openclaw-setup) below.
- An Omarchy desktop (Quickshell popup, theme integration,
  `omarchy-shell` IPC for the `omapilot` trigger).

## Setup

```bash
cargo build --release

# Put the built binary on PATH -- every command below assumes
# `omarchy-novad` resolves bare, and the popup's own Approve/Deny/
# Insert/Cancel buttons *require* it: they run `omarchy-novad respond
# <action>` via Quickshell.Io.Process, which execs directly (no shell,
# no `./target/release/...` shortcut) and silently no-ops if the binary
# isn't found on the *qs process's own* PATH -- clicking a button just
# does nothing, with only `qs`'s own log (`~/.local/state/quickshell/
# by-pid/<pid>/log.qslog`, or wherever `qs` printed "Saving logs to")
# ever showing "Process failed to start". `~/Work/<project>/bin/` is
# usually already on PATH (see the `openclaw` symlink further down for
# the same pattern) -- if yours isn't, use any other PATH directory.
mkdir -p bin && ln -sf "$(pwd)/target/release/omarchy-novad" bin/omarchy-novad

# One-time: convert a wake-word phrase to NPU-runnable ONNX
omarchy-novad setup wake-model hey_jarvis

# Download a small local model for intent classification (any
# OpenVINO IR chat model works; this was tuned against
# OpenVINO/Qwen3-1.7B-int4-ov — small enough to classify in well
# under a second, large enough to disambiguate intents reliably)
uv run --with huggingface_hub python3 -c "
from huggingface_hub import snapshot_download
snapshot_download(repo_id='OpenVINO/Qwen3-1.7B-int4-ov',
                   local_dir='~/.local/share/omarchy-novad/llm-models/qwen3-1.7b-instruct')
"

# Terminal 1: serve the classifier
LD_LIBRARY_PATH=~/.local/share/openvino-genai-sdk/<version>/runtime/lib/intel64 \
  omarchy-novad serve \
    --model ~/.local/share/omarchy-novad/llm-models/qwen3-1.7b-instruct \
    --device GPU --model-id qwen3-1.7b-instruct --port 8420

# Terminal 2: listen for the wake word
omarchy-novad detect --wakeword hey_jarvis --device NPU

# Terminal 3 (optional but expected by the pipeline): the popup
qs -p quickshell
```

`omarchy-novad detect --help` covers every flag (`--threshold`,
`--patience`, `--classify-base-url`, `--classify-model-id`,
`--on-detect`).

### Data locations

| Path | What |
|---|---|
| `~/.local/share/omarchy-novad/wake-models/` | Converted openWakeWord ONNX models |
| `~/.local/share/omarchy-novad/llm-models/` | Downloaded OpenVINO IR chat models |
| `~/.cache/omarchy-novad/wake-cache/` | Compiled NPU device blobs (wake models) |
| `~/.cache/omarchy-novad/llm-cache/` | Compiled device blobs (`omarchy-novad serve`'s model) |
| `$XDG_RUNTIME_DIR/omarchy-novad/` | Popup state file + control socket (ephemeral) |

## Configuration

`~/.config/omarchy-novad/config.toml` (mode 600 — it holds real
tokens/passwords). Every CLI flag still works and wins when passed;
the file only needs to hold what you want to override, or (for
`[home_assistant]`/`[bluebubbles]`) can't safely pass as a flag at all
(shell history and `/proc/<pid>/cmdline` are both readable by every
process running as your user). Missing file, or a missing section
within it, behaves exactly like before this file existed.

```toml
[detect]
# wakeword = "hey_jarvis"
# device = "NPU"
# classify_base_url = "http://127.0.0.1:8420"
# classify_model_id = "qwen3-1.7b-instruct"

[serve]
# device = "GPU"
# model_id = "novad-local"
# port = 8420

[home_assistant]
url = "https://ha.example.com"
token = "..."
# allowed_entities = ["light.living_room", "lock.front_door"]

[bluebubbles]
server_url = "https://your-bluebubbles-server"
password = "..."
[bluebubbles.contacts]
# mom = "iMessage;-;+15551234567"

[omapilot]
# fallback = false
# direct_target = false
# direct_target_prefix = "pilot"
# plugin_id = "io.github.spencerbull.omapilot"
```

See [Home Assistant setup](#home-assistant-setup),
[BlueBubbles setup](#bluebubbles-setup), and
[OmaPilot integration](#omapilot-integration) below for what each
section actually needs.

## Home Assistant setup

`HOME_ASSISTANT` intent — "turn on the living room lights", "lock the
front door", "set the thermostat to 68", "is the garage door closed"
— talks to Home Assistant's plain REST API directly (`src/router/
home_assistant.rs`), not through the `hass` Omarchy shell plugin
already on this system: that plugin's bridge process is spawned by
its own Service.qml as a child process communicating over that
process's own stdin/stdout, with no socket/port reachable from
outside the shell to plug into.

1. In Home Assistant: profile → Security → Long-Lived Access Tokens →
   create one.
2. Add `[home_assistant]` to `config.toml` (see
   [Configuration](#configuration)) with your instance's `url` and
   that `token`.
3. Optional: `allowed_entities` restricts voice control to specific
   entity IDs. Omitted/empty means every entity HA reports is
   controllable.

Entity names are fuzzy-matched (exact friendly name → substring →
word-overlap, preserving location qualifiers like "upstairs"/"garage"
so they don't cross-match) — spoken names don't need to match HA's
friendly names exactly. Compound commands work too ("turn on
downstairs and turn off upstairs lights").

## BlueBubbles setup

`MESSAGE` intent — "text mom I'm running late", "send a message to
sarah saying I'll be there soon" — sends iMessages through a
self-hosted [BlueBubbles](https://bluebubbles.app) server (a Mac
running the BlueBubbles Server app, exposing a local REST API).
Started as a port of a working `~/.agents/skills/bluebubbles` OmaPilot
skill; `src/router/bluebubbles.rs` reimplements its verified send call
in Rust and adds real contact resolution on top.

1. In the BlueBubbles Server app on the Mac: note the server URL and
   password from its own settings.
2. Add `[bluebubbles]` to `config.toml` with `server_url` and
   `password`.
3. That's it for anyone already in that Mac's Contacts app — "text
   `<name>`" resolves live against `GET /api/v1/contact`, fuzzy-matched
   on full name then first name (erroring with the candidate list if
   ambiguous, e.g. two different Sarahs, rather than guessing), then
   either sends into an existing 1:1 thread or starts a brand-new
   conversation if none exists yet. Verified live that modern macOS
   (Big Sur+) requires the first message and the address together to
   create a chat at all — there's no separate "start an empty
   conversation" step, so this is one atomic action either way.
4. `[bluebubbles.contacts]` is a manual name → chat-GUID override,
   only needed for an alias ("mom" for someone Contacts has under
   their legal name) or someone not in Contacts at all.

`method: "private-api"` (used for every send) requires the BlueBubbles
Private API helper installed on the Mac — see BlueBubbles' own docs if
sends fail with a private-API-related error.

## Confirmation flow

Some actions need a human in the loop before they run — currently a
`TERMINAL` command that isn't in the safe-readonly allowlist
(`terminal::is_safe_readonly`), and every `MESSAGE` (texting a real
person is a much higher-stakes, harder-to-undo action than toggling a
light, so it never sends straight from the classifier's output the
way `HOME_ASSISTANT` does).

`router::route` returns `RouteResult::NeedsConfirmation { preview,
kind }` for these; `pipeline.rs` shows the popup in
`PopupPhase::Confirming` with `preview` as the text (e.g. `Text Sarah:
"running late, be there in 10"`, or `... (new conversation)` when
BlueBubbles is about to start a fresh thread), waits for an
Approve/Deny click, and only then calls `router::run_confirmed(kind,
...)` to actually execute. `ConfirmKind` is what makes this
extensible — a future confirmable intent just adds a variant and an
arm in `run_confirmed`, the popup/pipeline plumbing doesn't change.

## OpenClaw setup

`EXTERNAL` and `CODING` intents (anything needing real reasoning,
writing, or code) hand off to [OpenClaw](https://openclaw.ai) via a
small CLI bridge script rather than talking to a gateway API directly
— see `src/router/openclaw.rs`.

### 1. Install the OpenClaw CLI

```bash
# Follow OpenClaw's own install docs, or if it's already installed
# under ~/.openclaw/bin/openclaw and not on PATH:
ln -sf ~/.openclaw/bin/openclaw ~/Work/bin/openclaw   # or any PATH dir
openclaw --version
```

### 2. Point it at a gateway

`openclaw-handoff` (below) needs `OPENCLAW_GATEWAY_URL` and
`OPENCLAW_GATEWAY_TOKEN`, sourced from
`~/.config/openclaw-novad.env` (chmod 600 — this holds a bearer
token, keep it out of the repo and out of novad's own process
environment; the wrapper script sources it, novad never sees it):

```bash
mkdir -p ~/.config
cat > ~/.config/openclaw-novad.env <<'EOF'
OPENCLAW_GATEWAY_URL=wss://your-gateway-host
OPENCLAW_GATEWAY_TOKEN=<your gateway device token>
EOF
chmod 600 ~/.config/openclaw-novad.env
```

**Remote gateway** (a cluster/server running OpenClaw, reached over
the network — this project's own setup uses a Kubernetes-hosted
gateway): `OPENCLAW_GATEWAY_URL` is the gateway's WebSocket URL. No
local OpenClaw server process needed on this machine — the CLI is
just a client.

**Local gateway** (OpenClaw running on this machine): point
`OPENCLAW_GATEWAY_URL` at `ws://127.0.0.1:<port>` instead (OpenClaw's
own docs cover starting a local gateway). Same CLI, same bridge
script either way.

If the gateway is unreachable at handoff time, `openclaw-handoff`
falls back to a local embedded agent with no provider auth — it
errors out with a clear message rather than mis-answering, and the
popup shows that error instead of an answer. So the voice flow
degrades gracefully on a gateway outage; it just won't lie to you.

### 3. Pair this device

The gateway rejects any CLI device until an operator approves it —
this is a one-time step per machine (or per OpenClaw profile), not
per session:

```bash
# Run any command that talks to the gateway once (e.g. openclaw devices list)
# — this creates a pending pairing request.
openclaw devices list

# On the gateway side (e.g. exec into the OpenClaw pod/container):
openclaw devices approve <requestId>   # requests expire after 5 minutes
```

Approve with the role/scope you actually want this device to have —
by default `openclaw devices approve` grants full operator scope; a
narrower scope can be set per device if you'd rather novad's handoffs
run with reduced privileges.

### 4. Install the bridge script

`scripts/openclaw-handoff` (in this repo) is what
`src/router/openclaw.rs` shells out to — install it on `PATH`:

```bash
install -Dm755 scripts/openclaw-handoff ~/.local/bin/openclaw-handoff
```

It runs `openclaw agent --agent main --session-key
agent:main:novad:<conversation> --message <utterance> --json`,
sanitizes the conversation-id segment, and prints the reply's text on
stdout (or a fallback line on stderr and a non-zero exit on failure).
All wake-word-triggered handoffs currently share one conversation id
(`"voice"`, see `CONVERSATION_ID` in `openclaw.rs`) so OpenClaw keeps
context turn to turn.

### Verify

```bash
openclaw-handoff "reply with the single word PING and nothing else" "smoke-test"
# should print PING within ~10s
```

Then say the wake word and something open-ended ("what's the capital
of France", "write a python script to sort a list") — the popup
should show "Asking OpenClaw…" and come back with a real answer.

## OmaPilot integration

Two independent, optional roles OmaPilot can fill, both off by default
— set either or both under `[omapilot]` (see [Configuration](#configuration)):

- **`fallback = true`** — when `EXTERNAL`/`CODING` need a handoff (see
  [OpenClaw setup](#openclaw-setup)) and OpenClaw is unavailable (not
  installed, gateway down, timed out) or unconfigured, try OmaPilot
  next instead of giving up.
- **`direct_target = true`** — recognize `direct_target_prefix`
  (default `"pilot"`) at the start of any wake-word utterance and
  route straight to OmaPilot, bypassing classification entirely. "hey
  jarvis, pilot: what's the capital of France" hands OmaPilot exactly
  "what's the capital of France", not the classifier's (often
  keyword-stripped) extraction.

Both go through the same mechanism (`router::omapilot::ask`, via
`omarchy-shell <plugin_id> askText <text>`), which behaves nothing
like OpenClaw's handoff: OpenClaw's CLI bridge is a real request/reply
round-trip and returns the agent's answer as text for the popup to
show. OmaPilot's `askText` is fire-and-forget — it opens OmaPilot's
own panel and starts it answering there, and the answer streams into
OmaPilot's UI directly, never back through omarchy-novad. So the popup
can only show "Handed off to OmaPilot", never the actual answer.

**`askText` is not part of OmaPilot's upstream API.** It's a local
patch on top of the plugin's own git clone
(`~/.config/omarchy/plugins/io.github.spencerbull.omapilot`, branch
`patch/omapilot-asktext-ipc`, not merged into that clone's `main` and
not upstreamed to `spencerbull/omarchy-omapilot`). Without the patch
applied and checked out, every `askText` call fails with "Function not
found" and both `fallback` and `direct_target` degrade to "OmaPilot
isn't available right now" — `fallback` still works via OpenClaw
alone, `direct_target` just never fires. See that commit's message in
the plugin's own repo for what the patch adds and why.

**Before enabling either flag**, know that OmaPilot's configured
provider can be a real tool-capable agent (its "pi" harness), and if
OmaPilot's own `configuredDangerousAutoApprove` setting is on,
`askText` hands it a transcript that gets acted on without a human
confirming each tool call. omarchy-novad has no visibility into that
setting and can't override it from here — check OmaPilot's own
settings before turning either of these on if that matters to you.

### Verify

```bash
omarchy-shell io.github.spencerbull.omapilot askText "In one sentence, what's the capital of France?"
# should print "ok" and OmaPilot's panel should open and start answering
```

If it prints `Function not found`, the `askText` patch isn't applied
in the running plugin checkout — see above.

## Known classifier gaps

The intent classifier (a small local model, see [Setup](#setup)) is
tuned but not perfect. Two known residual misclassifications:

- "turn on the living room lights" (and similar — "turn on Sophie's
  room", a device-type-free area/person name) → classified
  `SYSTEM_CONTROL` instead of `HOME_ASSISTANT`. No longer harmless
  now that `HOME_ASSISTANT` has a real handler, but caught by a
  recovery check in `router::route`'s `SYSTEM_CONTROL` arm
  (`home_assistant::looks_like_home_assistant_command`, same pattern
  `media_control::looks_like_media_command` already used for
  transport commands like "pause"/"stop `<player>`") before it ever
  reaches `system_control::run`.
- "what did I tell you to remember" → classified `HOME_ASSISTANT`
  instead of `MEMORY_RETURN`. Still harmless: `MEMORY_RETURN` has no
  local handler, and `HOME_ASSISTANT` correctly finds no matching
  entity for a phrase like this, so it fails closed rather than
  acting on the wrong thing.

If `MEMORY_RETURN` ever gets a real handler, revisit the system
prompt in `src/classify/mod.rs`.

## Roadmap

- **Real Omarchy shell plugin.** Today the popup is a standalone
  `qs -p quickshell` process this daemon spawns itself — not a plugin
  the Omarchy shell discovers and loads via its own plugin registry.
  Getting there means a `manifest.json` (schemaVersion, id, declared
  `kinds`, matching `entryPoints`), restructuring the popup as a
  loadable `overlay` entry point (and possibly a `bar-widget` for an
  idle/listening status indicator), and moving control from this
  daemon's own `qs -p` invocation into the shell's plugin lifecycle.
  Run `omarchy plugin validate <folder>` against whatever's built to
  confirm it actually passes the schema the shell enforces.
- **Config menu.** `config.toml` (see [Configuration](#configuration))
  covers everything now, but only as a text file — a Quickshell-based
  settings UI (wake word, classify model, Home Assistant/BlueBubbles
  credentials, OpenClaw on/off) is a natural pairing with the plugin
  work above.
- **Spotify integration**, ported from nova-npu's
  `integrations/spotify.py` (OAuth + Web API playback control) — MPRIS
  alone covers local players; Spotify's own Web API would add
  search-and-play and remote-device control nova's original had.

## Credits

Ports [nova-npu](https://github.com/tslove923/nova-npu)'s wake-word
pipeline, intent taxonomy, and command routing (MIT) into Rust. Hands
dictation off to [voxtype](https://github.com/peteonrails/voxtype)
(MIT) and reasoning/coding requests off to
[OpenClaw](https://openclaw.ai).
