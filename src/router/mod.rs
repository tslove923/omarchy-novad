//! Command router — maps a [`Intent`](crate::classify::Intent) from
//! the classifier to an executed action. Scoped port of nova-npu's
//! `ai/router.py` + `ai/commands/*.py`: covers the intents that have
//! a simple local handler (app launch, web search/open, system
//! volume/brightness, MPRIS media control, Home Assistant when
//! `[home_assistant]` is configured -- see config.rs) plus TERMINAL,
//! which needs a confirm round-trip through the popup before running
//! anything, and EXTERNAL/CODING, which hand off to a real reasoning
//! agent via `openclaw` (see openclaw.rs). MEMORY_RETURN still falls
//! through to [`RouteResult::Unhandled`] -- no local handler for it
//! yet, and guessing wrong there is worse than admitting "not yet."

mod app_launcher;
mod bluebubbles;
pub mod home_assistant;
mod media_control;
mod openclaw;
mod system_control;
mod terminal;
mod web;

use crate::classify::Intent;
use crate::config::{BlueBubblesConfig, HomeAssistantConfig};

/// Which confirmed handler to call once the user approves a
/// `RouteResult::NeedsConfirmation` in the popup — see [`run_confirmed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Terminal,
    Message,
}

/// Outcome of routing one classified utterance.
pub enum RouteResult {
    /// Executed immediately; message is shown in the popup's Ready
    /// phase (see `popup::PopupPhase::Ready`).
    Done { success: bool, message: String },
    /// Needs an Approve/Deny round-trip before running — a `Terminal`
    /// command that isn't in the safe-readonly allowlist (see
    /// `terminal::is_safe_readonly`), or any `Message` (BlueBubbles
    /// always confirms — see `classify::Intent::Message`'s doc comment).
    NeedsConfirmation { preview: String, kind: ConfirmKind },
    /// No local handler for this intent (yet). Caller should fall
    /// back to something else — e.g. treat the utterance as plain
    /// dictation instead of a command.
    Unhandled,
}

/// Whether `intent` is routed via a (potentially slow, seconds-to-a-
/// minute) external handoff rather than a fast local action --
/// `pipeline.rs` checks this before calling [`route`] so it can show
/// `PopupPhase::HandingOff` for the duration of the call, which
/// [`route`] itself can't signal since it's synchronous and already
/// finished by the time it returns.
pub fn is_external_handoff(intent: Intent) -> bool {
    matches!(intent, Intent::External | Intent::Coding)
}

