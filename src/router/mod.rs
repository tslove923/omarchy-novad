//! Command router — maps a [`Intent`](crate::classify::Intent) from
//! the classifier to an executed action. Scoped port of nova-npu's
//! `ai/router.py` + `ai/commands/*.py`: covers the intents that have
//! a simple local handler (app launch, web search/open, system
//! volume/brightness, MPRIS media control, Home Assistant when
//! `[home_assistant]` is configured -- see config.rs) plus TERMINAL,
//! MESSAGE, and TELEGRAM, which need a confirm round-trip through the
//! popup before running anything, and EXTERNAL/CODING, which hand off
//! to a real reasoning agent via `openclaw` (see openclaw.rs).
//! MEMORY_RETURN still falls through to [`RouteResult::Unhandled`] --
//! no local handler for it yet, and guessing wrong there is worse than
//! admitting "not yet."

mod app_launcher;
mod bluebubbles;
pub mod home_assistant;
mod media_control;
mod omapilot;
pub mod openclaw;
mod system_control;
mod telegram;
mod terminal;
mod web;

use crate::classify::Intent;
use crate::config::{BlueBubblesConfig, HomeAssistantConfig, TelegramConfig};

pub use omapilot::{ask as ask_omapilot, strip_direct_target_prefix};

/// Which confirmed handler to call once the user approves a
/// `RouteResult::NeedsConfirmation` in the popup — see [`run_confirmed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Terminal,
    Message,
    Telegram,
}

/// Outcome of routing one classified utterance.
pub enum RouteResult {
    /// Executed immediately; message is shown in the popup's Ready
    /// phase (see `popup::PopupPhase::Ready`).
    Done { success: bool, message: String },
    /// Needs an Approve/Deny round-trip before running — a `Terminal`
    /// command that isn't in the safe-readonly allowlist (see
    /// `terminal::is_safe_readonly`), or any `Message`/`Telegram` (both
    /// always confirm — see `classify::Intent::Message`'s doc comment).
    NeedsConfirmation {
        /// Short header shown above `body` in the popup, e.g. "Text
        /// Jessica" or "Text Jessica (new conversation)" for a Message
        /// confirmation, or "Telegram Sarah" for a Telegram one. `None`
        /// when `body` alone already says enough (Terminal's "Run:
        /// <command>" needs no separate header).
        label: Option<String>,
        /// What's shown — and, when `editable` is true, can be changed
        /// before Approve — in the popup's main text area.
        body: String,
        /// Whether `body` should render as an edit box rather than
        /// plain text. True for `Message`/`Telegram`; Terminal commands
        /// aren't edit-before-run (`run_confirmed`'s `edited_text`
        /// plumbing already supports it if that ever changes).
        editable: bool,
        kind: ConfirmKind,
    },
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
    telegram: Option<&TelegramConfig>,
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
                    label: None,
                    body: format!("Run: {argument}"),
                    editable: false,
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
                Ok(prepared) => RouteResult::NeedsConfirmation {
                    label: Some(prepared.label),
                    body: prepared.body,
                    editable: true,
                    kind: ConfirmKind::Message,
                },
                Err(message) => RouteResult::Done {
                    success: false,
                    message,
                },
            },
            None => RouteResult::Unhandled,
        },
        Intent::Telegram => match telegram {
            Some(cfg) => match telegram::prepare(argument, cfg) {
                Ok(prepared) => RouteResult::NeedsConfirmation {
                    label: Some(prepared.label),
                    body: prepared.body,
                    editable: true,
                    kind: ConfirmKind::Telegram,
                },
                Err(message) => RouteResult::Done {
                    success: false,
                    message,
                },
            },
            None => RouteResult::Unhandled,
        },
        // External/Coding don't go through here -- pipeline.rs checks
        // is_external_handoff() before calling route() and enters the
        // full conversation loop (crate::converse) against the whole
        // transcript instead of this intent's (often keyword-stripped)
        // argument. A coding/reasoning request needs the full utterance
        // for context, not an extracted phrase -- see openclaw.rs's docs.
        Intent::MemoryReturn | Intent::Coding | Intent::External => RouteResult::Unhandled,
    }
}

/// Recovery check: does `text` (meant to be the full transcript, not
/// the classifier's own argument extraction) look like a MESSAGE
/// command by its leading word(s), even though the classifier picked a
/// different intent? See `bluebubbles::looks_like_message_command`'s
/// docs. pipeline.rs uses this the same way `route`'s own
/// `SYSTEM_CONTROL` arm above uses `media_control`/`home_assistant`'s
/// `looks_like_*` checkers, just one level up -- it needs the *full*
/// transcript (a mis-heard leading verb can leave nothing recognizable
/// in the classifier's own, often keyword-stripped, argument), which
/// `route` itself never receives.
pub fn looks_like_message_command(text: &str) -> bool {
    bluebubbles::looks_like_message_command(text)
}

/// Same recovery, Telegram side -- see
/// `telegram::looks_like_telegram_command`'s docs.
pub fn looks_like_telegram_command(text: &str) -> bool {
    telegram::looks_like_telegram_command(text)
}

