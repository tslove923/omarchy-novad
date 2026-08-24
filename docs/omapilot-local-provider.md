# Local (Ollama) provider for OmaPilot

Preserves nova-npu's local-LLM setup (`src/nova/ai/ollama_backend.py`) as an
optional local agentic provider for
[OmaPilot](https://github.com/spencerbull/omarchy-omapilot), alongside its
built-in cloud harnesses (OpenAI/Grok/Codex/OpenCode). Useful for offline use,
privacy, or machines without an Intel NPU.

## Why this shape

Ollama already speaks an OpenAI-compatible `/v1/chat/completions` API out of
the box, and OmaPilot's `models.json` already documents accepting arbitrary
`openai-completions` endpoints. No glue code needed — just configuration.

## Setup

```bash
# 1. Install and start Ollama: https://ollama.ai
# 2. Pull a chat-capable model. qwen3:8b is nova's own top recommendation
#    (src/nova/ai/ollama_backend.py RECOMMENDED_CHAT_MODELS) for the chat/
#    agentic role, as opposed to the smaller classifier-only models below.
ollama pull qwen3:8b
```

Add to `${XDG_CONFIG_HOME:-$HOME/.config}/omapilot/models.json`:

```json
{
  "providers": {
    "local": {
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
          "name": "Qwen3 8B (local)",
          "contextWindow": 32768,
          "maxTokens": 8192
        }
      ]
    }
  }
}
```

Then select **local / Qwen3 8B (local)** in OmaPilot's model picker (Settings
→ Built-in (OmaPilot) → model).

## Other models nova recommended

If `qwen3:8b` is too slow for the hardware, or a different tradeoff is
wanted, nova's own recommended lists (same file) — add any of these as
additional entries under `"models"` above, `ollama pull <name>` first:

**Chat / agentic role** (what this doc sets up by default):
`qwen2.5:7b`, `llama3.1:8b`, `llama3.2:3b`, `mistral:7b`, `phi4-mini`,
`deepseek-r1:8b`, `gemma2:9b`

**Lighter classifier-only role** (if OmaPilot or a future novad-native
fallback path wants a fast, low-context model rather than a full chat
model): `qwen2.5:1.5b`, `qwen2.5:3b`, `phi4-mini`, `phi3.5:latest`,
`llama3.2:1b`, `llama3.2:3b`

## Status

Documentation only — not yet verified against a running OmaPilot instance
(OmaPilot is very new, created 2026-08-12). Verify the `models.json` schema
against OmaPilot's own `docs/native-harness.md` before relying on this if
its config format has moved since.
