//! Home Assistant REST API integration. Port of nova-npu's
//! `ai/commands/home_assistant.py`: entity discovery (cached at first
//! use), a fuzzy spoken-name-to-entity_id matcher, natural-language
//! command parsing (turn on/off, toggle, set temperature/brightness,
//! lock/unlock, status check), and compound-command splitting
//! ("turn on downstairs and turn off upstairs lights").
//!
//! Deliberately talks to HA's plain REST API with its own long-lived
//! token (`[home_assistant]` in config.toml -- see config.rs) rather
//! than reusing the `hass` Omarchy shell plugin already installed on
//! this system: that plugin's `hass-bridge` process is spawned by
//! its own Service.qml as a child process communicating over that
//! process's stdin/stdout, not reachable via any socket/port from
//! outside the shell -- there's no existing seam for omarchy-novad to
//! plug into without new IPC work on the shell's side. A REST client
//! with a second token is a few hundred lines of self-contained code;
//! the alternative is designing and shipping new plugin IPC just to
//! avoid it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::HomeAssistantConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
struct EntityInfo {
    state: String,
    friendly_name: String,
    domain: String,
}

/// Cached entity snapshot, loaded once per process lifetime on first
/// use (matches nova's own `_cache_loaded` global -- HA entity lists
/// don't change often enough within one omarchy-novad run to justify
/// re-fetching every command).
struct EntityCache {
    entities: HashMap<String, EntityInfo>,
    loaded: bool,
    last_error: String,
}

static ENTITY_CACHE: Mutex<Option<EntityCache>> = Mutex::new(None);

fn headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("Authorization", format!("Bearer {token}")),
        ("Content-Type", "application/json".to_string()),
    ]
}

fn api_get(url: &str, path: &str, token: &str) -> Result<serde_json::Value, String> {
    let full = format!("{}/api/{path}", url.trim_end_matches('/'));
    let mut req = ureq::get(&full).timeout(REQUEST_TIMEOUT);
    for (k, v) in headers(token) {
        req = req.set(k, &v);
    }
    req.call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())
}

fn api_post(url: &str, path: &str, token: &str, data: &serde_json::Value) -> Result<(), String> {
    let full = format!("{}/api/{path}", url.trim_end_matches('/'));
    let mut req = ureq::post(&full).timeout(REQUEST_TIMEOUT);
    for (k, v) in headers(token) {
        req = req.set(k, &v);
    }
    req.send_json(data.clone()).map_err(|e| e.to_string())?;
    Ok(())
}

