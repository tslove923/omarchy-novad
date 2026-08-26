//! Send iMessages via a self-hosted BlueBubbles server. Started as a port
//! of the `~/.agents/skills/bluebubbles` OmaPilot skill (`SKILL.md`/
//! `send.sh`) -- that skill's send path was verified working against a
//! real server, so the HTTP call itself is unchanged. Contact resolution
//! is new: the skill required every contact to be hand-mapped to a raw
//! chat GUID because it never verified a name lookup existed. One does:
//! `GET /api/v1/contact` returns the real macOS Contacts entries
//! (verified live against a real server -- see `fetch_contacts`), so a
//! spoken name can resolve to a real person's address instead of
//! requiring the GUID up front.
//!
//! Always requires confirmation (see `router::ConfirmKind::Message` and
//! `router::route`'s `Intent::Message` arm) -- texting a real person is a
//! much higher-stakes, harder-to-undo action than toggling a light or
//! running a read-only shell command, so unlike `HomeAssistant` this
//! never sends straight from the classifier's output.
//!
//! ## Existing thread vs. new conversation
//!
//! Once a name resolves to an address, sending needs to know whether a
//! 1:1 thread with that address already exists:
//! - If yes, `POST /api/v1/message/text` (chatGuid + text) -- the
//!   already-verified path.
//! - If no, `POST /api/v1/chat/new` (addresses + message) instead. This
//!   is a genuinely different endpoint, not a fallback hack: verified
//!   live that on modern macOS (Big Sur+), BlueBubbles' server itself
//!   rejects a chat/new call with no message ("A message is required
//!   when creating chats on macOS Big Sur or newer!") -- there's no such
//!   thing as creating an empty conversation first and sending into it
//!   later, matching how Messages.app itself only creates a real chat
//!   entity the moment the first message actually sends. So "start a
//!   conversation with someone" and "send them a message" are the same
//!   action when no thread exists yet, not two steps.
//!
//! `[bluebubbles.contacts]` (config.rs) still exists as a manual
//! override -- checked first, before the dynamic lookup below -- for
//! aliases ("mom" for someone Contacts has under their legal name) or
//! for anyone not in the Mac's Contacts app at all. Its value is a
//! chat GUID directly, same as before.

use std::collections::HashMap;
use std::time::Duration;

use crate::config::BlueBubblesConfig;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How many of the most recent chats to search for an existing 1:1
/// thread with a resolved contact. Not exhaustive -- searching the full
/// history (users can easily have 1000+ chats) on every voice command
/// would add real latency for a case that only matters for someone with
/// a very old, since-gone-quiet 1:1 thread. Falling through to creating
/// a new chat in that rare case is a fine outcome (see the module docs'
/// "Existing thread vs. new conversation" section) -- it just starts a
/// fresh thread alongside the buried one, not a wrong send.
const CHAT_SEARCH_LIMIT: u32 = 1000;

/// Verbs that precede a contact name in a natural message command.
/// Checked longest-first so "send a message to" doesn't get cut short by
/// a naive "send " match leaving "a message to sarah ..." looking like
/// the contact name is "a".
const LEADING_PHRASES: &[&str] = &[
    "send a message to ",
    "send an imessage to ",
    "send a text to ",
    "send imessage to ",
    "send message to ",
    "send text to ",
    "imessage ",
    "message ",
    "text ",
    "tell ",
];

/// Words that separate the contact name from the message body when
/// present -- checked longest-first for the same reason as
/// `LEADING_PHRASES`.
const CONNECTORS: &[&str] = &[" that says ", " saying that ", " saying ", " that "];

/// The single-word verbs among `LEADING_PHRASES` (as opposed to the
/// multi-word ones, e.g. "send a message to ") -- kept separate because
/// fuzzy-matching a whole phrase against ASR noise is a much higher-
/// dimensional problem than fuzzy-matching one leading word, so only
/// these get the fuzzy fallback below.
const SINGLE_WORD_TRIGGERS: &[&str] = &["text", "message", "tell", "imessage"];

