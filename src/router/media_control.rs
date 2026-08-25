//! Media playback control via MPRIS D-Bus. Scoped port of nova-npu's
//! `ai/commands/media_control.py` + `integrations/mpris.py`: covers
//! play/pause/next/previous/status, the actions that need no
//! configuration. Nova's `play_track` (search-and-play a specific
//! song) fell back to the Spotify Web API when MPRIS alone couldn't
//! satisfy it -- that needs a configured `ai_spotify_*` client
//! id/secret nova had and novad doesn't have an equivalent for yet, so
//! "play <song>" here just sends a bare Play to the current player
//! rather than attempting a search.

use std::process::Command;

const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const OBJ_PATH: &str = "/org/mpris/MediaPlayer2";

// gdbus has no built-in timeout flag, unlike Python's
// subprocess.run(timeout=...). Every call site here is a single quick
// property read/method call on the session bus, so a hung D-Bus peer
// is an edge case, not the common path -- acceptable to skip a
// wait-with-timeout wrapper for it.
fn gdbus_call(dest: &str, path: &str, method: &str, extra_args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("gdbus");
    cmd.args([
        "call",
        "--session",
        "--dest",
        dest,
        "--object-path",
        path,
        "--method",
        method,
    ]);
    cmd.args(extra_args);
    match cmd.output() {
        Ok(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => {
            tracing::debug!(
                "[router:media] gdbus error: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
            None
        }
        Err(e) => {
            tracing::warn!("[router:media] gdbus not found: {e}");
            None
        }
    }
}

fn get_property(bus_name: &str, iface: &str, prop: &str) -> Option<String> {
    let raw = gdbus_call(
        bus_name,
        OBJ_PATH,
        "org.freedesktop.DBus.Properties.Get",
        &[&format!("'{iface}'"), &format!("'{prop}'")],
    )?;
    Some(strip_gvariant(&raw))
}

/// Strips gdbus's GVariant wrapper, e.g. `(<'Playing'>,)` -> `Playing`.
fn strip_gvariant(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix('(').unwrap_or(s);
    let s = s.strip_suffix(')').unwrap_or(s).trim_end_matches(',');
    let s = s.trim();
    let s = s
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(s);
    s.trim().trim_matches(|c| c == '\'' || c == '"').to_string()
}

fn active_players() -> Vec<String> {
    let Some(raw) = gdbus_call(
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus.ListNames",
        &[],
    ) else {
        return Vec::new();
    };
    // Output looks like (['name1', 'name2', ...],) -- pull out every
    // quoted name rather than parsing the tuple/list syntax properly.
    let mut names = Vec::new();
    let mut rest = raw.as_str();
    while let Some(start) = rest.find('\'') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('\'') else { break };
        let name = &rest[..end];
        if name.starts_with(MPRIS_PREFIX) {
            names.push(name.to_string());
        }
        rest = &rest[end + 1..];
    }
    names
}

fn pick_best_player(players: &[String]) -> Option<String> {
    if players.is_empty() {
        return None;
    }
    let mut paused = None;
    for p in players {
        match get_property(p, PLAYER_IFACE, "PlaybackStatus").as_deref() {
            Some("Playing") => return Some(p.clone()),
            Some("Paused") if paused.is_none() => paused = Some(p.clone()),
            _ => {}
        }
    }
    paused.or_else(|| players.first().cloned())
}

fn player_display_name(bus: &str) -> &str {
    bus.strip_prefix(MPRIS_PREFIX)
        .unwrap_or(bus)
        .split('.')
        .next()
        .unwrap_or(bus)
}

fn mpris_control(action: &str) -> (bool, String) {
    let players = active_players();
    if players.is_empty() {
        return (false, "No MPRIS media players found".to_string());
    }
    let Some(bus) = pick_best_player(&players) else {
        return (false, "No suitable MPRIS player found".to_string());
    };
    let name = player_display_name(&bus).to_string();

    if action == "status" {
        let status = get_property(&bus, PLAYER_IFACE, "PlaybackStatus")
            .unwrap_or_else(|| "Unknown".to_string());
        return (true, format!("{status} ({name})"));
    }

    let method = match action {
        "play" | "resume" => "Play",
        "pause" => "Pause",
        "play_pause" | "toggle" => "PlayPause",
        "next" => "Next",
        "previous" => "Previous",
        "stop" => "Stop",
        _ => return (false, format!("Unknown MPRIS action: {action}")),
    };

    match gdbus_call(&bus, OBJ_PATH, &format!("{PLAYER_IFACE}.{method}"), &[]) {
        Some(_) => (true, format!("{} \u{2192} {name}", capitalize(action))),
        None => (false, format!("MPRIS {action} failed on {name}")),
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Mirrors nova's `_parse_action`: natural language -> MPRIS action.
fn parse_action(arg: &str) -> &'static str {
    let text = arg.trim().to_lowercase();
    if text.is_empty() {
        return "status";
    }
    if text.starts_with("pause") || text.starts_with("stop") {
        return "pause";
    }
    if text.starts_with("resume") || text.starts_with("continue") || text.starts_with("unpause") {
        return "play";
    }
    if text.starts_with("next") || text.starts_with("skip") {
        return "next";
    }
    if text.starts_with("previous")
        || text.starts_with("prev")
        || text.starts_with("back")
        || text.starts_with("last track")
        || text.starts_with("go back")
    {
        return "previous";
    }
    if text.starts_with("toggle")
        || text.starts_with("play pause")
        || text.starts_with("play/pause")
    {
        return "play_pause";
    }
    if text.starts_with("what is playing")
        || text.starts_with("what's playing")
        || text.starts_with("status")
        || text.starts_with("current track")
        || text.starts_with("now playing")
        || text.starts_with("what song")
        || text.starts_with("what track")
    {
        return "status";
    }
    // "play <song>": no Spotify Web API fallback here (see module
    // docs), so treat it the same as a bare "play".
    "play"
}

pub fn run(argument: &str) -> (bool, String) {
    let action = parse_action(argument);
    tracing::debug!("[router:media] argument={argument:?} -> action={action}");
    mpris_control(action)
}