fn load_entities(url: &str, token: &str) -> HashMap<String, EntityInfo> {
    let mut guard = ENTITY_CACHE.lock().unwrap();
    if let Some(cache) = guard.as_ref() {
        if cache.loaded {
            return cache.entities.clone();
        }
    }

    match api_get(url, "states", token) {
        Ok(serde_json::Value::Array(states)) => {
            let mut entities = HashMap::new();
            for s in states {
                let Some(eid) = s.get("entity_id").and_then(|v| v.as_str()) else {
                    continue;
                };
                let state = s
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let friendly_name = s
                    .get("attributes")
                    .and_then(|a| a.get("friendly_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(eid)
                    .to_string();
                let domain = eid.split('.').next().unwrap_or("").to_string();
                entities.insert(
                    eid.to_string(),
                    EntityInfo {
                        state,
                        friendly_name,
                        domain,
                    },
                );
            }
            tracing::debug!("[router:home_assistant] cached {} entities", entities.len());
            *guard = Some(EntityCache {
                entities: entities.clone(),
                loaded: true,
                last_error: String::new(),
            });
            entities
        }
        Ok(_) => {
            *guard = Some(EntityCache {
                entities: HashMap::new(),
                loaded: false,
                last_error: "unexpected response shape from /api/states".to_string(),
            });
            HashMap::new()
        }
        Err(e) => {
            tracing::warn!("[router:home_assistant] entity fetch failed: {e}");
            *guard = Some(EntityCache {
                entities: HashMap::new(),
                loaded: false,
                last_error: e,
            });
            HashMap::new()
        }
    }
}

fn last_entity_error() -> Option<String> {
    let guard = ENTITY_CACHE.lock().unwrap();
    guard
        .as_ref()
        .filter(|c| !c.last_error.is_empty())
        .map(|c| c.last_error.clone())
}

fn is_allowed(eid: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed.iter().any(|a| a.eq_ignore_ascii_case(eid))
}

/// Location/device qualifier words preserved during fuzzy matching so
/// "upstairs" doesn't accidentally match "downstairs" -- see nova's
/// own `qualifier_words` set.
const QUALIFIER_WORDS: &[&str] = &[
    "upstairs",
    "downstairs",
    "garage",
    "front",
    "back",
    "left",
    "right",
    "kitchen",
    "living",
    "bedroom",
    "hall",
    "office",
    "porch",
    "nursery",
];

fn qualifier_match(name_lower: &str, eid: &str, friendly_name: &str) -> bool {
    let required: Vec<&str> = name_lower
        .split_whitespace()
        .filter(|w| QUALIFIER_WORDS.contains(w))
        .collect();
    if required.is_empty() {
        return true;
    }
    let hay = format!("{} {}", eid.to_lowercase(), friendly_name.to_lowercase());
    required.iter().all(|q| hay.contains(q))
}

/// Fuzzy-matches a spoken entity name to a HA entity_id: exact
/// friendly-name match, then substring, then word-overlap. Mirrors
/// nova's `_find_entity` three-tier fallback.
fn find_entity(
    name: &str,
    domain_hint: &str,
    url: &str,
    token: &str,
    allowed: &[String],
) -> Option<String> {
    let entities = load_entities(url, token);
    if entities.is_empty() {
        return None;
    }
    let name_lower = name.trim().to_lowercase();

    // Tier 1: exact friendly_name match.
    for (eid, info) in &entities {
        if !is_allowed(eid, allowed) || !qualifier_match(&name_lower, eid, &info.friendly_name) {
            continue;
        }
        if info.friendly_name.to_lowercase() == name_lower
            && (domain_hint.is_empty() || info.domain == domain_hint)
        {
            return Some(eid.clone());
        }
    }

    // Tier 2: substring match, unique candidate only.
    let mut candidates = Vec::new();
    for (eid, info) in &entities {
        if !is_allowed(eid, allowed) || !qualifier_match(&name_lower, eid, &info.friendly_name) {
            continue;
        }
        if !domain_hint.is_empty() && info.domain != domain_hint {
            continue;
        }
        let fn_lower = info.friendly_name.to_lowercase();
        if fn_lower.contains(&name_lower) || name_lower.contains(&fn_lower) {
            candidates.push(eid.clone());
        }
    }
    if candidates.len() == 1 {
        return candidates.into_iter().next();
    }

    // Tier 3: word-overlap, highest score wins (ties broken by
    // HashMap iteration order -- same nondeterminism nova's own dict
    // iteration had, not a regression).
    let name_words: std::collections::HashSet<&str> = name_lower.split_whitespace().collect();
    let mut best: Option<(String, usize)> = None;
    for (eid, info) in &entities {
        if !is_allowed(eid, allowed) || !qualifier_match(&name_lower, eid, &info.friendly_name) {
            continue;
        }
        if !domain_hint.is_empty() && info.domain != domain_hint {
            continue;
        }
        let fn_lower = info.friendly_name.to_lowercase();
        let fn_words: std::collections::HashSet<&str> = fn_lower.split_whitespace().collect();
        let overlap = name_words.intersection(&fn_words).count();
        if overlap > best.as_ref().map_or(0, |(_, s)| *s) {
            best = Some((eid.clone(), overlap));
        }
    }
    best.filter(|(_, score)| *score >= 1).map(|(eid, _)| eid)
}

#[derive(Debug, Clone)]
enum ServiceData {
    None,
    Temperature(i64),
    Brightness(i64),
}

/// Parses "turn off living room lights" -> ("turn_off", "living room
/// lights", ServiceData::None), etc. Mirrors nova's `_parse_ha_command`.
fn parse_command(arg: &str) -> (&'static str, String, ServiceData) {
    let lower = arg.trim().trim_end_matches(['?', '.', '!']).to_lowercase();

    if let Some(rest) = lower
        .strip_prefix("did i close ")
        .or_else(|| lower.strip_prefix("did i shut "))
        .or_else(|| lower.strip_prefix("did i lock "))
        .or_else(|| lower.strip_prefix("did i turn off "))
    {
        return ("check", strip_leading_the(rest), ServiceData::None);
    }

    if let Some(rest) = lower.strip_prefix("turn on ") {
        return ("turn_on", strip_leading_the(rest), ServiceData::None);
    }
    if let Some(rest) = lower.strip_prefix("turn off ") {
        return ("turn_off", strip_leading_the(rest), ServiceData::None);
    }
    if let Some(rest) = lower.strip_prefix("toggle ") {
        return ("toggle", strip_leading_the(rest), ServiceData::None);
    }
    if let Some(rest) = lower.strip_prefix("lock ") {
        return ("lock", strip_leading_the(rest), ServiceData::None);
    }
    if let Some(rest) = lower.strip_prefix("unlock ") {
        return ("unlock", strip_leading_the(rest), ServiceData::None);
    }

    // "set <entity> [to] <number>" or "set <entity> [to] <number>%"
    // Distinguishing set_temperature vs. set_brightness the way nova
    // did (brightness needs the word "brightness"/"bright" present;
    // otherwise a bare "set the entity to 72" is temperature).
    if let Some(rest) = lower.strip_prefix("set ") {
        if let Some((entity, value)) = split_trailing_number(rest) {
            let is_brightness = entity.contains("brightness") || entity.contains("bright");
            let entity_clean = strip_leading_the(
                entity
                    .replace("brightness", "")
                    .replace("bright", "")
                    .trim(),
            );
            if is_brightness {
                return (
                    "set_brightness",
                    entity_clean,
                    ServiceData::Brightness(value),
                );
            }
            return (
                "set_temperature",
                entity_clean,
                ServiceData::Temperature(value),
            );
        }
    }
    // "<entity> brightness [to] <number>" without a leading "set".
    if lower.contains("brightness") || lower.contains("bright") {
        if let Some((entity, value)) = split_trailing_number(&lower) {
            let entity_clean = strip_leading_the(
                entity
                    .replace("brightness", "")
                    .replace("bright", "")
                    .trim(),
            );
            return (
                "set_brightness",
                entity_clean,
                ServiceData::Brightness(value),
            );
        }
    }

    if let Some(rest) = lower
        .strip_prefix("check ")
        .or_else(|| lower.strip_prefix("status "))
        .or_else(|| lower.strip_prefix("state "))
        .or_else(|| lower.strip_prefix("is "))
    {
        let rest = strip_trailing_state_word(rest);
        return ("check", strip_leading_the(&rest), ServiceData::None);
    }

    // Fallback: "<verb> <entity>".
    if let Some((verb, rest)) = lower.split_once(' ') {
        return (
            known_verb_or_check(verb),
            rest.to_string(),
            ServiceData::None,
        );
    }

    ("check", lower, ServiceData::None)
}

fn strip_leading_the(s: &str) -> String {
    s.trim()
        .strip_prefix("the ")
        .unwrap_or(s.trim())
        .to_string()
}

/// Trailing " on"/" off"/" open"/" closed" that a status-check phrase
/// sometimes carries ("is the garage door open") -- stripped so the
/// remaining text is just the entity name.
fn strip_trailing_state_word(s: &str) -> String {
    let s = s.trim();
    for suffix in [" on", " off", " open", " closed"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            return stripped.to_string();
        }
    }
    s.to_string()
}

/// Splits "the thermostat to 72" -> ("the thermostat", 72), tolerating
/// an optional "to" before the number and an optional trailing "%".
fn split_trailing_number(s: &str) -> Option<(&str, i64)> {
    let s = s.trim().trim_end_matches('%');
    let (entity, num_str) = s.rsplit_once(' ')?;
    let value: i64 = num_str.parse().ok()?;
    let entity = entity.strip_suffix(" to").unwrap_or(entity);
    Some((entity.trim(), value))
}

/// `parse_command`'s fallback branch needs a `&'static str` action
/// name but only has a runtime `&str` slice from the split utterance
/// -- maps it onto one of the known action literals instead (falls
/// back to "check" for anything unrecognized, same as nova's own
/// fallback returning the raw first word verbatim would have hit
/// `_ACTION_SERVICES.get(action, {})`'s empty-dict default).
fn known_verb_or_check(verb: &str) -> &'static str {
    match verb {
        "turn_on" | "on" => "turn_on",
        "turn_off" | "off" => "turn_off",
        "toggle" => "toggle",
        "lock" => "lock",
        "unlock" => "unlock",
        "set" => "set",
        _ => "check",
    }
}