/// Plain O(n*m) Levenshtein (edit) distance -- fine at the lengths this
/// is ever called with (single words, a handful of characters each).
/// Used to catch ASR mis-hearings of the leading command verb: observed
/// live, voxtype transcribed "text Jessica is this working?" as "Tax is
/// this working." -- no exact prefix match, and the classifier read
/// "tax" as MEMORY_RETURN since it has no reason to associate that word
/// with messaging.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate().take(n + 1) {
        row[0] = i;
    }
    for j in 0..=m {
        d[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
        }
    }
    d[n][m]
}

/// How many edits to tolerate before a word stops plausibly being a
/// mis-hearing of a trigger of this length and starts just being a
/// different word -- scales with length since 2 edits on a 3-letter
/// word changes it completely, but is a minor ASR slip on a longer one.
fn max_fuzzy_distance(trigger_len: usize) -> usize {
    match trigger_len {
        0..=3 => 1,
        4..=6 => 2,
        _ => 3,
    }
}

/// Whether `word` is a plausible ASR mis-hearing of one of
/// `SINGLE_WORD_TRIGGERS` -- e.g. "tax"/"tex" for "text". An exact match
/// (distance 0) returns `false` here -- that's already handled by
/// `LEADING_PHRASES`'s plain prefix check, which this only supplements.
fn fuzzy_message_trigger(word: &str) -> bool {
    let word = word.to_lowercase();
    SINGLE_WORD_TRIGGERS.iter().any(|&trigger| {
        let dist = levenshtein(&word, trigger);
        dist > 0 && dist <= max_fuzzy_distance(trigger.len())
    })
}

/// Recovery check, mirroring `media_control::looks_like_media_command` /
/// `home_assistant::looks_like_home_assistant_command`'s shape: does
/// `text`'s leading word(s) look like a MESSAGE trigger verb (exact
/// `LEADING_PHRASES` match, or a fuzzy single-word one -- see
/// `fuzzy_message_trigger`)? Deliberately stricter than `parse_command`
/// itself, whose final fallback treats *any* two-or-more-word phrase as
/// a valid "name + message" pair -- that's fine once intent is already
/// known to be Message, but far too permissive as a standalone "does
/// this look like a message" detector, which is what deciding whether
/// to *believe* it's a Message (see `pipeline.rs`'s classifier-recovery
/// use of this) actually needs. Only real verb evidence counts here.
pub fn looks_like_message_command(text: &str) -> bool {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    if LEADING_PHRASES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    trimmed
        .split_whitespace()
        .next()
        .map(|w| fuzzy_message_trigger(&alphanumeric_only(w)))
        .unwrap_or(false)
}

fn alphanumeric_only(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Parsed voice command: who to message and what to say.
struct ParsedMessage {
    name: String,
    text: String,
}

/// Parse a natural-language message command into (contact name, message
/// text). Handles the common phrasings directly; anything genuinely
/// ambiguous falls back to "first word is the name, rest is the
/// message" -- simple and predictable rather than a full NLP parser, the
/// same trade-off `home_assistant::parse_command` makes.
fn parse_command(arg: &str) -> Option<ParsedMessage> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();

    let rest = LEADING_PHRASES
        .iter()
        .find_map(|p| lower.strip_prefix(p).map(|_| &trimmed[p.len()..]))
        .or_else(|| fuzzy_strip_leading_verb(trimmed))
        .unwrap_or(trimmed);

    if rest.trim().is_empty() {
        return None;
    }

    let rest_lower = rest.to_lowercase();
    for connector in CONNECTORS {
        if let Some(pos) = rest_lower.find(connector) {
            let name = rest[..pos].trim();
            let text = rest[pos + connector.len()..].trim();
            if !name.is_empty() && !text.is_empty() {
                return Some(ParsedMessage {
                    name: name.to_string(),
                    text: text.to_string(),
                });
            }
        }
    }

    // No connector found: first word is the name, everything else is
    // the message (e.g. "sarah running late" -> "sarah" / "running late").
    let mut words = rest.split_whitespace();
    let name = words.next()?.to_string();
    let text: String = words.collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    Some(ParsedMessage { name, text })
}