/// Route (and, unless it needs confirmation, execute) one classified
/// utterance. `home_assistant` is `None` when `[home_assistant]` isn't
/// configured (see config.rs) -- `Intent::HomeAssistant` falls back to
/// `RouteResult::Unhandled` in that case rather than erroring, same
/// shape as every other not-yet-handled intent.
pub fn route(
    intent: Intent,
    argument: &str,
    home_assistant: Option<&HomeAssistantConfig>,
    bluebubbles: Option<&BlueBubblesConfig>,
) -> RouteResult {
    match intent {
        Intent::OpenApp => {
            let (ok, msg) = app_launcher::open_app(argument);
            RouteResult::Done {
                success: ok,
                message: msg,
            }
        }
        Intent::WebSearch => {
            let (ok, msg) = web::search(argument);
            RouteResult::Done {
                success: ok,
                message: msg,
            }
        }
        Intent::OpenWebsite => {
            let (ok, msg) = web::open_site(argument);
            RouteResult::Done {
                success: ok,
                message: msg,
            }
        }
        Intent::SystemControl => {
            // Recovery: the classifier occasionally mislabels a media
            // transport command ("pause", "stop <player>") as
            // SYSTEM_CONTROL -- see media_control::looks_like_media_command
            // for the observed live cases and why. Catch it before
            // system_control::run, which would otherwise fail the
            // whole thing with "Unknown system control: ...".
            if media_control::looks_like_media_command(argument) {
                let (ok, msg) = media_control::run(argument);
                return RouteResult::Done {
                    success: ok,
                    message: msg,
                };
            }
            // Same recovery, different pair: "turn on the living room
            // lights" observed misclassified as SYSTEM_CONTROL too
            // (see README's "Known classifier gaps") -- catch it
            // before falling through to system_control::run, but only
            // when HA is actually configured; otherwise let it fail
            // the normal system_control path rather than claim a
            // false "Home Assistant not configured" for a volume/
            // brightness phrase that happens to share a verb.
            if let Some(cfg) = home_assistant {
                if home_assistant::looks_like_home_assistant_command(argument) {
                    let (ok, msg) = home_assistant::run(argument, cfg);
                    return RouteResult::Done {
                        success: ok,
                        message: msg,
                    };
                }
            }
            let (ok, msg) = system_control::run(argument);
            RouteResult::Done {
                success: ok,
                message: msg,
            }
        }
        Intent::MediaControl => {
            let (ok, msg) = media_control::run(argument);
            RouteResult::Done {
                success: ok,
                message: msg,
            }
        }
        Intent::Terminal => {
            if terminal::is_safe_readonly(argument) {
                let (ok, msg) = terminal::run(argument);
                RouteResult::Done {
                    success: ok,
                    message: msg,
                }
            } else {
                RouteResult::NeedsConfirmation {
                    preview: format!("Run: {argument}"),
                    kind: ConfirmKind::Terminal,
                }
            }
        }
        Intent::HomeAssistant => match home_assistant {
            Some(cfg) => {
                let (ok, msg) = home_assistant::run(argument, cfg);
                RouteResult::Done {
                    success: ok,
                    message: msg,
                }
            }
            None => RouteResult::Unhandled,
        },
        Intent::Message => match bluebubbles {
            Some(cfg) => match bluebubbles::prepare(argument, cfg) {
                Ok(preview) => RouteResult::NeedsConfirmation {
                    preview,
                    kind: ConfirmKind::Message,
                },
                Err(message) => RouteResult::Done {
                    success: false,
                    message,
                },
            },
            None => RouteResult::Unhandled,
        },
        // External/Coding don't go through here -- pipeline.rs checks
        // is_external_handoff() before calling route() and calls
        // openclaw::handoff() directly against the full transcript
        // instead of this intent's (often keyword-stripped) argument.
        // A coding/reasoning request needs the whole utterance for
        // context, not an extracted phrase -- see openclaw.rs's docs.
        Intent::MemoryReturn | Intent::Coding | Intent::External => RouteResult::Unhandled,
    }
}

/// Hands `utterance` off to OpenClaw. Called directly by pipeline.rs
/// for `Intent::External`/`Intent::Coding` against the full transcript
/// (bypassing [`route`] and its argument-only signature) -- see
/// `is_external_handoff` and openclaw.rs's module docs for why.
pub fn handoff_to_openclaw(utterance: &str) -> (bool, String) {
    openclaw::handoff(utterance)
}

/// Execute a [`RouteResult::NeedsConfirmation`] command after the user
/// approved it in the popup — dispatches on the `kind` that came back
/// with it. `bluebubbles` mirrors `route`'s own parameter: `None` when
/// `[bluebubbles]` isn't configured, which shouldn't be reachable in
/// practice (a `ConfirmKind::Message` can only have been produced by
/// `route` when `bluebubbles` was `Some`), but handled explicitly
/// rather than assumed away.
pub fn run_confirmed(
    kind: ConfirmKind,
    argument: &str,
    bluebubbles: Option<&BlueBubblesConfig>,
) -> (bool, String) {
    match kind {
        ConfirmKind::Terminal => terminal::run(argument),
        ConfirmKind::Message => match bluebubbles {
            Some(cfg) => bluebubbles::run_confirmed(argument, cfg),
            None => (
                false,
                "BlueBubbles is not configured (approved a Message confirmation with no \
                 [bluebubbles] section — this shouldn't happen)"
                    .to_string(),
            ),
        },
    }
}
