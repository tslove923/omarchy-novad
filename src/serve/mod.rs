//! OpenAI-compatible HTTP server backed by `openvino_genai::LlmPipeline`.
//!
//! Exists so `classify` (see src/classify/mod.rs), OmaPilot, or
//! anything else that speaks the OpenAI Chat Completions API can
//! point at a fully local, GPU-backed model. Also usable as an
//! OmaPilot `models.json` provider by hand (register a
//! `openai-completions` entry pointing at this server's `/v1`) --
//! replaces the earlier Ollama-based plan: Ollama already speaks this
//! API natively, so pointing OmaPilot at our own OpenVINO GenAI
//! pipeline instead means omarchy-novad has to speak it too.
//!
//! Scope: single model, single in-process pipeline. No auth (binds to
//! 127.0.0.1 — same trust boundary as Ollama's own default). Good
//! enough for "OmaPilot/Pi on this machine talks to omarchy-novad on this
//! machine"; revisit if that stops being true.
//!
//! Streaming (`stream: true`) is required, not optional, in practice —
//! testing against a real client (Pi, OmaPilot's own harness) found it
//! doesn't fall back to non-streaming at all; a 400 on stream=true just
//! fails every request. Implemented via SSE (`text/event-stream`,
//! OpenAI's `chat.completion.chunk` shape) — but not truly token-
//! incremental: generation still runs to completion server-side first.
//! Tool calls (see below) need the *complete* response before they can
//! be told apart from plain content, since both come back as the same
//! raw text stream from the model; streaming individual tokens would
//! leak the raw `<tool_call>...` markup to the client as if it were
//! visible assistant text. A real fix would detect the opening tag
//! incrementally and only stream tokens before it — not done here.
//!
//! Tool calling: openvino_genai's `ChatHistory` has native
//! `set_tools()`/`ChatMessage::assistant_with_tool_calls`, but nothing
//! parses the model's *output* back into structured tool calls — that
//! part is entirely on us. Qwen3-Coder (confirmed from its own
//! `chat_template.jinja`, not guessed) uses an XML-flavored format, not
//! the more common Hermes-style JSON-in-tags one:
//! `<tool_call>\n<function=name>\n<parameter=key>\nvalue\n</parameter>\n</function>\n</tool_call>`.
//! `parse_tool_calls` below parses exactly that. A different model
//! would need a different parser — this is not a generic solution.

use std::convert::Infallible;
use std::sync::Mutex;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::stream::{self, Stream, StreamExt};
use openvino_genai::{
    ChatHistory, ChatMessage, GenerationConfig, JsonContainer, LlmPipeline, ToolCall,
};
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::UnboundedReceiverStream;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("pipeline init failed: {0}")]
    Init(String),
    #[error("inference failed: {0}")]
    Infer(String),
}

pub struct ServeConfig {
    pub model_path: std::path::PathBuf,
    pub device: String,
    pub cache_dir: std::path::PathBuf,
    pub model_id: String,
    pub port: u16,
}

struct AppState {
    pipeline: Mutex<LlmPipeline>,
    model_id: String,
}

pub async fn run(config: ServeConfig) -> anyhow::Result<()> {
    let model_path = config
        .model_path
        .to_str()
        .ok_or_else(|| ServeError::Init(format!("non-utf8 model path: {:?}", config.model_path)))?
        .to_string();

    std::fs::create_dir_all(&config.cache_dir)?;
    let cache_dir_str = config.cache_dir.to_string_lossy().to_string();

    tracing::info!("Loading {} on {}...", model_path, config.device);
    let load_start = std::time::Instant::now();
    let pipeline = LlmPipeline::with_properties(
        &model_path,
        &config.device,
        &[("CACHE_DIR", &cache_dir_str)],
    )
    .map_err(|e| ServeError::Init(format!("LlmPipeline::with_properties: {e}")))?;
    tracing::info!("Model loaded in {:.2}s", load_start.elapsed().as_secs_f32());

    let state = std::sync::Arc::new(AppState {
        pipeline: Mutex::new(pipeline),
        model_id: config.model_id.clone(),
    });

    let app = Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/models", get(list_models))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(
        "omarchy-novad serve listening on http://{addr}  (model id: {})",
        config.model_id
    );
    println!(
        "[omarchy-novad] Serving {} at http://{addr}/v1",
        config.model_id
    );
    println!(
        "[omarchy-novad] Point 'omarchy-novad detect --classify-base-url http://{addr}' \
         (or OmaPilot's models.json baseUrl) at http://{addr}/v1"
    );

    axum::serve(listener, app).await?;
    Ok(())
}