/// Fallback for when no `LEADING_PHRASES` prefix matched exactly:
/// checks whether `trimmed`'s first word is a plausible mis-hearing of
/// a known trigger verb (see `fuzzy_message_trigger`) and, if so, strips
/// it the same way an exact match would -- e.g. "tax jessica is this
/// working" -> "jessica is this working", so the fuzzy-matched word
/// doesn't go on to get parsed as the contact's *name* instead of the
/// verb it actually was.
fn fuzzy_strip_leading_verb(trimmed: &str) -> Option<&str> {
    let first_word = trimmed.split_whitespace().next()?;
    if fuzzy_message_trigger(&alphanumeric_only(first_word)) {
        let rest = trimmed[first_word.len()..].trim_start();
        if !rest.is_empty() {
            return Some(rest);
        }
    }
    None
}

/// One real Contacts-app entry, flattened to what we need: a display
/// name and every phone number / email that could show up as a chat
/// participant address.
#[derive(Debug, Clone)]
struct ContactRecord {
    display_name: String,
    addresses: Vec<String>,
}

/// `GET /api/v1/contact` -- the Mac's real Contacts app entries, synced
/// through BlueBubbles. Verified live against a real server: returns
/// `displayName`, `phoneNumbers[].address`, `emails[].address`.
fn fetch_contacts(cfg: &BlueBubblesConfig) -> Result<Vec<ContactRecord>, String> {
    let url = format!(
        "{}/api/v1/contact?password={}",
        cfg.server_url.trim_end_matches('/'),
        cfg.password
    );
    let response: serde_json::Value = ureq::get(&url)
        .timeout(REQUEST_TIMEOUT)
        .call()
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    let entries = response
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "unexpected /api/v1/contact response shape".to_string())?;

    let mut contacts = Vec::new();
    for entry in entries {
        let display_name = entry
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if display_name.is_empty() {
            continue;
        }
        let mut addresses = Vec::new();
        for field in ["phoneNumbers", "emails"] {
            if let Some(arr) = entry.get(field).and_then(|v| v.as_array()) {
                for a in arr {
                    if let Some(addr) = a.get("address").and_then(|v| v.as_str()) {
                        addresses.push(addr.to_string());
                    }
                }
            }
        }
        if !addresses.is_empty() {
            contacts.push(ContactRecord {
                display_name,
                addresses,
            });
        }
    }
    Ok(contacts)
}

/// Normalize a phone number to its last 10 digits for comparison --
/// formatting varies wildly between what Contacts stores ("+1 (503)
/// 989-5976") and what shows up as a chat's `chatIdentifier`/participant
/// address ("+15039895976"). Emails are compared verbatim (lowercased)
/// instead, handled by the caller.
fn normalize_phone(s: &str) -> String {
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() > 10 {
        digits[digits.len() - 10..].to_string()
    } else {
        digits
    }
}

fn addresses_match(a: &str, b: &str) -> bool {
    if a.contains('@') || b.contains('@') {
        a.eq_ignore_ascii_case(b)
    } else {
        normalize_phone(a) == normalize_phone(b)
    }
}

/// Find the single best contact match for a spoken name. Tries, in
/// order: exact full-name match, exact first-name match, substring
/// match -- stopping at the first tier that yields exactly one
/// candidate. An empty or multi-candidate result at every tier is an
/// error naming the ambiguity rather than guessing, same principle as
/// `home_assistant`'s entity matcher.
fn find_best_contact_match<'a>(
    name: &str,
    contacts: &'a [ContactRecord],
) -> Result<&'a ContactRecord, String> {
    let query = name.trim().to_lowercase();
    if query.is_empty() {
        return Err("no name given".to_string());
    }

    let exact: Vec<&ContactRecord> = contacts
        .iter()
        .filter(|c| c.display_name.to_lowercase() == query)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0]);
    }

    let first_name: Vec<&ContactRecord> = contacts
        .iter()
        .filter(|c| {
            c.display_name
                .split_whitespace()
                .next()
                .map(|f| f.to_lowercase() == query)
                .unwrap_or(false)
        })
        .collect();
    if first_name.len() == 1 {
        return Ok(first_name[0]);
    }

    let contains: Vec<&ContactRecord> = contacts
        .iter()
        .filter(|c| c.display_name.to_lowercase().contains(&query))
        .collect();
    if contains.len() == 1 {
        return Ok(contains[0]);
    }

    let ambiguous = if !first_name.is_empty() {
        &first_name
    } else {
        &contains
    };
    if ambiguous.is_empty() {
        Err(format!("No contact named {name:?} found"))
    } else {
        let names: Vec<&str> = ambiguous.iter().map(|c| c.display_name.as_str()).collect();
        Err(format!(
            "Multiple contacts match {name:?}: {}",
            names.join(", ")
        ))
    }
}

