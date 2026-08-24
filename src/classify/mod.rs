//! Intent classification — routes a transcribed utterance to one of
//! nova-npu's original intent categories (`ai/intent_classifier.py`),
//! but as a plain HTTP client against `novad serve`'s own
//! `/v1/chat/completions`, not a second hosted model.
//!
//! Originally scoped for a dedicated small model (Qwen3-1.7B-Instruct)
//! alongside the bigger chat model `novad serve` already hosts for
//! OmaPilot. Measured against the real Qwen3-Coder-30B-A3B instance
//! instead first: 0.5-1.6s per classification once warm, correct on
//! every test utterance tried. Not as fast as a dedicated small model
//! would be, but fast enough, and it means novad never has to load a
//! second model into memory just for this. If per-call latency ever
//! becomes a real problem, the fix is a dedicated small model behind
//! its own `novad serve` instance — this module doesn't care which
//! model answers, only that something at `base_url` does.

use serde::{Deserialize, Serialize};

/// nova's original intent taxonomy (`ai/intent_classifier.py`'s
/// `Intent` enum), unchanged — the classifier prompt below asks the
/// model to answer with exactly one of these names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    OpenApp,
    WebSearch,
    OpenWebsite,
    SystemControl,
    Terminal,
    HomeAssistant,
    MediaControl,
    MemoryReturn,
    Coding,
    /// Anything the local model shouldn't handle itself — nova forwarded
    /// this to an external provider (OpenClaw). novad's equivalent is
    /// OmaPilot, when it's the active trigger; see the roadmap's
    /// standalone-vs-OmaPilot decision.
    External,
}

impl Intent {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "OPEN_APP" => Some(Self::OpenApp),
            "WEB_SEARCH" => Some(Self::WebSearch),
            "OPEN_WEBSITE" => Some(Self::OpenWebsite),
            "SYSTEM_CONTROL" => Some(Self::SystemControl),
            "TERMINAL" => Some(Self::Terminal),
            "HOME_ASSISTANT" => Some(Self::HomeAssistant),
            "MEDIA_CONTROL" => Some(Self::MediaControl),
            "MEMORY_RETURN" => Some(Self::MemoryReturn),
            "CODING" => Some(Self::Coding),
            "EXTERNAL" => Some(Self::External),
            _ => None,
        }
    }
}

impl std::fmt::Display for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::OpenApp => "OPEN_APP",
            Self::WebSearch => "WEB_SEARCH",
            Self::OpenWebsite => "OPEN_WEBSITE",
            Self::SystemControl => "SYSTEM_CONTROL",
            Self::Terminal => "TERMINAL",
            Self::HomeAssistant => "HOME_ASSISTANT",
            Self::MediaControl => "MEDIA_CONTROL",
            Self::MemoryReturn => "MEMORY_RETURN",
            Self::Coding => "CODING",
            Self::External => "EXTERNAL",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct ClassificationResult {
    pub intent: Intent,
    pub argument: String,
    // Not read yet — kept for debug logging / a future "why did it
    // pick this intent" surface, same idea as Detection's timestamp.
    #[allow(dead_code)]
    pub raw_text: String,
    pub latency: std::time::Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ClassifyError {
    #[error("request to {0} failed: {1}")]
    Request(String, String),
    #[error("model response didn't match the expected INTENT/ARGUMENT format: {0:?}")]
    UnexpectedFormat(String),
    #[error("model returned an intent name we don't recognize: {0:?}")]
    UnknownIntent(String),
}

const SYSTEM_PROMPT: &str = "\
Classify the user utterance into exactly one intent: OPEN_APP, WEB_SEARCH, \
OPEN_WEBSITE, SYSTEM_CONTROL, TERMINAL, HOME_ASSISTANT, MEDIA_CONTROL, \
MEMORY_RETURN, CODING, EXTERNAL. Respond with ONLY two lines, no other text:\n\
INTENT: <name>\n\
ARGUMENT: <extracted argument text>";

pub struct Classifier {
    base_url: String,
    model_id: String,
}

impl Classifier {
    /// `base_url` is `novad serve`'s own address, e.g.
    /// `http://127.0.0.1:8420`.
    pub fn new(base_url: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            model_id: model_id.into(),
        }
    }

    pub fn classify(&self, utterance: &str) -> Result<ClassificationResult, ClassifyError> {
        let start = std::time::Instant::now();
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );

        let req = ChatRequest {
            model: &self.model_id,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: SYSTEM_PROMPT,
                },
                ChatMessage {
                    role: "user",
                    content: utterance,
                },
            ],
            max_tokens: 32,
            temperature: 0.0,
        };

        let response: ChatResponse = ureq::post(&url)
            .set("Content-Type", "application/json")
            .send_json(&req)
            .map_err(|e| ClassifyError::Request(url.clone(), e.to_string()))?
            .into_json()
            .map_err(|e| ClassifyError::Request(url, e.to_string()))?;

        let content = response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| ClassifyError::UnexpectedFormat(String::new()))?;

        parse_response(&content, start.elapsed())
    }
}

fn parse_response(
    content: &str,
    latency: std::time::Duration,
) -> Result<ClassificationResult, ClassifyError> {
    let mut intent_str = None;
    let mut argument = String::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("INTENT:") {
            intent_str = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("ARGUMENT:") {
            argument = rest.trim().to_string();
        }
    }

    let intent_str =
        intent_str.ok_or_else(|| ClassifyError::UnexpectedFormat(content.to_string()))?;
    let intent = Intent::parse(&intent_str).ok_or(ClassifyError::UnknownIntent(intent_str))?;

    Ok(ClassificationResult {
        intent,
        argument,
        raw_text: content.to_string(),
        latency,
    })
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    max_tokens: usize,
    temperature: f32,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessageOwned,
}

#[derive(Deserialize)]
struct ChatMessageOwned {
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_response() {
        let result = parse_response(
            "INTENT: OPEN_APP\nARGUMENT: firefox",
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(result.intent, Intent::OpenApp);
        assert_eq!(result.argument, "firefox");
    }

    #[test]
    fn parses_response_with_extra_whitespace() {
        let result = parse_response(
            "  INTENT:   WEB_SEARCH  \n  ARGUMENT:  best pizza near me  ",
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(result.intent, Intent::WebSearch);
        assert_eq!(result.argument, "best pizza near me");
    }

    #[test]
    fn rejects_unknown_intent() {
        let err = parse_response(
            "INTENT: MAKE_COFFEE\nARGUMENT: espresso",
            std::time::Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(matches!(err, ClassifyError::UnknownIntent(_)));
    }

    #[test]
    fn rejects_missing_intent_line() {
        let err = parse_response(
            "I'm not sure what you mean.",
            std::time::Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(matches!(err, ClassifyError::UnexpectedFormat(_)));
    }
}