// ── OpenAI-compatible request/response shapes ──────────────────────

#[derive(Deserialize)]
struct ChatCompletionRequest {
    #[allow(dead_code)] // echoed back, not used to select a pipeline (single-model server)
    model: Option<String>,
    messages: Vec<ChatMessageIn>,
    #[serde(default = "default_max_tokens")]
    max_tokens: usize,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    stream: bool,
    /// Raw passthrough — openvino_genai's ChatHistory::set_tools takes
    /// exactly this shape (OpenAI's `[{"type":"function","function":{...}}]`
    /// array) already, so there's nothing to translate.
    #[serde(default)]
    tools: Option<serde_json::Value>,
}

fn default_max_tokens() -> usize {
    1024
}

#[derive(Deserialize)]
struct ChatMessageIn {
    role: String,
    #[serde(default)]
    content: Option<MessageContent>,
    /// Present on assistant messages that themselves made tool calls
    /// (Pi re-sends its own prior turn this way when continuing a
    /// conversation after tool results come back).
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallIn>>,
    /// Present on role:"tool" messages — which tool call this result
    /// answers.
    #[serde(default)]
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct ToolCallIn {
    // openvino_genai's ChatMessage::assistant_with_tool_calls has no
    // per-call id slot to round-trip this into — the id only matters
    // for OUR own outgoing tool calls (see ToolCallOut below), not for
    // replaying history back in.
    function: ToolCallFunctionIn,
}

#[derive(Deserialize)]
struct ToolCallFunctionIn {
    name: String,
    /// OpenAI sends this as a JSON-encoded string, not a nested object.
    arguments: String,
}

/// OpenAI clients send `content` as either a plain string or an array
/// of content parts (`[{"type": "text", "text": "..."}, ...]`) — the
/// latter is how real multipart-capable clients speak the API even for
/// plain text (found via Pi: it always sends the array form). We don't
/// support non-text parts (images etc.); anything else is dropped.
#[derive(Deserialize)]
#[serde(untagged)]
enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Deserialize)]
struct ContentPart {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

impl MessageContent {
    fn to_text(&self) -> String {
        match self {
            MessageContent::Text(s) => s.clone(),
            MessageContent::Parts(parts) => parts
                .iter()
                .filter(|p| p.kind == "text")
                .filter_map(|p| p.text.as_deref())
                .collect::<Vec<_>>()
                .join(""),
        }
    }
}

#[derive(Serialize)]
struct ChatCompletionResponse {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<ChatChoice>,
    usage: ChatUsage,
}

#[derive(Serialize)]
struct ChatChoice {
    index: u32,
    message: ChatMessageOut,
    finish_reason: &'static str,
}

#[derive(Serialize)]
struct ChatMessageOut {
    role: &'static str,
    // `null` (via None) when tool_calls is present, matching OpenAI's
    // own convention — clients like Pi branch on this.
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallOut>>,
}

#[derive(Serialize, Clone)]
struct ToolCallOut {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: ToolCallFunctionOut,
}

#[derive(Serialize, Clone)]
struct ToolCallFunctionOut {
    name: String,
    /// JSON-encoded string, per the OpenAI spec — not a nested object.
    arguments: String,
}

#[derive(Serialize)]
struct ChatUsage {
    prompt_tokens: usize,
    completion_tokens: usize,
    total_tokens: usize,
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Serialize)]
struct ErrorDetail {
    message: String,
    #[serde(rename = "type")]
    kind: &'static str,
}

