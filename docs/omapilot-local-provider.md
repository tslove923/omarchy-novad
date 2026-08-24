# Local providers for OmaPilot

Two ways to give OmaPilot a fully local/offline model, picked by GPU vendor
— OpenVINO's GPU plugin is Intel-only, so it's the wrong tool on AMD/NVIDIA
hardware even though it's the *right* one on Intel:

| Hardware | Use | Why |
|---|---|---|
| Intel iGPU / NPU | `novad serve` (this repo, OpenVINO GenAI) | OpenVINO is Intel's own stack — it's what gets real perf out of Intel SoCs. Ollama has no OpenVINO backend, so it can't reach the NPU or use the iGPU efficiently on Intel. |
| AMD / NVIDIA GPU | Ollama (ROCm / CUDA) | OpenVINO's GPU plugin doesn't run on AMD/NVIDIA hardware at all. Ollama's ROCm/CUDA backends are the correct local option there — this is nova-npu's own original fallback design (`src/nova/ai/ollama_backend.py`: "When no NPU/OpenVINO hardware is available (e.g. AMD GPU desktop), Ollama... can serve as the model backend").

Both register in OmaPilot's `models.json` the same way — an
`openai-completions` endpoint with a `baseUrl` — since both speak (or, for
`novad serve`, are built to speak) the OpenAI Chat Completions API.

## Option A — Intel: `novad serve`

Runs a local OpenVINO GenAI model on the iGPU or NPU, exposed as an
OpenAI-compatible HTTP server (`src/serve/mod.rs`).

```bash
# 1. Download a model, e.g. the largest that comfortably fits 32GB of
#    shared system RAM at INT4 (see novad roadmap for the sizing math):
#    OpenVINO/Qwen3-Coder-30B-A3B-Instruct-int4-ov (MoE, ~3B active/token)
#
# 2. Serve it:
novad serve --model ~/.local/share/novad/llm-models/qwen3-coder-30b-a3b \
            --device GPU --model-id qwen3-coder-30b-a3b --port 8420
```

Add to `${XDG_CONFIG_HOME:-$HOME/.config}/omapilot/models.json`:

```json
{
  "providers": {
    "novad": {
      "baseUrl": "http://127.0.0.1:8420/v1",
      "api": "openai-completions",
      "apiKey": "local-only-placeholder",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        {
          "id": "qwen3-coder-30b-a3b",
          "name": "Qwen3 Coder 30B-A3B (novad, GPU)",
          "contextWindow": 32768,
          "maxTokens": 8192
        }
      ]
    }
  }
}
```

Then select **novad / Qwen3 Coder 30B-A3B** in OmaPilot's model picker
(Settings → Built-in (OmaPilot) → model).

**Larger hardware** (more system RAM — e.g. Spencer's PTL machine): the same
`novad serve` command works with
[DeepSeek-R1-Distill-Llama-70B-int4-ov](https://huggingface.co/Morteza89/DeepSeek-R1-Distill-Llama-70B-int4-ov)
instead — ~40GB of weights alone, so it needs real headroom beyond 32GB
total RAM, but no code changes; just point `--model` at it and give it a
distinct `model_id`/port if running alongside a smaller model.

Status: server code is written and compiles clean; not yet verified against
a running model on this machine (Qwen3-Coder-30B-A3B is mid-download as of
this writing — 16.3GB total).

## Option B — AMD/NVIDIA: Ollama

```bash
# 1. Install and start Ollama: https://ollama.ai
# 2. Pull a chat-capable model. qwen3:8b is nova's own top recommendation
#    (src/nova/ai/ollama_backend.py RECOMMENDED_CHAT_MODELS).
ollama pull qwen3:8b
```

```json
{
  "providers": {
    "ollama": {
      "baseUrl": "http://127.0.0.1:11434/v1",
      "api": "openai-completions",
      "apiKey": "local-only-placeholder",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        {
          "id": "qwen3:8b",
          "name": "Qwen3 8B (Ollama, local)",
          "contextWindow": 32768,
          "maxTokens": 8192
        }
      ]
    }
  }
}
```

Ollama already speaks OpenAI's `/v1/chat/completions` natively — no glue
code, just this config.

Other models nova recommended (same file), pull first with `ollama pull
<name>`:

**Chat / agentic role**: `qwen2.5:7b`, `llama3.1:8b`, `llama3.2:3b`,
`mistral:7b`, `phi4-mini`, `deepseek-r1:8b`, `gemma2:9b`

**Lighter classifier-only role**: `qwen2.5:1.5b`, `qwen2.5:3b`, `phi4-mini`,
`phi3.5:latest`, `llama3.2:1b`, `llama3.2:3b`

## Status

Both providers can be registered at once — OmaPilot's model picker lets a
user pick per-conversation. Neither has been verified against a running
OmaPilot instance yet (OmaPilot is very new, created 2026-08-12). Verify
the `models.json` schema against OmaPilot's own `docs/native-harness.md`
before relying on this if its config format has moved since.