/// Where a message should go once a contact's resolved: an existing 1:1
/// thread (send into it) or a brand-new one (create it, atomically, by
/// sending the first message -- see the module docs).
enum Destination {
    ExistingChat(String),
    NewChat(String),
}

/// A name resolved to somewhere to send a message.
struct Resolved {
    display_name: String,
    destination: Destination,
}

/// `POST /api/v1/chat/query` -- search recent chats (see
/// `CHAT_SEARCH_LIMIT`) for a 1:1 (single-participant) thread matching
/// any of `addresses`. Returns the most recently active match, if any.
fn find_existing_direct_chat(
    addresses: &[String],
    cfg: &BlueBubblesConfig,
) -> Result<Option<String>, String> {
    let url = format!(
        "{}/api/v1/chat/query?password={}",
        cfg.server_url.trim_end_matches('/'),
        cfg.password
    );
    let body = serde_json::json!({
        "with": ["participants", "lastmessage"],
        "sort": "lastmessage",
        "limit": CHAT_SEARCH_LIMIT,
    });
    let response: serde_json::Value = ureq::post(&url)
        .timeout(REQUEST_TIMEOUT)
        .send_json(body)
        .map_err(|e| e.to_string())?
        .into_json()
        .map_err(|e| e.to_string())?;

    let chats = response
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "unexpected /api/v1/chat/query response shape".to_string())?;

    for chat in chats {
        let participants = chat
            .get("participants")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if participants.len() != 1 {
            continue; // group chat -- never a valid "direct message" target.
        }
        let Some(addr) = participants[0].get("address").and_then(|v| v.as_str()) else {
            continue;
        };
        if addresses.iter().any(|a| addresses_match(a, addr)) {
            if let Some(guid) = chat.get("guid").and_then(|v| v.as_str()) {
                return Ok(Some(guid.to_string()));
            }
        }
    }
    Ok(None)
}

/// Resolve a spoken contact name to somewhere a message can actually be
/// sent. `[bluebubbles.contacts]` (a manual chat-GUID alias) wins if
/// present; otherwise looks the name up in the Mac's real Contacts
/// (`fetch_contacts`) and searches for an existing direct thread
/// (`find_existing_direct_chat`), falling back to "start a new
/// conversation" with that contact's first address if none is found.
fn resolve(name: &str, cfg: &BlueBubblesConfig) -> Result<Resolved, String> {
    if let Some((display_name, guid)) = resolve_manual_alias(name, &cfg.contacts) {
        tracing::debug!("[router:bluebubbles] {name:?} -> manual alias {display_name:?}");
        return Ok(Resolved {
            display_name: display_name.to_string(),
            destination: Destination::ExistingChat(guid.to_string()),
        });
    }

    tracing::debug!("[router:bluebubbles] resolving {name:?} against {}", cfg.server_url);
    let contacts = fetch_contacts(cfg).inspect_err(|e| {
        tracing::warn!("[router:bluebubbles] fetch_contacts failed: {e}");
    })?;
    tracing::debug!("[router:bluebubbles] fetched {} contacts", contacts.len());
    let contact = find_best_contact_match(name, &contacts).inspect_err(|e| {
        tracing::debug!("[router:bluebubbles] no unique match for {name:?}: {e}");
    })?;
    let existing = find_existing_direct_chat(&contact.addresses, cfg).inspect_err(|e| {
        tracing::warn!("[router:bluebubbles] find_existing_direct_chat failed: {e}");
    })?;
    let destination = match existing {
        Some(guid) => {
            tracing::debug!(
                "[router:bluebubbles] matched {:?} -> existing chat {guid}",
                contact.display_name
            );
            Destination::ExistingChat(guid)
        }
        None => {
            tracing::debug!(
                "[router:bluebubbles] matched {:?} -> no existing thread, will start a new one",
                contact.display_name
            );
            Destination::NewChat(contact.addresses[0].clone())
        }
    };
    Ok(Resolved {
        display_name: contact.display_name.clone(),
        destination,
    })
}

