//! Command router — maps a [`Intent`](crate::classify::Intent) from
//! the classifier to an executed action. Scoped port of nova-npu's
//! `ai/router.py` + `ai/commands/*.py`: covers the intents that have
//! a simple, config-free local handler (app launch, web search/open,
//! system volume/brightness, MPRIS media control) plus TERMINAL, which
//! needs a confirm round-trip through the popup before running
//! anything. Intents nova routed through per-user config or an
//! external LLM (HOME_ASSISTANT, MEMORY_RETURN, CODING, EXTERNAL) fall
//! through to [`RouteResult::Unhandled`] here — there's no
//! `novad serve`-hosted router smart enough to safely improvise a
//! shell command or HA call yet, and guessing wrong there is worse
//! than admitting "not yet."

mod app_launcher;
mod media_control;
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
        Intent::HomeAssistant | Intent::MemoryReturn | Intent::Coding | Intent::External => {
            RouteResult::Unhandled
        }
    }
}

/// Execute a [`RouteResult::NeedsConfirmation`] command after the user
/// approved it in the popup. Only `Terminal` ever produces that
/// variant today, so this only needs to cover that case; a future
/// confirmable intent should extend this the same way `router.py`'s
/// `execute_confirmed` did.
pub fn run_confirmed_terminal(argument: &str) -> (bool, String) {
    terminal::run(argument)
}
