//! Vision-language model prompt rendering for `serve`'s `Pipeline::Vlm`
//! path.
//!
//! `VlmPipeline::generate` (unlike `LlmPipeline::generate_with_history`)
//! takes a single flat prompt string, not a `ChatHistory` -- the
//! `openvino_genai` Rust bindings expose no way to apply a model's own
//! chat template to a VLM prompt the way `ChatHistory` does internally
//! for the LLM path. So this renders the model's own `chat_template.jinja`
//! (shipped in every OpenVINO IR checkout, the same file HF's
//! `apply_chat_template` would use) with `minijinja`, producing the exact
//! prompt structure the model actually expects -- not a guessed
//! approximation. Verified against the real qwen3.8-27b template: it
//! renders the identical `<tool_call>\n<function=...` XML markup
//! `serve::parse_tool_calls` already parses for the LLM path, so no new
//! *output* parser is needed here, only this new prompt-*input* path.
//!
//! Image tensors aren't wired up yet. The template renders
//! `<|vision_start|><|image_pad|><|vision_end|>` placeholder tokens for
//! any image/video content part -- exactly where `VlmPipeline::generate`'s
//! `images` parameter would need to supply real `ov_tensor_t` pointers --
//! but every request today renders with an empty `images` array (text
//! content only; non-text parts in a message are silently dropped by
//! `MessageContent::to_text`, same as the LLM path already does).

use minijinja::value::Value;
use minijinja::{Environment, ErrorKind};

use super::{ChatCompletionRequest, MessageContent, ServeError};

/// Render `template_source` (the model's own `chat_template.jinja`)
/// against `req`, producing the prompt string to hand `VlmPipeline::generate`.
///
/// `enable_thinking` is passed straight through as a real template
/// variable -- unlike the LLM path's `NO_THINK_SUFFIX` suffix trick (a
/// workaround for *not* controlling the template rendering directly),
/// this path renders the template itself, so thinking suppression is
/// just the variable the template already branches on (see
/// `chat_template.jinja`'s own `enable_thinking` checks) rather than a
/// soft hint the model might not honor.
pub fn render_prompt(
    template_source: &str,
    req: &ChatCompletionRequest,
    enable_thinking: bool,
) -> Result<String, ServeError> {
    let mut env = Environment::new();
    // HF chat templates commonly call a `raise_exception(msg)` function
    // for malformed input (e.g. "System message cannot contain images.")
    // -- not a minijinja builtin, so without this every such guard in
    // the template would fail with "unknown function" instead of the
    // template's own, more useful message.
    env.add_function(
        "raise_exception",
        |msg: String| -> Result<(), minijinja::Error> {
            Err(minijinja::Error::new(ErrorKind::InvalidOperation, msg))
        },
    );
    // HF chat templates routinely call Python string methods
    // (`content.startswith(...)`, seen live in qwen3.8-27b's own
    // template) that plain minijinja doesn't support -- Jinja2 itself
    // doesn't either, but transformers' Environment falls through to
    // real Python attribute lookup, which HF templates lean on freely.
    // minijinja-contrib's pycompat module implements the common ones.
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env.add_template("chat", template_source)
        .map_err(|e| ServeError::Infer(format!("parse chat_template.jinja: {e}")))?;
    let template = env
        .get_template("chat")
        .map_err(|e| ServeError::Infer(format!("get chat template: {e}")))?;

    let messages: Vec<Value> = req.messages.iter().map(message_to_context).collect();
    let tools = req
        .tools
        .as_ref()
        .map(Value::from_serialize)
        .unwrap_or(Value::UNDEFINED);

    let ctx = minijinja::context! {
        messages => messages,
        tools => tools,
        add_generation_prompt => true,
        enable_thinking => enable_thinking,
    };

    template
        .render(ctx)
        .map_err(|e| ServeError::Infer(format!("render chat_template.jinja: {e}")))
}