/// Look up a spoken contact name in `[bluebubbles.contacts]`. Exact
/// (case-insensitive) match only -- this is a manual override, not a
/// fuzzy directory; see `find_best_contact_match` for the dynamic path.
fn resolve_manual_alias<'a>(
    name: &str,
    contacts: &'a HashMap<String, String>,
) -> Option<(&'a str, &'a str)> {
    let lower = name.trim().to_lowercase();
    contacts
        .iter()
        .find(|(k, _)| k.to_lowercase() == lower)
        .map(|(k, v)| (k.as_str(), v.as_str()))
}

/// Short recipient/context header shown above the editable message body
/// in the confirm popup, e.g. `Text Sarah` or `Text Sarah (new
/// conversation)` when no thread exists yet -- worth calling out
/// explicitly since starting a fresh conversation is a more noticeable
/// action than adding to one already in progress. The destination
/// address itself is never shown, just who it resolved to.
fn recipient_label(resolved: &Resolved) -> String {
    let suffix = match resolved.destination {
        Destination::ExistingChat(_) => "",
        Destination::NewChat(_) => " (new conversation)",
    };
    format!("Text {}{}", resolved.display_name, suffix)
}

/// What [`prepare`] hands back to the popup: a short recipient header
/// (see `recipient_label`) and the parsed message body -- shown as an
/// editable box (see `popup::PopupState::editable`) so the user can fix
/// up whatever the wake-word/transcribe/classify pipeline got wrong
/// before it actually sends. Whatever's in that box when Approve is
/// clicked comes back as `run_confirmed`'s `edited_body`.
pub struct PreparedMessage {
    pub label: String,
    pub body: String,
}

/// Called by `router::route` for `Intent::Message`. Parses the command
/// and resolves the contact, but does NOT send anything yet -- returns
/// the confirmation preview on success, or a user-facing error message
/// if the command couldn't be understood or the contact couldn't be
/// resolved. `router::route` turns `Ok` into
/// `RouteResult::NeedsConfirmation` and `Err` into a failed
/// `RouteResult::Done`, matching how every other router error surfaces.
pub fn prepare(arg: &str, cfg: &BlueBubblesConfig) -> Result<PreparedMessage, String> {
    let Some(parsed) = parse_command(arg) else {
        tracing::warn!("[router:bluebubbles] prepare: couldn't parse a name/message out of {arg:?}");
        return Err(format!("Couldn't tell who to message or what to say: {arg:?}"));
    };
    tracing::debug!(
        "[router:bluebubbles] prepare: parsed name={:?} text_len={}",
        parsed.name,
        parsed.text.len()
    );
    let resolved = resolve(&parsed.name, cfg)?;
    Ok(PreparedMessage {
        label: recipient_label(&resolved),
        body: parsed.text,
    })
}

/// Called by `router::run_confirmed` after the user approves the popup.
/// Re-parses `arg` and re-resolves the contact (same as `prepare` saw --
/// re-resolving rather than threading state through the confirm
/// round-trip, same pattern `terminal::run` uses), then actually sends.
/// `edited_body`, when present, replaces whatever `parse_command` pulled
/// out of `arg` as the message text -- this is what the user may have
/// typed into the popup's editable box before approving (see
/// `PreparedMessage`'s docs); an empty/whitespace-only edit is treated
/// as an error rather than silently sending nothing or falling back to
/// the original parse, since either of those would be surprising.
pub fn run_confirmed(arg: &str, edited_body: Option<&str>, cfg: &BlueBubblesConfig) -> (bool, String) {
    tracing::debug!("[router:bluebubbles] run_confirmed: {arg:?} edited_body={edited_body:?}");
    let Some(mut parsed) = parse_command(arg) else {
        tracing::warn!("[router:bluebubbles] run_confirmed: couldn't parse a name/message out of {arg:?}");
        return (
            false,
            format!("Couldn't tell who to message or what to say: {arg:?}"),
        );
    };
    if let Some(body) = edited_body {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            tracing::warn!("[router:bluebubbles] run_confirmed: edited body is empty, refusing to send");
            return (false, "Message is empty -- not sending".to_string());
        }
        parsed.text = trimmed.to_string();
    }
    let resolved = match resolve(&parsed.name, cfg) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("[router:bluebubbles] run_confirmed: resolve failed: {e}");
            return (false, e);
        }
    };

    let result = match &resolved.destination {
        Destination::ExistingChat(guid) => {
            tracing::debug!("[router:bluebubbles] sending into existing chat {guid}");
            send(guid, &parsed.text, cfg)
        }
        Destination::NewChat(address) => {
            tracing::debug!("[router:bluebubbles] starting a new chat with {address}");
            create_and_send(address, &parsed.text, cfg)
        }
    };
    match result {
        Ok(()) => {
            tracing::info!("[router:bluebubbles] sent to {}", resolved.display_name);
            (true, format!("Sent to {}", resolved.display_name))
        }
        Err(e) => {
            tracing::warn!(
                "[router:bluebubbles] send to {} failed: {e}",
                resolved.display_name
            );
            (
                false,
                format!("Failed to send to {}: {e}", resolved.display_name),
            )
        }
    }
}

