//! Intent classification — routes a transcribed utterance to one of
//! nova-npu's original intent categories (`ai/intent_classifier.py`),
//! but as a plain HTTP client against `omarchy-novad serve`'s own
//! `/v1/chat/completions`, not a second hosted model.
//!
//! Originally scoped for a dedicated small model (Qwen3-1.7B-Instruct)
//! alongside the bigger chat model `omarchy-novad serve` already hosts for
//! OmaPilot. Measured against the real Qwen3-Coder-30B-A3B instance
//! instead first: 0.5-1.6s per classification once warm, correct on
//! every test utterance tried. Not as fast as a dedicated small model
//! would be, but fast enough, and it means omarchy-novad never has to load a
//! second model into memory just for this. If per-call latency ever
//! becomes a real problem, the fix is a dedicated small model behind
//! its own `omarchy-novad serve` instance — this module doesn't care which
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
    /// Send an iMessage via BlueBubbles — see `router::bluebubbles`.
    /// Always routes through `RouteResult::NeedsConfirmation`: texting a
    /// real person is a much higher-stakes, harder-to-undo action than
    /// toggling a light, so unlike `HomeAssistant` it never executes
    /// straight from the classifier's output.
    Message,
    /// Anything the local model shouldn't handle itself — reasoning,
    /// open-ended questions, writing, general knowledge. Handed off to
    /// OpenClaw via `router::handoff_to_openclaw` (see openclaw.rs),
    /// same as `Coding`; the two intents exist separately only because
    /// nova's original taxonomy did and the classifier still uses both
    /// labels, but they're routed identically.
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
            "MESSAGE" => Some(Self::Message),
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
            Self::Message => "MESSAGE",
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

// Bare label names with no description left HOME_ASSISTANT acting as
// an accidental catch-all for anything conversational/open-ended
// ("write a haiku about kubernetes", "what is the meaning of life",
// "explain kubernetes pods" all misclassified as HOME_ASSISTANT in
// testing, none of which mention a smart-home device) -- the model
// had nothing to disambiguate it from CODING/EXTERNAL.
//
// First fix attempt used full-sentence descriptions per intent on
// separate lines; that overcorrected on this 1.7B model -- it started
// echoing back words from the *description* as the ARGUMENT (e.g.
// "turn on the living room lights" -> argument "control a smart-home
// device -- lights, locks...") and even invented a label not in the
// enum ("EXPLANATION"). Short parenthetical examples inline, one
// line, keeps the original's terse shape while still disambiguating,
// and the ARGUMENT line now says explicitly to pull from the
// utterance, not this prompt.
const SYSTEM_PROMPT: &str = "\
Classify the user utterance into exactly one intent: \
OPEN_APP (launch an app, e.g. \"open firefox\"), \
WEB_SEARCH (search the web), \
OPEN_WEBSITE (open a known site, e.g. \"go to youtube\"), \
SYSTEM_CONTROL (volume/brightness), \
TERMINAL (run a shell command), \
HOME_ASSISTANT (control a smart-home device: lights, locks, thermostat), \
MEDIA_CONTROL (music/video playback), \
MEMORY_RETURN (recall something said earlier), \
CODING (write/explain/debug code), \
MESSAGE (send a text/iMessage to a person, e.g. \"text mom I'm running late\"), \
EXTERNAL (anything else: open-ended questions, explanations, general \
knowledge -- the default when nothing else fits). \
Respond with ONLY two lines, no other text:\n\
INTENT: <name>\n\
ARGUMENT: <the relevant words copied verbatim from the user's utterance>";

/// Generation budget for a classification call. Thinking-mode models
/// (Qwen3's default chat template) emit a `<think>...</think>` block
/// before the actual answer unless suppressed — see `/no_think` below
/// and `strip_thinking`. 32 was sized for a non-thinking model and
/// truncated Qwen3-1.7B mid-thought every time even with `/no_think`
/// appended, so this covers worst-case reasoning tokens too, not just
/// the two-line answer.
const MAX_TOKENS: usize = 256;

