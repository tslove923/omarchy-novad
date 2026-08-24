//! OpenAI-compatible HTTP server backed by `openvino_genai::LlmPipeline`.
//!
//! Exists so OmaPilot (or anything else that speaks the OpenAI Chat
//! Completions API) can point at a fully local, GPU-backed model —
//! `docs/omapilot-local-provider.md` registers this as an OmaPilot
//! `models.json` provider. Replaces the earlier Ollama-based plan:
//! Ollama already speaks this API natively, so pointing OmaPilot at our
//! own OpenVINO GenAI pipeline instead means novad has to speak it too.
//!
//! Scope: single model, single in-process pipeline, non-streaming only.
//! No auth (binds to 127.0.0.1 — same trust boundary as Ollama's own
//! default). Good enough for "OmaPilot on this machine talks to novad
//! on this machine"; revisit if that stops being true.

use std::sync::Mutex;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use openvino_genai::{ChatHistory, ChatMessage, GenerationConfig, LlmPipeline};
use serde::{Deserialize, Serialize};

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
        "novad serve listening on http://{addr}  (model id: {})",
        config.model_id
    );
    println!("[novad] Serving {} at http://{addr}/v1", config.model_id);
    println!("[novad] Point OmaPilot's models.json baseUrl at http://{addr}/v1 — see docs/omapilot-local-provider.md");

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
}

fn default_max_tokens() -> usize {
    1024
}

#[derive(Deserialize)]
struct ChatMessageIn {
    role: String,
    content: String,
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
    content: String,
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
        "data": [{"id": state.model_id, "object": "model", "owned_by": "novad"}],
    }))
}

async fn chat_completions(
    State(state): State<std::sync::Arc<AppState>>,
    Json(req): Json<ChatCompletionRequest>,
) -> impl IntoResponse {
    if req.stream {
        return err_response(
            StatusCode::BAD_REQUEST,
            "stream=true is not yet supported by novad serve (non-streaming only for now)",
        );
    }
    if req.messages.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "messages must not be empty");
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

fn run_generation(
    state: &AppState,
    req: &ChatCompletionRequest,
) -> Result<ChatCompletionResponse, ServeError> {
    let mut history =
        ChatHistory::new().map_err(|e| ServeError::Infer(format!("ChatHistory::new: {e}")))?;
    for m in &req.messages {
        let msg = match m.role.as_str() {
            "system" => ChatMessage::system(m.content.clone()),
            "assistant" => ChatMessage::assistant(m.content.clone()),
            // "user" and any unrecognized role default to user — an
            // OpenAI-compatible client shouldn't send anything else,
            // but failing the whole request over an unknown role string
            // (function/tool roles some clients still emit) is worse
            // than treating it as user content.
            _ => ChatMessage::user(m.content.clone()),
        };
        history
            .push(&msg)
            .map_err(|e| ServeError::Infer(format!("ChatHistory::push: {e}")))?;
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

    Ok(ChatCompletionResponse {
        id: format!("chatcmpl-novad-{}", now_unix_ms()),
        object: "chat.completion",
        created: now_unix_ms() / 1000,
        model: state.model_id.clone(),
        choices: vec![ChatChoice {
            index: 0,
            message: ChatMessageOut {
                role: "assistant",
                content: text,
            },
            finish_reason: "stop",
        }],
        usage: ChatUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens: prompt_tokens + completion_tokens,
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
