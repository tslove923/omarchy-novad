//! Command router — maps a [`Intent`](crate::classify::Intent) from
//! the classifier to an executed action. Scoped port of nova-npu's
//! `ai/router.py` + `ai/commands/*.py`: covers the intents that have
//! a simple, config-free local handler (app launch, web search/open,
//! system volume/brightness, MPRIS media control) plus TERMINAL, which
//! needs a confirm round-trip through the popup before running
//! anything, and EXTERNAL/CODING, which hand off to a real reasoning
//! agent via `openclaw` (see openclaw.rs). HOME_ASSISTANT and
//! MEMORY_RETURN still fall through to [`RouteResult::Unhandled`] --
//! no local handler for those yet, and guessing wrong there (a smart-
//! home action, a memory lookup) is worse than admitting "not yet."

mod app_launcher;
mod media_control;
mod openclaw;
mod system_control;
mod terminal;
mod web;

use crate::classify::Intent;

/// Outcome of routing one classified utterance.
pub enum RouteResult {
    /// Executed immediately; message is shown in the popup's Ready
    /// phase (see `popup::PopupPhase::Ready`).
    Done { success: bool, message: String },
    /// Needs an Approve/Deny round-trip before running — currently
    /// only `Intent::Terminal` on a command that isn't in the
    /// safe-readonly allowlist (see `terminal::is_safe_readonly`).
    NeedsConfirmation { preview: String },
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
/// utterance.
pub fn route(intent: Intent, argument: &str) -> RouteResult {
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
                }
            }
        }
        // External/Coding don't go through here -- pipeline.rs checks
        // is_external_handoff() before calling route() and calls
        // openclaw::handoff() directly against the full transcript
        // instead of this intent's (often keyword-stripped) argument.
        // A coding/reasoning request needs the whole utterance for
        // context, not an extracted phrase -- see openclaw.rs's docs.
        Intent::HomeAssistant | Intent::MemoryReturn | Intent::Coding | Intent::External => {
            RouteResult::Unhandled
        }
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
/// approved it in the popup. Only `Terminal` ever produces that
/// variant today, so this only needs to cover that case; a future
/// confirmable intent should extend this the same way `router.py`'s
/// `execute_confirmed` did.
pub fn run_confirmed_terminal(argument: &str) -> (bool, String) {
    terminal::run(argument)
}
