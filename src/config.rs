//! Persistent config file: `~/.config/omarchy-novad/config.toml`.
//!
//! Everything was CLI-flags-only until Home Assistant needed
//! somewhere to keep a token that isn't shell history or `ps aux` --
//! see `home_assistant.token` below. Every field has a sensible
//! default (matching the CLI flag defaults that existed before this
//! file did) so a missing or partial config.toml, or no file at all,
//! behaves exactly like today: `[toml::from_str]`'s `#[serde(default)]`
//! on every field means an absent `[home_assistant]` section, or an
//! absent file entirely, deserializes to `Config::default()`.
//!
//! CLI flags in main.rs stay the source of truth when passed --
//! resolve_str/resolve_opt below implement "flag wins if present,
//! config file next, hardcoded default last," the same layering
//! voxtype itself uses (CLI > config file > default; omarchy-novad
//! has no env-var layer today, unlike voxtype, since nothing here has
//! asked for one yet).

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub detect: DetectConfig,
    #[serde(default)]
    pub serve: ServeConfig,
    #[serde(default)]
    pub home_assistant: Option<HomeAssistantConfig>,
    #[serde(default)]
    pub bluebubbles: Option<BlueBubblesConfig>,
    #[serde(default)]
    pub telegram: Option<TelegramConfig>,
    #[allow(dead_code)] // not read until router::spotify lands -- see SpotifyConfig's own TODO
    #[serde(default)]
    pub spotify: Option<SpotifyConfig>,
    #[serde(default)]
    pub omapilot: Option<OmaPilotConfig>,
    #[serde(default)]
    pub openclaw: Option<OpenClawConfig>,
    #[serde(default)]
    pub tts: TtsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DetectConfig {
    pub wakeword: String,
    pub device: String,
    pub threshold: f32,
    pub patience: usize,
    pub classify_base_url: String,
    pub classify_model_id: String,
    pub on_detect: Option<String>,
}

