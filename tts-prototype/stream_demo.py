"""
Prototype: sentence-by-sentence streaming TTS. Synthesizes on a background
thread (CPU, kokoro-v1.0.onnx, af_nova voice) one sentence at a time while
already-ready sentences play on the main thread via paplay -- validates
that a 10-20s response can start speaking in well under 1s, even though
the model's RTF (~0.3) can't synthesize the *whole* response that fast.
"""

import os
import queue
import re
import subprocess
import tempfile
import threading
import time

import onnxruntime as ort
import soundfile as sf
from kokoro_onnx import Kokoro

MODEL = "kokoro-v1.0.onnx"
VOICES = "voices-v1.0.bin"
VOICE = "af_nova"

# A synthetic ~15s-of-audio "OpenClaw response" style paragraph.
RESPONSE_TEXT = (
    "Sure, I can help with that. I went ahead and checked the cluster status, "
    "and everything looks healthy right now. All three nodes are reporting "
    "ready, and the openclaw deployment has one pod running without restarts. "
    "I didn't see any errors in the recent logs, so I think we're good to go."
)


def split_sentences(text):
    parts = re.split(r"(?<=[.!?])\s+", text.strip())
    return [p for p in parts if p]


def synth_worker(kokoro, sentences, out_queue, timings, t0):
    for i, sentence in enumerate(sentences):
        start = time.monotonic() - t0
        audio, sr = kokoro.create(sentence, voice=VOICE, lang="en-us")
        done = time.monotonic() - t0
        timings.append((i, start, done, len(audio) / sr))
        out_queue.put((i, audio, sr))
    out_queue.put(None)


def play_pcm(audio, sr):
    path = tempfile.mktemp(suffix=".wav")
    sf.write(path, audio, sr)
    subprocess.run(["paplay", path], check=True)
    os.remove(path)


def main():
    print("loading model...")
    session = ort.InferenceSession(MODEL, providers=["CPUExecutionProvider"])
    kokoro = Kokoro.from_session(session, VOICES)

    sentences = split_sentences(RESPONSE_TEXT)
    print(f"{len(sentences)} sentences, {len(RESPONSE_TEXT)} chars total\n")

    out_queue = queue.Queue()
    synth_timings = []
    t0 = time.monotonic()

    worker = threading.Thread(
        target=synth_worker, args=(kokoro, sentences, out_queue, synth_timings, t0)
    )
    worker.start()

    first_audio_start = None
    play_events = []
    while True:
        item = out_queue.get()
        if item is None:
            break
        i, audio, sr = item
        play_start = time.monotonic() - t0
        if first_audio_start is None:
            first_audio_start = play_start
        play_pcm(audio, sr)
        play_end = time.monotonic() - t0
        play_events.append((i, play_start, play_end, len(audio) / sr))

    worker.join()

    print("=== synthesis timeline ===")
    for i, start, done, secs in synth_timings:
        print(
            f"  sentence {i}: synth {start:.3f}s -> {done:.3f}s "
            f"(took {done - start:.3f}s for {secs:.2f}s audio, "
            f"RTF {(done - start) / secs:.3f})"
        )

    print("\n=== playback timeline ===")
    prev_end = 0.0
    for i, pstart, pend, secs in play_events:
        gap = pstart - prev_end
        print(
            f"  sentence {i}: playback {pstart:.3f}s -> {pend:.3f}s "
            f"({secs:.2f}s audio), gap-before-playback={gap:.3f}s"
        )
        prev_end = pend

    total_audio = sum(s for *_, s in play_events)
    print(f"\nTIME TO FIRST AUDIO: {first_audio_start:.3f}s")
    print(f"TOTAL WALL TIME: {play_events[-1][2]:.3f}s for {total_audio:.2f}s of audio")


if __name__ == "__main__":
    main()
