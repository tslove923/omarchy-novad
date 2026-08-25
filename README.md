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
volume/brightness, MPRIS media control, sandboxed terminal commands),
and OpenClaw handoff for anything else. Not yet a real Omarchy shell
plugin — see [Roadmap](#roadmap).

## Architecture

| Component | What it does |
|---|---|
| `omarchy-novad detect` | Listens for a wake word via a 3-stage openWakeWord ONNX pipeline on the NPU, then runs the pipeline below on each detection |
| `src/pipeline.rs` | One session per detection: start voxtype recording → wait for it to auto-stop and transcribe → classify → route → drive the popup |
| `src/classify/` | Intent classification against an OpenAI-compatible endpoint (`omarchy-novad serve`, see below) |
| `src/router/` | Local handlers for the intents that don't need real reasoning: `app_launcher`, `web`, `system_control`, `media_control`, `terminal` (sandboxed, confirm-gated), `openclaw` (external handoff) |
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
  `--features openvino`, on `PATH` as `voxtype`. This project
  currently targets a personal fork
  ([tslove923/voxtype](https://github.com/tslove923/voxtype),
  `feature/streaming-openvino` branch) with a few fixes ahead of
  upstream — external-trigger silence auto-stop, an OpenVINO Whisper
  patch, and an experimental sliding-window streaming engine. Point
  the systemd unit / `voxtype daemon` invocation at that build.
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

## Known classifier gaps

The intent classifier (a small local model, see [Setup](#setup)) is
tuned but not perfect. Two known residual misclassifications, both
currently harmless since neither target intent has a local handler
(`HOME_ASSISTANT` and `MEMORY_RETURN` both fall through to plain
dictation either way):

- "turn on the living room lights" → classified `SYSTEM_CONTROL`
  instead of `HOME_ASSISTANT`
- "what did I tell you to remember" → classified `HOME_ASSISTANT`
  instead of `MEMORY_RETURN`

If `HOME_ASSISTANT`/`MEMORY_RETURN` ever get real handlers, revisit
the system prompt in `src/classify/mod.rs`.

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
- **Config menu.** No persistent config file yet — everything is CLI
  flags. A Quickshell-based settings UI (wake word, classify model,
  OpenClaw on/off) is a natural pairing with the plugin work above.
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