/// Splits "turn on downstairs and turn off upstairs lights" into
/// independent sub-commands. Mirrors nova's `_split_compound_commands`.
fn split_compound(arg: &str) -> Vec<String> {
    let text = arg.trim();
    if text.is_empty() {
        return Vec::new();
    }

    let normalized = text
        .replace(", and then ", " and ")
        .replace(" and then ", " and ")
        .replace(" then ", " and ")
        .replace(", ", " and ")
        .replace("; ", " and ");
    let parts: Vec<&str> = normalized
        .split(" and ")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() <= 1 {
        return vec![text.to_string()];
    }

    fn starts_with_verb(s: &str) -> bool {
        let lower = s.to_lowercase();
        [
            "turn on ",
            "turn off ",
            "toggle ",
            "lock ",
            "unlock ",
            "set ",
            "check ",
            "status ",
            "state ",
            "is ",
            "did i ",
        ]
        .iter()
        .any(|v| lower.starts_with(v))
    }
    fn leading_action_phrase(s: &str) -> String {
        let lower = s.to_lowercase();
        for v in ["turn on", "turn off", "toggle", "lock", "unlock", "set"] {
            if lower.starts_with(v) {
                return v.to_string();
            }
        }
        String::new()
    }

    let mut commands = Vec::new();
    let mut pending_prefix = String::new();
    for part in &parts {
        if starts_with_verb(part) {
            commands.push(part.to_string());
            pending_prefix = leading_action_phrase(part);
        } else if !pending_prefix.is_empty() {
            commands.push(format!("{pending_prefix} {part}"));
        } else {
            // Not enough structure to split safely -- same bail-out
            // nova's own splitter used.
            return vec![text.to_string()];
        }
    }
    if commands.is_empty() {
        vec![text.to_string()]
    } else {
        commands
    }
}