/// POST to BlueBubbles' `/api/v1/message/text`, mirroring
/// `~/.agents/skills/bluebubbles/send.sh` exactly (same endpoint, same
/// JSON body shape, same query-string auth). `private-api` requires the
/// BlueBubbles Private API helper installed on the Mac; if that's not
/// set up, the server rejects the request rather than silently
/// downgrading, so a failure here may mean switching to `apple-script`
/// is needed -- see SKILL.md.
fn send(chat_guid: &str, text: &str, cfg: &BlueBubblesConfig) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/message/text?password={}",
        cfg.server_url.trim_end_matches('/'),
        cfg.password
    );
    // Body field is "message", not "text" -- despite the endpoint being
    // named .../message/text and the field being called `text` in both
    // this module's original reference (the `~/.agents/skills/
    // bluebubbles` skill's send.sh, presumably written against an older
    // server version) and BlueBubbles' own webhook example docs. Found
    // live: the server's real validation error was "The message field
    // must be present (but can be empty)." -- `create_and_send` below
    // already uses "message" for /api/v1/chat/new; this just brings
    // `send` in line with what the server actually validates today.
    let body = serde_json::json!({
        "chatGuid": chat_guid,
        "message": text,
        "method": "private-api",
    });
    post_expect_success(&url, body)
}

/// POST to BlueBubbles' `/api/v1/chat/new` -- starts a brand-new
/// conversation with `address` by sending `text` as its first message.
/// Verified live: this endpoint requires both `addresses` and `message`
/// together on modern macOS (see the module docs' "Existing thread vs.
/// new conversation" section) -- there's no separate "create an empty
/// chat" step to call first.
fn create_and_send(address: &str, text: &str, cfg: &BlueBubblesConfig) -> Result<(), String> {
    let url = format!(
        "{}/api/v1/chat/new?password={}",
        cfg.server_url.trim_end_matches('/'),
        cfg.password
    );
    let body = serde_json::json!({
        "addresses": [address],
        "message": text,
        "method": "private-api",
    });
    post_expect_success(&url, body)
}