async fn list_models(State(state): State<std::sync::Arc<AppState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "object": "list",
        "data": [{"id": state.model_id, "object": "model", "owned_by": "omarchy-novad"}],
    }))
}

async fn chat_completions(
    State(state): State<std::sync::Arc<AppState>>,
    Json(req): Json<ChatCompletionRequest>,
) -> Response {
    if req.messages.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "messages must not be empty");
    }

    if req.stream {
        return chat_completions_streaming(state, req).await;
    }

    let result = tokio::task::spawn_blocking({
        let state = state.clone();
        move || run_generation(&state, &req)
    })
    .await;

    match result {
        Ok(Ok(body)) => (StatusCode::OK, Json(body)).into_response(),
        Ok(Err(e)) => err_response(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => err_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("generation task panicked: {e}"),
        ),
    }
}

/// Message passed from the blocking generation thread to the async SSE
/// stream. Not truly token-incremental — see the module-level streaming
/// doc comment for why (tool-call correctness).
enum StreamMsg {
    Complete(GenerationResult),
    Error(String),
}

async fn chat_completions_streaming(
    state: std::sync::Arc<AppState>,
    req: ChatCompletionRequest,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamMsg>();

    let id = format!("chatcmpl-novad-{}", now_unix_ms());
    let created = now_unix_ms() / 1000;
    let model_id = state.model_id.clone();

    tokio::task::spawn_blocking(move || {
        let msg = match run_generation_core(&state, &req) {
            Ok(result) => StreamMsg::Complete(result),
            Err(e) => StreamMsg::Error(e.to_string()),
        };
        let _ = tx.send(msg);
    });

    // One generation result becomes a short, fixed sequence of chunks:
    // role announcement, then either a content delta or a tool_calls
    // delta, then the finish chunk. Real incremental typing would need
    // to detect the `<tool_call>` tag as tokens arrive and only stream
    // content up to that point — deferred; see module doc comment.
    let events = UnboundedReceiverStream::new(rx).flat_map(move |msg| {
        let chunks = match msg {
            StreamMsg::Complete(result) => {
                tracing::debug!(
                    prompt_tokens = result.prompt_tokens,
                    completion_tokens = result.completion_tokens,
                    "stream complete"
                );
                let (content, tool_calls) = parse_tool_calls(&result.text);
                let mut chunks = vec![chunk_event(
                    &id,
                    created,
                    &model_id,
                    ChunkDelta {
                        role: Some("assistant"),
                        content: None,
                        tool_calls: None,
                    },
                    None,
                )];
                if tool_calls.is_empty() {
                    chunks.push(chunk_event(
                        &id,
                        created,
                        &model_id,
                        ChunkDelta {
                            role: None,
                            content: Some(content),
                            tool_calls: None,
                        },
                        None,
                    ));
                    chunks.push(chunk_event(
                        &id,
                        created,
                        &model_id,
                        ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: None,
                        },
                        Some("stop"),
                    ));
                } else {
                    chunks.push(chunk_event(
                        &id,
                        created,
                        &model_id,
                        ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: Some(tool_calls),
                        },
                        None,
                    ));
                    chunks.push(chunk_event(
                        &id,
                        created,
                        &model_id,
                        ChunkDelta {
                            role: None,
                            content: None,
                            tool_calls: None,
                        },
                        Some("tool_calls"),
                    ));
                }
                chunks
            }
            // No error frame in the OpenAI streaming spec — close the
            // stream with a message in the final delta's content so
            // it's at least visible to the client instead of a
            // silently truncated response.
            StreamMsg::Error(e) => vec![chunk_event(
                &id,
                created,
                &model_id,
                ChunkDelta {
                    role: Some("assistant"),
                    content: Some(format!("[omarchy-novad error: {e}]")),
                    tool_calls: None,
                },
                Some("stop"),
            )],
        };
        stream::iter(chunks.into_iter().map(Ok::<_, Infallible>))
    });

    let done_marker = stream::once(async { Ok::<_, Infallible>(Event::default().data("[DONE]")) });
    let full_stream: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(events.chain(done_marker));

    Sse::new(full_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

fn chunk_event(
    id: &str,
    created: u64,
    model: &str,
    delta: ChunkDelta,
    finish_reason: Option<&'static str>,
) -> Event {
    let chunk = ChatCompletionChunk {
        id: id.to_string(),
        object: "chat.completion.chunk",
        created,
        model: model.to_string(),
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason,
        }],
    };
    Event::default().data(serde_json::to_string(&chunk).unwrap_or_default())
}