/// Same recovery, OpenClaw side -- see
/// `openclaw::looks_like_external_command`'s docs. Recovers into
/// `Intent::External`, not routed through `route()` at all (see
/// `is_external_handoff`) -- pipeline.rs checks this ahead of that
/// branch instead of after, unlike the Message/Telegram checks above.
pub fn looks_like_external_command(text: &str) -> bool {
    openclaw::looks_like_external_command(text)
}

/// Same recovery, MediaControl side -- see
/// `media_control::looks_like_media_command`'s docs. Already used
/// inside `route`'s own `SYSTEM_CONTROL` arm to catch the same
/// confusion one level down; exposed here too so pipeline.rs's
/// MEMORY_RETURN recovery chain can catch it before `route` is ever
/// called, same as the Message/Telegram/OpenClaw checks.
pub fn looks_like_media_command(text: &str) -> bool {
    media_control::looks_like_media_command(text)
}

/// Same recovery, Home Assistant side -- see
/// `home_assistant::looks_like_home_assistant_command`'s docs. Same
/// reasoning as `looks_like_media_command` above.
pub fn looks_like_home_assistant_command(text: &str) -> bool {
    home_assistant::looks_like_home_assistant_command(text)
}

/// Execute a [`RouteResult::NeedsConfirmation`] command after the user
/// approved it in the popup — dispatches on the `kind` that came back
/// with it. `bluebubbles` mirrors `route`'s own parameter: `None` when
/// `[bluebubbles]` isn't configured, which shouldn't be reachable in
/// practice (a `ConfirmKind::Message` can only have been produced by
/// `route` when `bluebubbles` was `Some`), but handled explicitly
/// rather than assumed away. `edited_text` is whatever was left in the
/// popup's editable box when Approve was clicked (see
/// `popup::PopupAction::Approve`) -- `None` if the confirmation wasn't
/// editable, or the user approved without touching it. Only
/// `ConfirmKind::Message` renders an edit box today (see
/// `RouteResult::NeedsConfirmation`'s `editable` field), but
/// `ConfirmKind::Terminal` honors an override here too if one somehow
/// arrives, rather than silently ignoring it.
pub fn run_confirmed(
    kind: ConfirmKind,
    argument: &str,
    edited_text: Option<&str>,
    bluebubbles: Option<&BlueBubblesConfig>,
    telegram: Option<&TelegramConfig>,
) -> (bool, String) {
    match kind {
        ConfirmKind::Terminal => terminal::run(edited_text.unwrap_or(argument)),
        ConfirmKind::Message => match bluebubbles {
            Some(cfg) => bluebubbles::run_confirmed(argument, edited_text, cfg),
            None => (
                false,
                "BlueBubbles is not configured (approved a Message confirmation with no \
                 [bluebubbles] section — this shouldn't happen)"
                    .to_string(),
            ),
        },
        ConfirmKind::Telegram => match telegram {
            Some(cfg) => telegram::run_confirmed(argument, edited_text, cfg),
            None => (
                false,
                "Telegram is not configured (approved a Telegram confirmation with no \
                 [telegram] section — this shouldn't happen)"
                    .to_string(),
            ),
        },
    }
}

/// One-time interactive Telegram login -- called by `omarchy-novad
/// setup telegram-auth` (see main.rs). See `telegram::login`'s docs.
pub fn telegram_login(
    cfg: &TelegramConfig,
    prompt_phone: impl FnOnce() -> String,
    prompt_code: impl FnOnce() -> String,
    prompt_password: impl FnOnce(Option<&str>) -> String,
) -> Result<String, String> {
    telegram::login(cfg, prompt_phone, prompt_code, prompt_password)
}

/// Direct, non-interactive send -- bypasses the confirm popup entirely.
/// Used by `omarchy-novad telegram send <name> <text>` (see main.rs),
/// itself meant for scripted callers (e.g. an OmaPilot skill script)
/// that already have their own approval step upstream, not for the
/// voice pipeline, which always confirms via the popup instead (see
/// `Intent::Telegram`'s doc comment).
pub fn telegram_send(name: &str, text: &str, cfg: &TelegramConfig) -> (bool, String) {
    telegram::run_confirmed(&format!("telegram {name} saying {text}"), None, cfg)
}

/// Same direct-send escape hatch, BlueBubbles side -- see
/// `telegram_send`'s docs for why this exists and who calls it.
pub fn bluebubbles_send(name: &str, text: &str, cfg: &BlueBubblesConfig) -> (bool, String) {
    bluebubbles::run_confirmed(&format!("text {name} saying {text}"), None, cfg)
}

/// `omarchy-novad openclaw continue-in-herdr` (see main.rs) -- see
/// `openclaw::continue_in_herdr`'s doc comment for why this is a
/// separate, explicit action rather than part of the automatic
/// wake-word handoff.
pub fn openclaw_continue_in_herdr(cfg: Option<&crate::config::OpenClawConfig>) -> (bool, String) {
    openclaw::continue_in_herdr(cfg)
}