impl Default for DetectConfig {
    fn default() -> Self {
        Self {
            wakeword: "hey_jarvis".to_string(),
            device: "NPU".to_string(),
            threshold: crate::wake::detector::DEFAULT_THRESHOLD,
            patience: crate::wake::detector::DEFAULT_PATIENCE,
            classify_base_url: "http://127.0.0.1:8420".to_string(),
            classify_model_id: "qwen3-1.7b-instruct".to_string(),
            on_detect: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServeConfig {
    pub device: String,
    pub model_id: String,
    pub port: u16,
    /// `false` (default): every `/v1/chat/completions` response has its
    /// `<think>...</think>` reasoning block suppressed (`/no_think`
    /// appended to the current turn, plus a defensive strip -- see
    /// `serve::run_generation_core`) before it reaches the client.
    /// `true`: the block is kept, but reformatted as collapsible
    /// markdown (`<details><summary>...`) instead of left as raw
    /// `<think>` tags inline with the answer -- some people want to
    /// see the reasoning, but not have it clutter the visible answer
    /// by default. Whether a given client's markdown renderer actually
    /// renders `<details>` as click-to-expand (vs. plain HTML-ish
    /// text) isn't controlled by this server.
    pub show_thinking: bool,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            device: "GPU".to_string(),
            model_id: "novad-local".to_string(),
            port: 8420,
            show_thinking: false,
        }
    }
}

/// Home Assistant REST API credentials -- see `router::home_assistant`.
/// No `#[derive(Default)]`: unlike `DetectConfig`/`ServeConfig`, there's
/// no sensible default `url`/`token` (an empty string would just fail
/// every request with a confusing error instead of a clear "not
/// configured" one), so this section is `Option<HomeAssistantConfig>`
/// on `Config` -- present only when the user actually configured it.
#[derive(Debug, Clone, Deserialize)]
pub struct HomeAssistantConfig {
    /// e.g. "https://ha.example.com" or "http://homeassistant.local:8123".
    pub url: String,
    /// Long-lived access token (HA profile -> Security -> Long-Lived
    /// Access Tokens). Kept out of CLI flags/argv on purpose -- both
    /// shell history and /proc/<pid>/cmdline (readable by every
    /// process running as this user) would leak it.
    pub token: String,
    /// Restrict voice control to these entity ids (e.g.
    /// ["light.living_room", "lock.front_door"]). Empty/absent means
    /// no restriction -- every entity HA reports is controllable.
    #[serde(default)]
    pub allowed_entities: Vec<String>,
}

/// BlueBubbles REST API credentials -- see `router::bluebubbles`. Ported
/// from the `~/.agents/skills/bluebubbles` OmaPilot skill (send-only,
/// verified working against a real server) rather than starting fresh.
/// No `#[derive(Default)]`, same reasoning as `HomeAssistantConfig`:
/// there's no sensible default `server_url`/`password`, so this section
/// is `Option<BlueBubblesConfig>` -- present only when configured.
#[derive(Debug, Clone, Deserialize)]
pub struct BlueBubblesConfig {
    /// e.g. "http://192.168.1.50:1234" -- the Mac running BlueBubbles
    /// Server, from that app's own settings.
    pub server_url: String,
    /// Server password, also from the BlueBubbles Server app's settings.
    /// Kept out of CLI flags/argv for the same reason as HA's token --
    /// see `HomeAssistantConfig::token`.
    pub password: String,
    /// Manual spoken-name -> chat GUID overrides, checked before the
    /// dynamic lookup in `router::bluebubbles` (which resolves a name
    /// against the Mac's real Contacts app via `GET /api/v1/contact` and
    /// finds or creates a thread automatically -- see that module's
    /// docs). Only needed for an alias ("mom" for someone Contacts has
    /// under a different name) or someone not in Contacts at all. Find a
    /// chat's GUID in the BlueBubbles Server app's chat list / logs, or
    /// from an incoming-message webhook payload's `data.chats[0].guid`.
    #[serde(default)]
    pub contacts: std::collections::HashMap<String, String>,
}

/// Telegram, as your own user account (MTProto, not the Bot API -- see
/// `router::telegram`'s module docs for why). `api_id`/`api_hash` are a
/// developer credential pair from https://my.telegram.org/apps -- they
/// identify the *client application*, not you; the same pair could be
/// shipped in any grammers-based binary. The actual per-account login
/// (phone number, code, optional 2FA password) happens once via
/// `omarchy-novad setup telegram-auth` and is persisted to
/// `session_path`, not stored here.
#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub api_id: i32,
    pub api_hash: String,
    /// Where the logged-in session (a SQLite file holding the
    /// authorization key) is persisted -- see `router::telegram`'s
    /// `session_path()`. Losing this file means logging in again;
    /// leaking it is as sensitive as leaking your Telegram password, so
    /// it's created mode 600, same as config.toml itself.
    #[serde(default = "default_telegram_session_path")]
    pub session_path: PathBuf,
}

fn default_telegram_session_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad")
        .join("telegram.session")
}

