"""
Local HTTP server wrapping Kokoro TTS (CPU-only ONNX Runtime inference)
for the omarchy-novad Rust daemon to call over loopback HTTP.

Loads the ONNX model once at process startup (cold load takes real time --
this is meant to run as a long-lived warm process, not spawned per-call)
and exposes kokoro.create() over HTTP. See stream_demo.py for the
validated reference pipeline this reproduces.

Run:
    uv run uvicorn server:app --host 127.0.0.1 --port 8421

Port/host can also be overridden via the TTS_SERVER_HOST / TTS_SERVER_PORT
env vars when running through this module's own __main__ entrypoint:
    uv run python server.py --port 8421
"""

import argparse
import io
import logging
import os
import time
from contextlib import asynccontextmanager

import onnxruntime as ort
import soundfile as sf
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, Response
from kokoro_onnx import Kokoro

MODEL = os.path.join(os.path.dirname(os.path.abspath(__file__)), "kokoro-v1.0.onnx")
VOICES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "voices-v1.0.bin")

DEFAULT_HOST = os.environ.get("TTS_SERVER_HOST", "127.0.0.1")
DEFAULT_PORT = int(os.environ.get("TTS_SERVER_PORT", "8421"))

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [tts-server] %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("tts-server")

# Populated once at startup by the lifespan handler below.
_state: dict = {"kokoro": None, "voices": None}


@asynccontextmanager
async def lifespan(app: FastAPI):
    log.info("loading kokoro model (%s, %s)...", MODEL, VOICES)
    t0 = time.monotonic()
    session = ort.InferenceSession(MODEL, providers=["CPUExecutionProvider"])
    kokoro = Kokoro.from_session(session, VOICES)
    _state["kokoro"] = kokoro
    try:
        _state["voices"] = set(kokoro.get_voices())
    except Exception:
        _state["voices"] = None  # fall back to "let kokoro.create() decide"
    log.info("model loaded in %.2fs, ready", time.monotonic() - t0)
    yield
    _state["kokoro"] = None


app = FastAPI(lifespan=lifespan)


@app.get("/healthz")
async def healthz():
    return {"status": "ok"}


@app.post("/synthesize")
async def synthesize(request: Request):
    try:
        body = await request.json()
    except Exception:
        return JSONResponse(
            status_code=400, content={"error": "request body must be valid JSON"}
        )

    if not isinstance(body, dict):
        return JSONResponse(
            status_code=400, content={"error": "request body must be a JSON object"}
        )

    text = body.get("text")
    voice = body.get("voice")

    if not isinstance(text, str) or not text.strip():
        return JSONResponse(
            status_code=400,
            content={"error": "'text' must be a non-empty, non-whitespace string"},
        )

    if not isinstance(voice, str) or not voice.strip():
        return JSONResponse(
            status_code=400,
            content={"error": "'voice' must be a non-empty string"},
        )

    known_voices = _state["voices"]
    if known_voices is not None and voice not in known_voices:
        return JSONResponse(
            status_code=400,
            content={"error": f"unknown voice {voice!r}"},
        )

    kokoro: Kokoro = _state["kokoro"]
    if kokoro is None:
        return JSONResponse(
            status_code=503, content={"error": "model is not loaded yet"}
        )

    t0 = time.monotonic()
    try:
        audio, sample_rate = kokoro.create(text, voice=voice, lang="en-us")
    except Exception as e:
        log.warning("synthesis failed for voice=%r text=%r: %s", voice, text, e)
        return JSONResponse(
            status_code=400,
            content={"error": f"synthesis failed: {e}"},
        )
    elapsed = time.monotonic() - t0

    buf = io.BytesIO()
    sf.write(buf, audio, sample_rate, format="WAV")
    wav_bytes = buf.getvalue()

    audio_secs = len(audio) / sample_rate
    log.info(
        "synthesized %d chars (voice=%s) -> %.2fs audio in %.3fs (RTF %.3f)",
        len(text),
        voice,
        audio_secs,
        elapsed,
        elapsed / audio_secs if audio_secs > 0 else float("nan"),
    )

    return Response(content=wav_bytes, media_type="audio/wav")


def main():
    import uvicorn

    parser = argparse.ArgumentParser(description="Kokoro TTS HTTP server")
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    args = parser.parse_args()
    uvicorn.run(app, host=args.host, port=args.port)


if __name__ == "__main__":
    main()
