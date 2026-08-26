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
    #[allow(dead_code)] // not read until router::spotify lands -- see SpotifyConfig's own TODO
    #[serde(default)]
    pub spotify: Option<SpotifyConfig>,
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
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            device: "GPU".to_string(),
            model_id: "novad-local".to_string(),
            port: 8420,
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