fn action_service(action: &str, domain: &str) -> Option<&'static str> {
    match (action, domain) {
        ("turn_on", "light") => Some("light/turn_on"),
        ("turn_on", "switch") => Some("switch/turn_on"),
        ("turn_on", "fan") => Some("fan/turn_on"),
        ("turn_on", "media_player") => Some("media_player/turn_on"),
        ("turn_on", _) => Some("homeassistant/turn_on"),
        ("turn_off", "light") => Some("light/turn_off"),
        ("turn_off", "switch") => Some("switch/turn_off"),
        ("turn_off", "fan") => Some("fan/turn_off"),
        ("turn_off", "media_player") => Some("media_player/turn_off"),
        ("turn_off", _) => Some("homeassistant/turn_off"),
        ("toggle", "light") => Some("light/toggle"),
        ("toggle", "switch") => Some("switch/toggle"),
        ("toggle", "fan") => Some("fan/toggle"),
        ("toggle", _) => Some("homeassistant/toggle"),
        ("lock", _) => Some("lock/lock"),
        ("unlock", _) => Some("lock/unlock"),
        ("set_temperature", _) => Some("climate/set_temperature"),
        ("set_brightness", _) => Some("light/turn_on"),
        _ => None,
    }
}

fn execute_single(arg: &str, cfg: &HomeAssistantConfig) -> (bool, String) {
    let (action, entity_name, service_data) = parse_command(arg);
    tracing::debug!(
        "[router:home_assistant] action={action} entity={entity_name:?} data={service_data:?}"
    );

    let domain_hint = match action {
        "lock" | "unlock" => "lock",
        "set_temperature" => "climate",
        _ => "",
    };

    let Some(eid) = find_entity(
        &entity_name,
        domain_hint,
        &cfg.url,
        &cfg.token,
        &cfg.allowed_entities,
    ) else {
        return match last_entity_error() {
            Some(e) => (false, format!("Home Assistant unavailable: {e}")),
            None => (false, format!("Entity not found: {entity_name}")),
        };
    };

    if action == "check" {
        let entities = load_entities(&cfg.url, &cfg.token);
        let info = entities.get(&eid);
        let state = info.map(|i| i.state.as_str()).unwrap_or("unknown");
        let fname = info.map(|i| i.friendly_name.as_str()).unwrap_or(&eid);
        return (true, format!("{fname}: {state}"));
    }

    let domain = eid.split('.').next().unwrap_or("");
    let entities = load_entities(&cfg.url, &cfg.token);
    let fname = entities
        .get(&eid)
        .map(|i| i.friendly_name.clone())
        .unwrap_or_else(|| eid.clone());

    let Some(service) = action_service(action, domain) else {
        return (false, format!("Unsupported action '{action}' for {domain}"));
    };

    let mut svc_data = serde_json::json!({ "entity_id": eid });
    match service_data {
        ServiceData::Brightness(pct) => {
            let pct = pct.clamp(0, 100);
            svc_data["brightness"] = serde_json::json!(pct * 255 / 100);
        }
        ServiceData::Temperature(t) => {
            svc_data["temperature"] = serde_json::json!(t);
        }
        ServiceData::None => {}
    }

    tracing::debug!("[router:home_assistant] calling {service} with {svc_data}");
    match api_post(
        &cfg.url,
        &format!("services/{service}"),
        &cfg.token,
        &svc_data,
    ) {
        Ok(()) => (true, format!("{}: {fname}", action.replace('_', " "))),
        Err(e) => (false, format!("HA call failed: {e}")),
    }
}