#[derive(Serialize)]
struct ChatCompletionChunk {
    id: String,
    object: &'static str,
    created: u64,
    model: String,
    choices: Vec<ChunkChoice>,
}

#[derive(Serialize)]
struct ChunkChoice {
    index: u32,
    delta: ChunkDelta,
    finish_reason: Option<&'static str>,
}

#[derive(Serialize)]
struct ChunkDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallOut>>,
}

/// Result of one generation call — the raw model output plus token
/// counts, before tool-call parsing. Shared by the streaming and
/// non-streaming paths.
struct GenerationResult {
    text: String,
    prompt_tokens: usize,
    completion_tokens: usize,
}

fn run_generation_core(
    state: &AppState,
    req: &ChatCompletionRequest,
) -> Result<GenerationResult, ServeError> {
    let (history, gen_config) = build_history_and_config(req)?;

    let mut pipeline = state
        .pipeline
        .lock()
        .map_err(|_| ServeError::Infer("pipeline lock poisoned".to_string()))?;
    let results = pipeline
        .generate_with_history(&history, Some(&gen_config), None)
        .map_err(|e| ServeError::Infer(format!("generate_with_history: {e}")))?;
    let text = results
        .get_string()
        .map_err(|e| ServeError::Infer(format!("get_string: {e}")))?;
    let (prompt_tokens, completion_tokens) = results
        .get_perf_metrics()
        .and_then(|m| Ok((m.get_num_input_tokens()?, m.get_num_generation_tokens()?)))
        .unwrap_or((0, 0));

    Ok(GenerationResult {
        text,
        prompt_tokens,
        completion_tokens,
    })
}

/// Parse Qwen3-Coder's tool-call markup out of raw model output.
///
/// Format confirmed from the model's own `chat_template.jinja` (not
/// guessed): `<tool_call>\n<function=name>\n<parameter=key>\nvalue\n
/// </parameter>\n...</function>\n</tool_call>`, zero or more times.
/// Reasoning/plain text always comes before the first tag, never after
/// (the template's own instruction to the model). Parameter values come
/// back as plain text with no type info, so every value is emitted as a
/// JSON string — correct for text/path/command-shaped arguments, wrong
/// for a tool whose schema expects a number or bool. Fixing that needs
/// cross-referencing the original tool schema's `parameters`, not done
/// here.
fn parse_tool_calls(text: &str) -> (String, Vec<ToolCallOut>) {
    let Some(first_tag) = text.find("<tool_call>") else {
        return (text.trim().to_string(), Vec::new());
    };
    let content = text[..first_tag].trim().to_string();

    let mut calls = Vec::new();
    let mut rest = &text[first_tag..];
    let mut call_index = 0;
    while let Some(start) = rest.find("<tool_call>") {
        let after_open = &rest[start + "<tool_call>".len()..];
        let Some(end) = after_open.find("</tool_call>") else {
            break;
        };
        let block = &after_open[..end];
        if let Some(call) = parse_one_tool_call(block, call_index) {
            calls.push(call);
            call_index += 1;
        }
        rest = &after_open[end + "</tool_call>".len()..];
    }

    (content, calls)
}

