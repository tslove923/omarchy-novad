# tts-prototype

CPU-only Kokoro TTS (ONNX Runtime) prototype and HTTP server for
omarchy-novad's `converse` feature.

## Setup

```bash
cd tts-prototype
uv sync

# Model weights aren't committed (see .gitignore) -- fetch the v1.0
# release files from the kokoro-onnx project and place them here:
#   https://github.com/thewh1teagle/kokoro-onnx/releases
# (look for the "model-files-v1.0" or similarly named release; grab
# kokoro-v1.0.onnx and voices-v1.0.bin specifically -- the v0.19
# generation was tried and abandoned for this project, see the
# "GPU/NPU acceleration" section of the main README.)
```

## Files

- `kokoro-v1.0.onnx`, `voices-v1.0.bin` -- the model and voice-embedding
  files. Must stay alongside `server.py` (or `stream_demo.py`) since both
  load them by relative path.
- `stream_demo.py` -- standalone script validating sentence-by-sentence
  streaming synthesis + playback (no HTTP involved).
- `server.py` -- the FastAPI/uvicorn HTTP server described below.

## Running the TTS server

`server.py` loads the ONNX model once at startup and serves it over
loopback HTTP. This is a long-running process the Rust daemon calls into
over HTTP (via `ureq`) -- it is **not** spawned or managed by
`omarchy-novad` itself, the same relationship `omarchy-novad serve` (the
local LLM) has: start it yourself, in its own terminal or a systemd user
unit, before running `omarchy-novad converse start`.

```bash
cd tts-prototype
uv run uvicorn server:app --host 127.0.0.1 --port 8421
```

Port and host default to `127.0.0.1:8421` (matching
`TtsConfig::default().serve_url` in the Rust config) and can be
overridden with `--host`/`--port` if you run it via
`uv run python server.py --port 8421` instead, or via the
`TTS_SERVER_HOST` / `TTS_SERVER_PORT` env vars (only consulted for that
module's own argparse defaults, so pass `--host`/`--port` explicitly if
invoking `uvicorn` directly).

Model load takes under a second on this machine; the server logs
`model loaded in <N>s, ready` once `/healthz` and `/synthesize` are
usable.

### Endpoints

- `GET /healthz` -- `{"status": "ok"}` (200) once the model has loaded.
- `POST /synthesize` -- body `{"text": "<string>", "voice": "<string>"}`
  (e.g. `voice: "af_nova"`), response is a raw WAV file
  (`Content-Type: audio/wav`). Treats the whole `text` field as a single
  `kokoro.create()` call -- no server-side sentence splitting (the Rust
  caller already splits sentences before calling this endpoint). Empty/
  whitespace-only `text` or a missing/unknown `voice` returns `400` with
  a JSON `{"error": "..."}` body rather than a 500.

### Performance

Measured on this machine (CPU-only `CPUExecutionProvider`, warm model):
RTF (compute-seconds per audio-second) around 0.28-0.34, so a short
sentence's `/synthesize` round trip (including HTTP overhead) is
typically 0.5-1.1s wall time.

### Voices

Voice ids come from `voices-v1.0.bin`; `af_nova` is the one
`omarchy-novad`'s default config uses (`TtsConfig::default().voice`).
Run `uv run python -c "from kokoro_onnx import Kokoro; import onnxruntime
as ort; s = ort.InferenceSession('kokoro-v1.0.onnx',
providers=['CPUExecutionProvider']); print(sorted(Kokoro.from_session(s,
'voices-v1.0.bin').get_voices()))"` to list them all.