/// Executes a Home Assistant voice command (single or compound). Same
/// entry point shape as nova's `home_assistant()`.
pub fn run(arg: &str, cfg: &HomeAssistantConfig) -> (bool, String) {
    if arg.trim().is_empty() {
        return (false, "No command provided".to_string());
    }

    let commands = split_compound(arg);
    if commands.len() <= 1 {
        return execute_single(arg, cfg);
    }

    tracing::debug!(
        "[router:home_assistant] split compound command into {} steps: {commands:?}",
        commands.len()
    );
    let results: Vec<(bool, String)> = commands.iter().map(|c| execute_single(c, cfg)).collect();
    let failures: Vec<&str> = results
        .iter()
        .filter(|(ok, _)| !ok)
        .map(|(_, m)| m.as_str())
        .collect();
    let successes = results.iter().filter(|(ok, _)| *ok).count();

    if !failures.is_empty() {
        if successes > 0 {
            return (
                false,
                format!(
                    "{successes}/{} commands succeeded; failed: {}",
                    results.len(),
                    failures[0]
                ),
            );
        }
        return (false, failures[0].to_string());
    }
    (
        true,
        results
            .into_iter()
            .map(|(_, m)| m)
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Heuristic check for common Home Assistant device-control phrases --
/// used as a recovery check the same way
/// `media_control::looks_like_media_command` catches classifier
/// misclassifications for its own intent pair. Mirrors nova's
/// `looks_like_home_assistant_command`, with one deliberate change: a
/// device-*type* word ("lights", "switch", ...) is no longer required,
/// only a strong action verb prefix.
///
/// Nova's original required both. Real usage often names an area or
/// person instead of a device type ("turn on Sophie's room" --
/// observed live, misclassified as SYSTEM_CONTROL, then failed
/// system_control::run with "Unknown system control" since it's
/// neither a volume nor brightness phrase either -- see the README's
/// "Known classifier gaps"). This function's only caller
/// (`router::route`'s SystemControl arm) already ruled out
/// volume/brightness keywords and a media-transport phrase before
/// reaching here, so an unambiguous device-control verb like "turn
/// on"/"lock" is enough signal on its own -- unlike nova's original
/// scope, where this ran against *any* utterance with no such
/// upstream filtering already applied. "check"/"status"/"is " stay
/// excluded from the action list on purpose: those prefixes are too
/// generic on their own (e.g. "is that true") to trust without a
/// device-type word backing them up.
pub fn looks_like_home_assistant_command(arg: &str) -> bool {
    let lower = arg.trim().to_lowercase();
    if lower.is_empty() || lower.contains("spotify") {
        return false;
    }
    [
        "turn on ",
        "turn off ",
        "toggle ",
        "lock ",
        "unlock ",
        "set ",
    ]
    .iter()
    .any(|v| lower.starts_with(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_turn_on_off() {
        assert_eq!(parse_command("turn on the living room lights").0, "turn_on");
        assert_eq!(
            parse_command("turn on the living room lights").1,
            "living room lights"
        );
        assert_eq!(parse_command("turn off garage light").0, "turn_off");
    }

    #[test]
    fn parses_lock_unlock() {
        assert_eq!(parse_command("lock the front door").0, "lock");
        assert_eq!(parse_command("unlock front door").0, "unlock");
    }

    #[test]
    fn parses_set_temperature() {
        let (action, entity, data) = parse_command("set the thermostat to 72");
        assert_eq!(action, "set_temperature");
        assert_eq!(entity, "thermostat");
        assert!(matches!(data, ServiceData::Temperature(72)));
    }

    #[test]
    fn parses_set_brightness() {
        let (action, entity, data) = parse_command("set the lamp brightness to 40");
        assert_eq!(action, "set_brightness");
        assert!(entity.contains("lamp"));
        assert!(matches!(data, ServiceData::Brightness(40)));
    }

    #[test]
    fn parses_check() {
        assert_eq!(parse_command("check the garage door").0, "check");
        assert_eq!(
            parse_command("is the front door locked").1,
            "front door locked"
        );
    }

    #[test]
    fn splits_compound_commands() {
        let cmds = split_compound("turn on downstairs and turn off upstairs lights");
        assert_eq!(cmds, vec!["turn on downstairs", "turn off upstairs lights"]);
    }

    #[test]
    fn compound_split_is_noop_on_plain_command() {
        assert_eq!(
            split_compound("turn on the lights"),
            vec!["turn on the lights"]
        );
    }

    #[test]
    fn detects_home_assistant_phrases() {
        assert!(looks_like_home_assistant_command(
            "turn on the living room lights"
        ));
        assert!(looks_like_home_assistant_command("lock the front door"));
        assert!(!looks_like_home_assistant_command("play spotify"));
        assert!(!looks_like_home_assistant_command("what's the weather"));
    }
}