fn parse_one_tool_call(block: &str, index: usize) -> Option<ToolCallOut> {
    let block = block.trim();
    let name_start = block.find("<function=")? + "<function=".len();
    let name_end = block[name_start..].find('>')? + name_start;
    let name = block[name_start..name_end].trim().to_string();

    let body_start = name_end + 1;
    let body_end = block.find("</function>").unwrap_or(block.len());
    let body = &block[body_start.min(body_end)..body_end];

    let mut args = serde_json::Map::new();
    let mut rest = body;
    while let Some(start) = rest.find("<parameter=") {
        let key_start = start + "<parameter=".len();
        let Some(key_end_rel) = rest[key_start..].find('>') else {
            break;
        };
        let key_end = key_start + key_end_rel;
        let key = rest[key_start..key_end].trim().to_string();

        let value_start = key_end + 1;
        let Some(value_end_rel) = rest[value_start..].find("</parameter>") else {
            break;
        };
        let value_end = value_start + value_end_rel;
        let value = rest[value_start..value_end].trim().to_string();

        args.insert(key, serde_json::Value::String(value));
        rest = &rest[value_end + "</parameter>".len()..];
    }

    Some(ToolCallOut {
        id: format!("call_novad_{index}_{}", now_unix_ms()),
        kind: "function",
        function: ToolCallFunctionOut {
            name,
            arguments: serde_json::Value::Object(args).to_string(),
        },
    })
}

/// Shared between the streaming and non-streaming paths: turn the
/// request's messages/sampling params into a `ChatHistory` +
/// `GenerationConfig` pair ready to hand to `generate_with_history`.
fn build_history_and_config(
    req: &ChatCompletionRequest,
) -> Result<(ChatHistory, GenerationConfig), ServeError> {
    let mut history =
        ChatHistory::new().map_err(|e| ServeError::Infer(format!("ChatHistory::new: {e}")))?;
    for m in &req.messages {
        let text = m
            .content
            .as_ref()
            .map(MessageContent::to_text)
            .unwrap_or_default();
        let msg = match m.role.as_str() {
            "system" => ChatMessage::system(text),
            "assistant" => match &m.tool_calls {
                Some(calls) if !calls.is_empty() => ChatMessage::assistant_with_tool_calls(
                    text,
                    calls
                        .iter()
                        .map(|c| ToolCall {
                            name: c.function.name.clone(),
                            arguments: c.function.arguments.clone(),
                        })
                        .collect(),
                ),
                _ => ChatMessage::assistant(text),
            },
            "tool" => {
                let call_id = m.tool_call_id.clone().unwrap_or_default();
                ChatMessage::tool(text, call_id)
            }
            // "user" and any unrecognized role default to user — an
            // OpenAI-compatible client shouldn't send anything else,
            // but failing the whole request over an unknown role string
            // is worse than treating it as user content.
            _ => ChatMessage::user(text),
        };
        history
            .push(&msg)
            .map_err(|e| ServeError::Infer(format!("ChatHistory::push: {e}")))?;
    }

    if let Some(tools) = &req.tools {
        let tools_json = serde_json::to_string(tools)
            .map_err(|e| ServeError::Infer(format!("serialize tools: {e}")))?;
        let tools_container = JsonContainer::from_json_str(&tools_json)
            .map_err(|e| ServeError::Infer(format!("JsonContainer::from_json_str(tools): {e}")))?;
        history
            .set_tools(&tools_container)
            .map_err(|e| ServeError::Infer(format!("ChatHistory::set_tools: {e}")))?;
    }

    let mut gen_config = GenerationConfig::new()
        .map_err(|e| ServeError::Infer(format!("GenerationConfig::new: {e}")))?;
    gen_config
        .set_max_new_tokens(req.max_tokens)
        .map_err(|e| ServeError::Infer(format!("set_max_new_tokens: {e}")))?;
    if let Some(t) = req.temperature {
        gen_config
            .set_temperature(t)
            .map_err(|e| ServeError::Infer(format!("set_temperature: {e}")))?;
        gen_config
            .set_do_sample(t > 0.0)
            .map_err(|e| ServeError::Infer(format!("set_do_sample: {e}")))?;
    }
    if let Some(p) = req.top_p {
        gen_config
            .set_top_p(p)
            .map_err(|e| ServeError::Infer(format!("set_top_p: {e}")))?;
    }

    Ok((history, gen_config))
}