fn post_expect_success(url: &str, body: serde_json::Value) -> Result<(), String> {
    // ureq treats any non-2xx as `Err(ureq::Error::Status(code, response))`
    // -- *not* as a normal `Ok(Response)` the JSON-body check below could
    // inspect. Observed live: BlueBubbles returned a plain HTTP 400 for a
    // rejected send, and the naive `.map_err(|e| e.to_string())` on that
    // collapsed it to just "status code 400", throwing away the JSON
    // `{"message": "..."}` body BlueBubbles actually sent explaining why
    // (e.g. the Private API helper not being enabled on the Mac -- see
    // this module's docs). `Error::Status` carries that response, so pull
    // the real message out of it before falling back to the bare code.
    let response = match ureq::post(url).timeout(REQUEST_TIMEOUT).send_json(body) {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let message = r
                .into_json::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from));
            return Err(message.unwrap_or_else(|| format!("HTTP {code}")));
        }
        Err(e) => return Err(e.to_string()),
    };

    let response: serde_json::Value = response.into_json().map_err(|e| e.to_string())?;
    // A 2xx response can *still* carry a `{"status": <code>}` >= 400 in
    // its own JSON body -- BlueBubbles doesn't do this for the send
    // endpoints this module calls today, as far as observed, but this
    // check predates that observation and costs nothing to keep as a
    // second line of defense.
    let status = response.get("status").and_then(|v| v.as_u64()).unwrap_or(0);
    if status >= 400 {
        let message = response
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(message.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manual_contacts() -> HashMap<String, String> {
        [("mom".to_string(), "iMessage;-;+15551234567".to_string())]
            .into_iter()
            .collect()
    }

    fn contact_records() -> Vec<ContactRecord> {
        vec![
            ContactRecord {
                display_name: "Sarah Jones".to_string(),
                addresses: vec!["+15559876543".to_string()],
            },
            ContactRecord {
                display_name: "Sarah Miller".to_string(),
                addresses: vec!["+15551112222".to_string()],
            },
            ContactRecord {
                display_name: "Andrew Heath".to_string(),
                addresses: vec!["+15037582384".to_string(), "andrew@example.com".to_string()],
            },
        ]
    }

    /// Live, read-only smoke test against the real BlueBubbles server --
    /// not part of the normal suite (`#[ignore]`), no fixtures. Exercises
    /// `fetch_contacts` + `resolve` for real to isolate whether the
    /// network layer even gets reached; sends nothing. Run with:
    ///   cargo test --release live_resolve_smoke -- --ignored --nocapture
    /// TEMP: remove once the silent-failure investigation is done.
    #[test]
    #[ignore]
    fn live_resolve_smoke() {
        let cfg = crate::config::load().expect("load config").bluebubbles.expect("[bluebubbles] configured");
        eprintln!("[live_resolve_smoke] fetching contacts from {}...", cfg.server_url);
        let contacts = fetch_contacts(&cfg).expect("fetch_contacts");
        eprintln!("[live_resolve_smoke] got {} contacts", contacts.len());

        let Ok(name) = std::env::var("BB_TEST_CONTACT") else {
            eprintln!("[live_resolve_smoke] BB_TEST_CONTACT not set -- stopping after fetch_contacts");
            return;
        };
        let contact = find_best_contact_match(&name, &contacts).expect("find_best_contact_match");
        eprintln!(
            "[live_resolve_smoke] matched: {} ({} address(es))",
            contact.display_name,
            contact.addresses.len()
        );
        let existing = find_existing_direct_chat(&contact.addresses, &cfg).expect("find_existing_direct_chat");
        eprintln!("[live_resolve_smoke] existing direct chat: {existing:?}");
        let resolved = resolve(&name, &cfg).expect("resolve");
        eprintln!("[live_resolve_smoke] recipient label: {}", recipient_label(&resolved));
    }

    #[test]
    fn parses_plain_verb_prefix() {
        let p = parse_command("text mom I'm running late").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "I'm running late");
    }

    #[test]
    fn parses_fuzzy_mis_heard_verb() {
        // Observed live: voxtype transcribed "text Jessica is this
        // working?" as "Tax is this working." -- "tax" should still
        // strip as the verb, not get treated as the contact's name.
        let p = parse_command("tax jessica is this working").unwrap();
        assert_eq!(p.name, "jessica");
        assert_eq!(p.text, "is this working");

        let p = parse_command("tex sarah I'm running late").unwrap();
        assert_eq!(p.name, "sarah");
        assert_eq!(p.text, "I'm running late");
    }

    #[test]
    fn does_not_fuzzy_match_unrelated_leading_words() {
        // "mom running late" relies on the *no-verb* fallback (see
        // falls_back_to_first_word_as_name_with_no_connector) --
        // "mom" must not itself get eaten as a fuzzy-matched verb.
        let p = parse_command("mom running late").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "running late");
    }

    #[test]
    fn looks_like_message_command_catches_fuzzy_verbs_but_not_unrelated_text() {
        assert!(looks_like_message_command("text jessica is this working"));
        assert!(looks_like_message_command("tax jessica is this working"));
        assert!(looks_like_message_command("Tax is this working."));
        assert!(looks_like_message_command(
            "send a message to mom saying hi"
        ));
        assert!(!looks_like_message_command("turn on the living room lights"));
        assert!(!looks_like_message_command("what's my tax rate this year"));
    }

    #[test]
    fn parses_saying_connector() {
        let p = parse_command("text sarah saying I'll be there in 10").unwrap();
        assert_eq!(p.name, "sarah");
        assert_eq!(p.text, "I'll be there in 10");
    }

    #[test]
    fn parses_send_a_message_to_phrasing() {
        let p = parse_command("send a message to mom saying dinner's ready").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "dinner's ready");
    }

    #[test]
    fn parses_tell_verb() {
        let p = parse_command("tell sarah I'm on my way").unwrap();
        assert_eq!(p.name, "sarah");
        assert_eq!(p.text, "I'm on my way");
    }

    #[test]
    fn falls_back_to_first_word_as_name_with_no_connector() {
        // Classifier's argument extraction may have already stripped
        // the leading verb.
        let p = parse_command("mom running late").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "running late");
    }

    #[test]
    fn empty_or_name_only_returns_none() {
        assert!(parse_command("").is_none());
        assert!(parse_command("text").is_none());
        assert!(parse_command("text mom").is_none());
    }

    #[test]
    fn resolve_manual_alias_is_case_insensitive() {
        let c = manual_contacts();
        let (name, guid) = resolve_manual_alias("MOM", &c).unwrap();
        assert_eq!(name, "mom");
        assert_eq!(guid, "iMessage;-;+15551234567");
    }

    #[test]
    fn resolve_manual_alias_unknown_name_is_none() {
        let c = manual_contacts();
        assert!(resolve_manual_alias("grandpa", &c).is_none());
    }

    #[test]
    fn find_best_contact_match_exact_full_name() {
        let contacts = contact_records();
        let m = find_best_contact_match("Andrew Heath", &contacts).unwrap();
        assert_eq!(m.display_name, "Andrew Heath");
    }

    #[test]
    fn find_best_contact_match_unique_first_name() {
        let contacts = contact_records();
        let m = find_best_contact_match("andrew", &contacts).unwrap();
        assert_eq!(m.display_name, "Andrew Heath");
    }

    #[test]
    fn find_best_contact_match_ambiguous_first_name_errors_listing_candidates() {
        let contacts = contact_records();
        let err = find_best_contact_match("sarah", &contacts).unwrap_err();
        assert!(err.contains("Sarah Jones"));
        assert!(err.contains("Sarah Miller"));
    }

    #[test]
    fn find_best_contact_match_no_match_errors() {
        let contacts = contact_records();
        let err = find_best_contact_match("grandpa", &contacts).unwrap_err();
        assert!(err.contains("grandpa"));
    }

    #[test]
    fn addresses_match_normalizes_phone_formatting() {
        assert!(addresses_match("+1 (503) 758-2384", "+15037582384"));
        assert!(addresses_match("5037582384", "+15037582384"));
        assert!(!addresses_match("+15037582384", "+15037582385"));
    }

    #[test]
    fn addresses_match_emails_are_case_insensitive_not_digit_normalized() {
        assert!(addresses_match("Andrew@Example.com", "andrew@example.com"));
        assert!(!addresses_match("andrew@example.com", "+15037582384"));
    }

    #[test]
    fn recipient_label_flags_new_conversations() {
        let resolved = Resolved {
            display_name: "Andrew Heath".to_string(),
            destination: Destination::NewChat("+15037582384".to_string()),
        };
        assert_eq!(recipient_label(&resolved), "Text Andrew Heath (new conversation)");

        let resolved = Resolved {
            display_name: "Andrew Heath".to_string(),
            destination: Destination::ExistingChat("SMS;-;+15037582384".to_string()),
        };
        assert_eq!(recipient_label(&resolved), "Text Andrew Heath");
    }

    #[test]
    fn run_confirmed_rejects_an_empty_edited_body() {
        let cfg = BlueBubblesConfig {
            server_url: "http://example.invalid".to_string(),
            password: "x".to_string(),
            contacts: manual_contacts(),
        };
        let (ok, message) = run_confirmed("text mom running late", Some("   "), &cfg);
        assert!(!ok);
        assert_eq!(message, "Message is empty -- not sending");
    }
}