/// Spotify OAuth (PKCE) config + stored tokens -- see
/// `router::spotify`. Unlike `HomeAssistantConfig`, this section is
/// partly *written* by omarchy-novad itself, not just read: the one-
/// time `omarchy-novad setup spotify-auth` command fills in
/// `refresh_token`/`access_token`/`expires_at` after the user
/// completes the browser consent flow, and every subsequent token
/// refresh rewrites them -- see `spotify::save_tokens`. `client_id`
/// is the only field the user sets by hand (from
/// developer.spotify.com/dashboard); everything else starts absent.
// TODO(spotify): router::spotify (the `omarchy-novad setup
// spotify-auth` command and the MediaControl/EXTERNAL handler that
// reads this) hasn't landed yet -- Config.spotify and
// save_spotify_tokens are wired into the schema and persistence layer
// but nothing constructs/reads them yet. Remove this allow once that
// lands.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
pub struct SpotifyConfig {
    pub client_id: String,
    /// Loopback redirect URI registered on the Spotify app (Spotify
    /// requires an explicit loopback IP literal, not "localhost" --
    /// see `router::spotify::validate_redirect_uri`). Defaults to
    /// `http://127.0.0.1:9876/callback` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Unix timestamp (seconds) the access token expires at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}

/// OmaPilot voice-assistant integration -- see `router::omapilot`. No
/// `#[derive(Default)]`: unlike `HomeAssistantConfig`/`BlueBubblesConfig`
/// there's no missing-credential reason to require this section, but a
/// silently-defaulted "on" would start feeding transcripts into a
/// third-party plugin (possibly a tool-capable, auto-approving agent --
/// see `router::omapilot`'s module docs) without the user ever having
/// opted in. `Option<OmaPilotConfig>` on `Config`, same shape as the
/// other integrations, means it does nothing at all until configured.
#[derive(Debug, Clone, Deserialize)]
pub struct OmaPilotConfig {
    /// Try OmaPilot when `Intent::External`/`Coding` need a handoff and
    /// OpenClaw is unavailable (not installed, gateway down, timed
    /// out) or unconfigured. OpenClaw is always tried first when it's
    /// on `PATH` -- it returns a real synchronous reply for the popup
    /// to show; OmaPilot's `askText` (see `router::omapilot`) doesn't,
    /// it only reports whether the handoff itself succeeded.
    #[serde(default)]
    pub fallback: bool,
    /// Recognize `direct_target_prefix` at the start of any wake-word
    /// utterance and route straight to OmaPilot via `askText`, bypassing
    /// classification (and `fallback`'s OpenClaw-first ordering)
    /// entirely -- e.g. "hey jarvis, pilot: what's the capital of
    /// France" with the default prefix.
    #[serde(default)]
    pub direct_target: bool,
    /// Case-insensitive; a trailing `:` or `,` (with surrounding
    /// whitespace) is stripped along with the prefix itself. Only
    /// checked when `direct_target` is true.
    #[serde(default = "default_direct_target_prefix")]
    pub direct_target_prefix: String,
    /// Quickshell plugin target id `omarchy-shell` dispatches
    /// `askText` to. Override only if OmaPilot is ever installed under
    /// a different id.
    #[serde(default = "default_omapilot_plugin_id")]
    pub plugin_id: String,
}

impl Default for OmaPilotConfig {
    fn default() -> Self {
        Self {
            fallback: false,
            direct_target: false,
            direct_target_prefix: default_direct_target_prefix(),
            plugin_id: default_omapilot_plugin_id(),
        }
    }
}

fn default_direct_target_prefix() -> String {
    "pilot".to_string()
}

fn default_omapilot_plugin_id() -> String {
    "io.github.spencerbull.omapilot".to_string()
}

/// OpenClaw's own gateway credentials (`~/.config/openclaw-novad.env`,
/// same file `openclaw-handoff` already sources) aren't duplicated
/// here -- this section is only for the one thing that's genuinely
/// novad-specific: how *this machine* gets a fresh `openclaw tui`
/// session past the gateway's device-pairing requirement. See
/// `router::openclaw::continue_in_herdr`'s module docs for why that's
/// even necessary.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OpenClawConfig {
    /// Run once, after opening an interactive Herdr session, if the
    /// gateway reports a pending device-pairing request -- e.g. `ssh
    /// archpocket "kubectl exec -n openclaw deploy/openclaw -- openclaw
    /// devices approve --latest"`, if that's where your gateway
    /// actually runs. Fully user-authored on purpose: only you know
    /// what network path can reach your gateway's admin surface (a
    /// k3s pod, a different host, a local loopback -- see
    /// `router::openclaw`'s module docs for why the credentials this
    /// server ships with can't do this themselves). `None` (the
    /// default) means never attempt it -- device pairing, if needed,
    /// is left for you to approve by hand.
    ///
    /// Only needed once in practice: once a device is approved it
    /// stays paired for future sessions from the same machine, so this
    /// is a bootstrap/recovery aid, not something that runs on every
    /// launch.
    #[serde(default)]
    pub approve_device_command: Option<String>,
}