fn run_generation(
    state: &AppState,
    req: &ChatCompletionRequest,
) -> Result<ChatCompletionResponse, ServeError> {
    let result = run_generation_core(state, req)?;
    let (content, tool_calls) = parse_tool_calls(&result.text);

    let (message, finish_reason) = if tool_calls.is_empty() {
        (
            ChatMessageOut {
                role: "assistant",
                content: Some(content),
                tool_calls: None,
            },
            "stop",
        )
    } else {
        (
            ChatMessageOut {
                role: "assistant",
                content: None,
                tool_calls: Some(tool_calls),
            },
            "tool_calls",
        )
    };

    Ok(ChatCompletionResponse {
        id: format!("chatcmpl-novad-{}", now_unix_ms()),
        object: "chat.completion",
        created: now_unix_ms() / 1000,
        model: state.model_id.clone(),
        choices: vec![ChatChoice {
            index: 0,
            message,
            finish_reason,
        }],
        usage: ChatUsage {
            prompt_tokens: result.prompt_tokens,
            completion_tokens: result.completion_tokens,
            total_tokens: result.prompt_tokens + result.completion_tokens,
        },
    })
}

fn err_response(status: StatusCode, message: &str) -> axum::response::Response {
    (
        status,
        Json(ErrorBody {
            error: ErrorDetail {
                message: message.to_string(),
                kind: "novad_error",
            },
        }),
    )
        .into_response()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tool_call_tests {
    use super::*;

    #[test]
    fn plain_text_no_tool_call() {
        let (content, calls) = parse_tool_calls("Just a plain answer, no tools involved.");
        assert_eq!(content, "Just a plain answer, no tools involved.");
        assert!(calls.is_empty());
    }

    #[test]
    fn single_tool_call_matches_qwen3_coder_template_exactly() {
        // Exact shape from the model's own chat_template.jinja, not a
        // simplified stand-in.
        let text = "I'll check the focused window.\n<tool_call>\n<function=hyprctl>\n<parameter=command>\nactivewindow\n</parameter>\n</function>\n</tool_call>";
        let (content, calls) = parse_tool_calls(text);
        assert_eq!(content, "I'll check the focused window.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "hyprctl");
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["command"], "activewindow");
    }

    #[test]
    fn multiple_parameters() {
        let text = "<tool_call>\n<function=bash>\n<parameter=command>\nls -la\n</parameter>\n<parameter=cwd>\n/home/trevor\n</parameter>\n</function>\n</tool_call>";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["command"], "ls -la");
        assert_eq!(args["cwd"], "/home/trevor");
    }

    #[test]
    fn multiline_parameter_value() {
        // The template explicitly documents this: "This is the value
        // for the second parameter that can span multiple lines".
        let text = "<tool_call>\n<function=write>\n<parameter=content>\nline one\nline two\nline three\n</parameter>\n</function>\n</tool_call>";
        let (_, calls) = parse_tool_calls(text);
        let args: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
        assert_eq!(args["content"], "line one\nline two\nline three");
    }

    #[test]
    fn no_parameters() {
        let text = "<tool_call>\n<function=list_windows>\n</function>\n</tool_call>";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "list_windows");
        assert_eq!(calls[0].function.arguments, "{}");
    }

    #[test]
    fn multiple_sequential_tool_calls() {
        let text = "<tool_call>\n<function=first>\n</function>\n</tool_call>\n<tool_call>\n<function=second>\n</function>\n</tool_call>";
        let (_, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "first");
        assert_eq!(calls[1].function.name, "second");
        assert_ne!(calls[0].id, calls[1].id, "each call needs a distinct id");
    }

    #[test]
    fn malformed_missing_closing_tag_does_not_panic() {
        let text = "<tool_call>\n<function=broken>\n<parameter=x>\nunterminated";
        let (_, calls) = parse_tool_calls(text);
        assert!(calls.is_empty());
    }
}