/// Qwen3's soft-switch to skip the `<think>` block for this turn,
/// appended to the user message content per Qwen's own convention
/// (it's a per-turn directive, not a system-prompt one). Belt-and-
/// suspenders with `strip_thinking`: some quantized/converted variants
/// don't honor it reliably, so the parser also strips any `<think>`
/// block that slips through instead of relying on this alone.
const NO_THINK_SUFFIX: &str = "\n/no_think";

pub struct Classifier {
    base_url: String,
    model_id: String,
}

impl Classifier {
    /// `base_url` is `omarchy-novad serve`'s own address, e.g.
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

        let user_content = format!("{utterance}{NO_THINK_SUFFIX}");
        let req = ChatRequest {
            model: &self.model_id,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: SYSTEM_PROMPT,
                },
                ChatMessage {
                    role: "user",
                    content: &user_content,
                },
            ],
            max_tokens: MAX_TOKENS,
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

/// Removes a leading `<think>...</think>` reasoning block, if present.
/// `/no_think` (see `NO_THINK_SUFFIX`) usually prevents the model from
/// emitting one at all, but isn't honored by every quantized/converted
/// variant, so the parser strips it defensively rather than trusting
/// the directive alone. Only strips a block anchored at the very start
/// (after whitespace) — content that merely mentions "<think>" further
/// in isn't touched.
fn strip_thinking(content: &str) -> &str {
    let trimmed = content.trim_start();
    match trimmed.strip_prefix("<think>") {
        Some(rest) => rest.split_once("</think>").map_or(rest, |(_, after)| after),
        None => trimmed,
    }
}

/// The model is asked for `ARGUMENT: <words copied verbatim>` but
/// inconsistently wraps that answer in quotes anyway (observed live:
/// `ARGUMENT: "text Jessica is this working?"` right alongside
/// unquoted `ARGUMENT: Turn on the living room, PV.` from the same
/// session) -- one matching pair of straight quotes around the whole
/// value is model formatting, not user content, so strip it. This
/// mattered more than it looks: a leading `"` silently defeated every
/// `LEADING_PHRASES` prefix match in `router::bluebubbles::parse_command`
/// (none of them start with a quote), so "text Jessica ..." resolved
/// the *contact name* to `"text` instead of `Jessica` and failed
/// closed with "No contact named ... found" -- indistinguishable from
/// nothing happening at all unless the popup was up to show it.
fn strip_wrapping_quotes(s: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(quote).and_then(|s| s.strip_suffix(quote)) {
            return inner;
        }
    }
    s
}

fn parse_response(
    content: &str,
    latency: std::time::Duration,
) -> Result<ClassificationResult, ClassifyError> {
    let content = strip_thinking(content);
    let mut intent_str = None;
    let mut argument = String::new();
    for line in content.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("INTENT:") {
            intent_str = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("ARGUMENT:") {
            argument = strip_wrapping_quotes(rest.trim()).to_string();
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
    fn strips_leading_think_block() {
        let result = parse_response(
            "<think>\nThe user wants to open an app.\n</think>\n\nINTENT: OPEN_APP\nARGUMENT: firefox",
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(result.intent, Intent::OpenApp);
        assert_eq!(result.argument, "firefox");
    }

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
    fn strips_wrapping_quotes_from_argument() {
        let result = parse_response(
            "INTENT: MESSAGE\nARGUMENT: \"text Jessica is this working?\"",
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(result.intent, Intent::Message);
        assert_eq!(result.argument, "text Jessica is this working?");
    }

    #[test]
    fn leaves_an_inner_quote_alone() {
        // Only a matching pair wrapping the *whole* value is stripped --
        // a single stray quote (unbalanced) is left as-is rather than
        // guessed at.
        let result = parse_response(
            "INTENT: MESSAGE\nARGUMENT: text mom say \"hi\" to dad",
            std::time::Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(result.argument, "text mom say \"hi\" to dad");
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