fn message_to_context(m: &super::ChatMessageIn) -> Value {
    let content = m
        .content
        .as_ref()
        .map(MessageContent::to_text)
        .unwrap_or_default();
    let mut obj = serde_json::json!({
        "role": m.role,
        "content": content,
    });

    if let Some(tool_calls) = &m.tool_calls {
        let rendered: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|c| {
                // The template does `tool_call.arguments|items`, i.e. it
                // expects a real mapping to iterate key/value pairs --
                // not the raw JSON-encoded string the wire format
                // carries (see ToolCallFunctionIn's own doc comment).
                // A client sending malformed JSON here shouldn't fail
                // the whole render; fall back to an empty object rather
                // than erroring, same spirit as the rest of this
                // server's tolerance for imperfect client input.
                let arguments: serde_json::Value = serde_json::from_str(&c.function.arguments)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            "[serve:vlm] tool_call arguments weren't valid JSON ({e}), \
                             rendering as empty: {:?}",
                            c.function.arguments
                        );
                        serde_json::json!({})
                    });
                serde_json::json!({
                    "function": {
                        "name": c.function.name,
                        "arguments": arguments,
                    }
                })
            })
            .collect();
        obj["tool_calls"] = serde_json::Value::Array(rendered);
    }

    if let Some(id) = &m.tool_call_id {
        obj["tool_call_id"] = serde_json::Value::String(id.clone());
    }

    Value::from_serialize(&obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serve::{ChatMessageIn, MessageContent, ToolCallFunctionIn, ToolCallIn};

    // A minimal but representative slice of the real qwen3.8-27b
    // template's own tool-call/system/user/assistant handling -- not
    // the full 169-line file, but exercises every path
    // message_to_context actually feeds: tools rendering, a plain user
    // turn, and an assistant turn with a tool call.
    const TEMPLATE: &str = r#"
{%- if tools %}<tools>{% for t in tools %}{{ t | tojson }}{% endfor %}</tools>{%- endif -%}
{%- for message in messages -%}
{%- if message.role == "user" -%}
<|im_start|>user
{{ message.content }}<|im_end|>
{%- elif message.role == "assistant" -%}
<|im_start|>assistant
{{ message.content }}
{%- if message.tool_calls -%}
{%- for tc in message.tool_calls -%}
<tool_call><function={{ tc.function.name }}>
{%- for k, v in tc.function.arguments|items -%}
<parameter={{ k }}>{{ v }}</parameter>
{%- endfor -%}
</function></tool_call>
{%- endfor -%}
{%- endif -%}
<|im_end|>
{%- endif -%}
{%- endfor -%}
{%- if add_generation_prompt -%}
<|im_start|>assistant
{%- if enable_thinking is defined and enable_thinking is false -%}
<think>

</think>

{%- else -%}
<think>
{%- endif -%}
{%- endif -%}
"#;

    fn req(
        messages: Vec<ChatMessageIn>,
        tools: Option<serde_json::Value>,
    ) -> ChatCompletionRequest {
        ChatCompletionRequest {
            model: None,
            messages,
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            stream: false,
            tools,
        }
    }

    fn user(text: &str) -> ChatMessageIn {
        ChatMessageIn {
            role: "user".to_string(),
            content: Some(MessageContent::Text(text.to_string())),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn renders_plain_user_turn_with_thinking_enabled() {
        let r = req(vec![user("hello")], None);
        let out = render_prompt(TEMPLATE, &r, true).unwrap();
        assert!(out.contains("<|im_start|>user\nhello<|im_end|>"));
        assert!(out.ends_with("<|im_start|>assistant<think>"));
    }

    #[test]
    fn thinking_disabled_renders_empty_think_block() {
        let r = req(vec![user("hello")], None);
        let out = render_prompt(TEMPLATE, &r, false).unwrap();
        assert!(out.ends_with("<think>\n\n</think>"));
    }

    #[test]
    fn tools_render_via_tojson() {
        let tools = serde_json::json!([{"type": "function", "function": {"name": "get_weather"}}]);
        let r = req(vec![user("weather?")], Some(tools));
        let out = render_prompt(TEMPLATE, &r, true).unwrap();
        assert!(out.contains("<tools>"));
        assert!(out.contains("get_weather"));
    }

    #[test]
    fn assistant_tool_call_arguments_render_via_items_filter() {
        let assistant = ChatMessageIn {
            role: "assistant".to_string(),
            content: Some(MessageContent::Text(String::new())),
            tool_calls: Some(vec![ToolCallIn {
                function: ToolCallFunctionIn {
                    name: "hyprctl".to_string(),
                    arguments: r#"{"command":"activewindow"}"#.to_string(),
                },
            }]),
            tool_call_id: None,
        };
        let r = req(vec![user("check window"), assistant], None);
        let out = render_prompt(TEMPLATE, &r, true).unwrap();
        assert!(out.contains("<tool_call><function=hyprctl>"));
        assert!(out.contains("<parameter=command>activewindow</parameter>"));
    }

    #[test]
    fn malformed_tool_call_arguments_render_as_empty_object_not_error() {
        let assistant = ChatMessageIn {
            role: "assistant".to_string(),
            content: Some(MessageContent::Text(String::new())),
            tool_calls: Some(vec![ToolCallIn {
                function: ToolCallFunctionIn {
                    name: "hyprctl".to_string(),
                    arguments: "not valid json".to_string(),
                },
            }]),
            tool_call_id: None,
        };
        let r = req(vec![user("check window"), assistant], None);
        let out = render_prompt(TEMPLATE, &r, true).unwrap();
        assert!(out.contains("<tool_call><function=hyprctl>"));
        assert!(out.contains("</function></tool_call>"));
    }
}