/// A locally-run Kokoro TTS HTTP server (see tts-server/) -- see
/// `crate::tts`'s module docs for why this is a sidecar process novad
/// only ever talks to over HTTP, same relationship `[serve]`'s
/// `classify_base_url` has to the LLM serve instance. Not
/// `Option<...>`: like `ServeConfig`, there's a sensible default
/// (a server on the loopback interface, default port), and it only
/// ever does anything when `omarchy-novad converse` is actually run --
/// an unconfigured, unused section shouldn't need an explicit opt-in
/// the way a third-party credential (BlueBubbles, Telegram) does.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// Base URL of the running TTS server.
    pub serve_url: String,
    /// Kokoro voice id -- see tts-server/README.md for the full list
    /// bundled in `voices-v1.0.bin`.
    pub voice: String,
}

impl Default for TtsConfig {
    fn default() -> Self {
        Self {
            serve_url: "http://127.0.0.1:8421".to_string(),
            voice: "af_nova".to_string(),
        }
    }
}

/// `~/.config/omarchy-novad/config.toml`.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad")
        .join("config.toml")
}

/// Loads the config file, or `Config::default()` if it doesn't exist.
/// A malformed file is a hard error rather than a silent fallback --
/// silently ignoring a typo'd config (especially one holding a token
/// meant to actually take effect) is worse than refusing to start.
pub fn load() -> anyhow::Result<Config> {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| anyhow::anyhow!("parse {path:?}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(anyhow::anyhow!("read {path:?}: {e}")),
    }
}

/// Writes `spotify`'s token fields into `[spotify]` in the config
/// file, in place -- preserves every other section, comment, and the
/// user's own formatting (see `toml_edit` in Cargo.toml for why this
/// isn't a full `Config` round-trip). Creates the file (with a
/// `[spotify]` table and mode 600, since it's about to hold a token)
/// if it doesn't exist yet.
#[allow(dead_code)] // not called until router::spotify lands -- see SpotifyConfig's own TODO
pub fn save_spotify_tokens(spotify: &SpotifyConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| anyhow::anyhow!("parse {path:?} for editing: {e}"))?;

    if !doc.contains_key("spotify") {
        doc["spotify"] = toml_edit::table();
    }
    let table = doc["spotify"]
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[spotify] in {path:?} is not a table"))?;

    table["client_id"] = toml_edit::value(spotify.client_id.clone());
    match &spotify.redirect_uri {
        Some(v) => table["redirect_uri"] = toml_edit::value(v.clone()),
        None => {
            table.remove("redirect_uri");
        }
    }
    match &spotify.refresh_token {
        Some(v) => table["refresh_token"] = toml_edit::value(v.clone()),
        None => {
            table.remove("refresh_token");
        }
    }
    match &spotify.access_token {
        Some(v) => table["access_token"] = toml_edit::value(v.clone()),
        None => {
            table.remove("access_token");
        }
    }
    match spotify.expires_at {
        Some(v) => table["expires_at"] = toml_edit::value(v as i64),
        None => {
            table.remove("expires_at");
        }
    }

    let was_new = !path.exists();
    std::fs::write(&path, doc.to_string())?;
    if was_new {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    Ok(())
}

/// CLI flag wins if `Some`/non-default; otherwise falls back to the
/// config file's value. Used for `String`/`PathBuf`-ish fields where
/// clap's own default makes "the user didn't pass this" and "the user
/// explicitly passed the default value" indistinguishable -- see each
/// call site in main.rs for which default that is.
pub fn resolve_str(flag: String, flag_default: &str, from_config: &str) -> String {
    if flag != flag_default {
        flag
    } else {
        from_config.to_string()
    }
}

/// Same idea as [`resolve_str`], for `Option<String>` flags with no
/// meaningful default to compare against -- `None` means "use the
/// config file's value," `Some` means the flag wins outright.
pub fn resolve_opt(flag: Option<String>, from_config: Option<String>) -> Option<String> {
    flag.or(from_config)
}
