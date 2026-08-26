//! Send Telegram messages, as your own account (MTProto via `grammers`,
//! not the HTTP Bot API -- see the Cargo.toml dependency comment for
//! why: a bot can't cold-DM an arbitrary contact, which "telegram
//! <name> ..." fundamentally needs, since Telegram blocks bots from
//! messaging someone who hasn't started a chat with them first).
//!
//! Shape mirrors `router::bluebubbles` closely on purpose -- parse a
//! command into name+text, resolve the name to somewhere to send,
//! `prepare`/`run_confirmed` split for the popup's Approve round-trip,
//! same fuzzy-leading-verb tolerance for ASR mis-hearings. The
//! structural difference: every grammers call is async (MTProto is a
//! real network protocol, not a REST endpoint `ureq` can call
//! synchronously), so each public function here spins up a short-lived
//! current-thread tokio runtime for the call's duration -- the same
//! bridge `main.rs`'s `Serve` command already uses to run async code
//! from `router::route`'s synchronous call site.
//!
//! ## Login
//!
//! One-time `omarchy-novad setup telegram-auth` (see main.rs) walks
//! through the real MTProto login (phone number, code, optional 2FA
//! password) and persists the resulting session to
//! `TelegramConfig::session_path` -- a SQLite file holding the
//! authorization key, not your password itself (Telegram never gives
//! third-party apps that). Every call here reopens that same session
//! file rather than logging in again; `connect` errors clearly if it's
//! missing or not yet authorized.
//!
//! ## Contact resolution -- and no "new conversation" distinction
//!
//! Telegram doesn't expose an equivalent of BlueBubbles'
//! `GET /api/v1/contact` at the friendly `grammers_client::Client`
//! level, so this calls the raw `contacts.getContacts` RPC directly
//! (`Client::invoke` -- documented by grammers as outside its semver
//! guarantee, not as unsafe or unsupported) for the full contact list,
//! same shape as BlueBubbles' `fetch_contacts`. Unlike BlueBubbles,
//! though, there's no second "does a thread already exist" step and no
//! separate endpoint for starting one: a contact's Telegram user id +
//! access_hash (both returned by `getContacts`) are already enough to
//! build a `PeerRef` and call `send_message` directly, whether or not
//! you've ever messaged them before -- Telegram's protocol doesn't
//! distinguish "reply into an existing chat" from "start a new one" the
//! way iMessage/BlueBubbles does, so there's nothing extra to warn
//! about in the confirmation preview either.

use grammers_client::{Client, SignInError};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use grammers_tl_types as tl;
use std::sync::Arc;

use crate::config::TelegramConfig;

/// Verbs that precede a contact name in a natural Telegram command.
/// Checked longest-first, same reasoning as `bluebubbles::LEADING_PHRASES`.
const LEADING_PHRASES: &[&str] = &[
    "send a telegram to ",
    "send a telegram message to ",
    "telegram message ",
    "telegram ",
];

/// Words that separate the contact name from the message body when
/// present -- identical set to `bluebubbles::CONNECTORS`.
const CONNECTORS: &[&str] = &[" that says ", " saying that ", " saying ", " that "];

/// The single-word verbs among `LEADING_PHRASES` eligible for fuzzy
/// matching (see `fuzzy_message_trigger`) -- just "telegram". "tg" is
/// deliberately excluded: at two characters, almost any short word
/// falls within a plausible edit distance of it, which would make the
/// fuzzy fallback fire on things that were never a Telegram command at
/// all. Multi-word phrases aren't fuzzy-matched either, same reasoning
/// as `bluebubbles::SINGLE_WORD_TRIGGERS`.
const SINGLE_WORD_TRIGGERS: &[&str] = &["telegram"];

/// Plain O(n*m) Levenshtein (edit) distance -- see
/// `bluebubbles::levenshtein`'s docs for why this exists at all (ASR
/// mis-hearings of the leading command verb). Duplicated rather than
/// shared: the trigger word sets differ enough per channel that a
/// shared helper would need to thread more through its signature than
/// it saves.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate().take(n + 1) {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
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

fn max_fuzzy_distance(trigger_len: usize) -> usize {
    match trigger_len {
        0..=3 => 1,
        4..=6 => 2,
        _ => 3,
    }
}

fn fuzzy_message_trigger(word: &str) -> bool {
    let word = word.to_lowercase();
    SINGLE_WORD_TRIGGERS.iter().any(|&trigger| {
        let dist = levenshtein(&word, trigger);
        dist > 0 && dist <= max_fuzzy_distance(trigger.len())
    })
}

fn alphanumeric_only(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Recovery check, same role as `bluebubbles::looks_like_message_command`
/// -- lets `pipeline.rs` recover a MEMORY_RETURN classification into
/// TELEGRAM when the original transcript's leading word looks like a
/// (possibly mis-heard) Telegram trigger, even though the classifier
/// picked a different intent.
pub fn looks_like_telegram_command(text: &str) -> bool {
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

/// Parsed voice command: who to message and what to say. Identical
/// shape and logic to `bluebubbles::parse_command` -- see that
/// function's docs.
struct ParsedMessage {
    name: String,
    text: String,
}

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

    let mut words = rest.split_whitespace();
    let name = words.next()?.to_string();
    let text: String = words.collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    Some(ParsedMessage { name, text })
}

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

/// One Telegram contact, flattened to what we need -- enough to build a
/// `PeerRef` directly (see the module docs' "Contact resolution"
/// section), no separate "does a chat already exist" lookup required.
#[derive(Debug)]
struct ContactRecord {
    display_name: String,
    id: i64,
    access_hash: i64,
}

/// Raw `contacts.getContacts` RPC -- see the module docs for why the
/// friendly `Client` API doesn't have a wrapped method for this.
/// `hash: 0` always requests the full list rather than "no changes
/// since"; the server-side caching that hash would enable isn't worth
/// the complexity for a call this infrequent (one per voice command).
async fn fetch_contacts(client: &Client) -> Result<Vec<ContactRecord>, String> {
    let response = client
        .invoke(&tl::functions::contacts::GetContacts { hash: 0 })
        .await
        .map_err(|e| e.to_string())?;

    let users = match response {
        tl::enums::contacts::Contacts::Contacts(c) => c.users,
        tl::enums::contacts::Contacts::NotModified => Vec::new(),
    };

    let mut contacts = Vec::new();
    for user in users {
        let tl::enums::User::User(u) = user else {
            continue; // tl::enums::User::Empty -- a deleted/inaccessible account
        };
        let display_name = match (&u.first_name, &u.last_name) {
            (Some(f), Some(l)) => format!("{f} {l}"),
            (Some(f), None) => f.clone(),
            (None, Some(l)) => l.clone(),
            (None, None) => continue,
        };
        // No access_hash means we have no authority to message this
        // user directly (Telegram didn't grant one) -- skip rather than
        // build a PeerRef that will just fail to send.
        let Some(access_hash) = u.access_hash else {
            continue;
        };
        contacts.push(ContactRecord {
            display_name,
            id: u.id,
            access_hash,
        });
    }
    Ok(contacts)
}

/// Find the single best contact match for a spoken name. Same
/// tiered-matching shape as `bluebubbles::find_best_contact_match` --
/// exact full-name, then unique first-name, then substring, erroring
/// with the candidate list on ambiguity rather than guessing.
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
        Err(format!("No Telegram contact named {name:?} found"))
    } else {
        let names: Vec<&str> = ambiguous.iter().map(|c| c.display_name.as_str()).collect();
        Err(format!(
            "Multiple Telegram contacts match {name:?}: {}",
            names.join(", ")
        ))
    }
}

/// A name resolved to somewhere a message can actually be sent.
struct Resolved {
    display_name: String,
    peer: PeerRef,
}

async fn resolve(name: &str, client: &Client) -> Result<Resolved, String> {
    let contacts = fetch_contacts(client).await.inspect_err(|e| {
        tracing::warn!("[router:telegram] fetch_contacts failed: {e}");
    })?;
    tracing::debug!("[router:telegram] fetched {} contacts", contacts.len());
    let contact = find_best_contact_match(name, &contacts).inspect_err(|e| {
        tracing::debug!("[router:telegram] no unique match for {name:?}: {e}");
    })?;
    let peer = PeerRef {
        id: PeerId::user(contact.id)
            .ok_or_else(|| format!("Telegram returned an invalid user id for {name:?}"))?,
        auth: PeerAuth::from_hash(contact.access_hash),
    };
    Ok(Resolved {
        display_name: contact.display_name.clone(),
        peer,
    })
}

/// What [`prepare`] hands back to the popup: a short recipient header
/// and the parsed message body -- shown as an editable box, same as
/// `bluebubbles::PreparedMessage`.
pub struct PreparedMessage {
    pub label: String,
    pub body: String,
}

/// Opens the session and confirms it's actually logged in, without
/// sending anything. Returns a user-facing error (not a panic) for
/// every way this can fail: no session file yet, session file exists
/// but was never completed, or Telegram itself is unreachable --
/// `omarchy-novad setup telegram-auth` is the fix for the first two.
async fn connect(cfg: &TelegramConfig) -> Result<Client, String> {
    let session = Arc::new(
        SqliteSession::open(&cfg.session_path)
            .await
            .map_err(|e| format!("opening Telegram session {:?}: {e}", cfg.session_path))?,
    );
    let SenderPool { runner, handle, .. } = SenderPool::new(Arc::clone(&session), cfg.api_id);
    let client = Client::new(handle);
    tokio::spawn(runner.run());

    let authorized = client
        .is_authorized()
        .await
        .map_err(|e| format!("connecting to Telegram: {e}"))?;
    if !authorized {
        return Err(
            "Not logged into Telegram -- run `omarchy-novad setup telegram-auth` first".to_string(),
        );
    }
    Ok(client)
}

/// Runs an async Telegram call from `router::route`'s synchronous call
/// site -- a fresh current-thread runtime per call, same bridge
/// `main.rs`'s `Serve` command uses for `serve::run`. Not a shared
/// long-lived runtime: these are human-paced, one-at-a-time voice
/// commands, not a hot path, so the overhead of spinning one up per
/// call is not worth avoiding at the cost of the complexity a
/// process-wide async runtime would add to an otherwise fully
/// synchronous daemon.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to start a tokio runtime for a Telegram call")
        .block_on(fut)
}

/// Called by `router::route` for `Intent::Telegram`. Parses the command
/// and resolves the contact, but does NOT send anything yet -- mirrors
/// `bluebubbles::prepare` exactly (see that function's docs).
pub fn prepare(arg: &str, cfg: &TelegramConfig) -> Result<PreparedMessage, String> {
    let Some(parsed) = parse_command(arg) else {
        tracing::warn!("[router:telegram] prepare: couldn't parse a name/message out of {arg:?}");
        return Err(format!(
            "Couldn't tell who to message or what to say: {arg:?}"
        ));
    };
    tracing::debug!(
        "[router:telegram] prepare: parsed name={:?} text_len={}",
        parsed.name,
        parsed.text.len()
    );
    block_on(async {
        let client = connect(cfg).await?;
        let resolved = resolve(&parsed.name, &client).await?;
        Ok(PreparedMessage {
            label: format!("Telegram {}", resolved.display_name),
            body: parsed.text,
        })
    })
}

/// Called by `router::run_confirmed` after the user approves the popup.
/// Mirrors `bluebubbles::run_confirmed` exactly, including the empty-
/// edited-body refusal -- see that function's docs.
pub fn run_confirmed(arg: &str, edited_body: Option<&str>, cfg: &TelegramConfig) -> (bool, String) {
    tracing::debug!("[router:telegram] run_confirmed: {arg:?} edited_body={edited_body:?}");
    let Some(mut parsed) = parse_command(arg) else {
        tracing::warn!(
            "[router:telegram] run_confirmed: couldn't parse a name/message out of {arg:?}"
        );
        return (
            false,
            format!("Couldn't tell who to message or what to say: {arg:?}"),
        );
    };
    if let Some(body) = edited_body {
        let trimmed = body.trim();
        if trimmed.is_empty() {
            tracing::warn!(
                "[router:telegram] run_confirmed: edited body is empty, refusing to send"
            );
            return (false, "Message is empty -- not sending".to_string());
        }
        parsed.text = trimmed.to_string();
    }

    block_on(async {
        let client = match connect(cfg).await {
            Ok(c) => c,
            Err(e) => return (false, e),
        };
        let resolved = match resolve(&parsed.name, &client).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[router:telegram] run_confirmed: resolve failed: {e}");
                return (false, e);
            }
        };
        match client
            .send_message(resolved.peer, parsed.text.as_str())
            .await
        {
            Ok(_) => {
                tracing::info!("[router:telegram] sent to {}", resolved.display_name);
                (true, format!("Sent to {}", resolved.display_name))
            }
            Err(e) => {
                tracing::warn!(
                    "[router:telegram] send to {} failed: {e}",
                    resolved.display_name
                );
                (
                    false,
                    format!("Failed to send to {}: {e}", resolved.display_name),
                )
            }
        }
    })
}

/// One-time interactive login -- called by `omarchy-novad setup
/// telegram-auth` (see main.rs, which owns the actual terminal
/// prompting). Takes the prompting/code/password as plain closures
/// rather than doing I/O here directly, so this function stays testable
/// and main.rs stays the only place that touches stdin/stdout.
pub fn login(
    cfg: &TelegramConfig,
    prompt_phone: impl FnOnce() -> String,
    prompt_code: impl FnOnce() -> String,
    prompt_password: impl FnOnce(Option<&str>) -> String,
) -> Result<String, String> {
    block_on(async {
        let session = Arc::new(
            SqliteSession::open(&cfg.session_path)
                .await
                .map_err(|e| format!("opening Telegram session {:?}: {e}", cfg.session_path))?,
        );
        let SenderPool { runner, handle, .. } = SenderPool::new(Arc::clone(&session), cfg.api_id);
        let client = Client::new(handle);
        tokio::spawn(runner.run());

        if client.is_authorized().await.map_err(|e| e.to_string())? {
            let me = client.get_me().await.map_err(|e| e.to_string())?;
            return Ok(format!(
                "Already logged in as {}",
                me.first_name().unwrap_or("(unknown)")
            ));
        }

        let phone = prompt_phone();
        let token = client
            .request_login_code(&phone, &cfg.api_hash)
            .await
            .map_err(|e| format!("requesting login code: {e}"))?;
        let code = prompt_code();
        let user = match client.sign_in(&token, &code).await {
            Ok(user) => user,
            Err(SignInError::PasswordRequired(password_token)) => {
                let password = prompt_password(password_token.hint());
                client
                    .check_password(password_token, password.trim())
                    .await
                    .map_err(|e| format!("checking 2FA password: {e}"))?
            }
            Err(SignInError::SignUpRequired) => {
                return Err(
                    "This phone number has no Telegram account yet -- sign up with an \
                     official Telegram app first, then re-run this"
                        .to_string(),
                );
            }
            Err(SignInError::InvalidCode) => {
                return Err("That code was rejected -- re-run and double check it".to_string());
            }
            Err(e) => return Err(format!("signing in: {e}")),
        };

        Ok(format!(
            "Logged in as {}",
            user.first_name().unwrap_or("(unknown)")
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_verb_prefix() {
        let p = parse_command("telegram sarah are you free tonight").unwrap();
        assert_eq!(p.name, "sarah");
        assert_eq!(p.text, "are you free tonight");
    }

    #[test]
    fn parses_saying_connector() {
        let p = parse_command("telegram mom saying I'll be there in 10").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "I'll be there in 10");
    }

    #[test]
    fn parses_send_a_telegram_to_phrasing() {
        let p = parse_command("send a telegram to mom saying dinner's ready").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "dinner's ready");
    }

    #[test]
    fn falls_back_to_first_word_as_name_with_no_connector() {
        let p = parse_command("sarah running late").unwrap();
        assert_eq!(p.name, "sarah");
        assert_eq!(p.text, "running late");
    }

    #[test]
    fn empty_or_name_only_returns_none() {
        assert!(parse_command("").is_none());
        assert!(parse_command("telegram").is_none());
        assert!(parse_command("telegram sarah").is_none());
    }

    #[test]
    fn parses_fuzzy_mis_heard_verb() {
        // "telegram" mis-heard, same category of ASR slip observed live
        // for BlueBubbles' "text" -> "tax" (see bluebubbles.rs).
        let p = parse_command("telegran sarah is this working").unwrap();
        assert_eq!(p.name, "sarah");
        assert_eq!(p.text, "is this working");
    }

    #[test]
    fn does_not_fuzzy_match_unrelated_leading_words() {
        let p = parse_command("mom running late").unwrap();
        assert_eq!(p.name, "mom");
        assert_eq!(p.text, "running late");
    }

    #[test]
    fn looks_like_telegram_command_catches_fuzzy_verbs_but_not_unrelated_text() {
        assert!(looks_like_telegram_command(
            "telegram sarah is this working"
        ));
        assert!(looks_like_telegram_command(
            "telegran sarah is this working"
        ));
        assert!(looks_like_telegram_command(
            "send a telegram to mom saying hi"
        ));
        assert!(!looks_like_telegram_command(
            "turn on the living room lights"
        ));
        assert!(!looks_like_telegram_command("what's the telegraph office"));
    }

    fn contact_records() -> Vec<ContactRecord> {
        vec![
            ContactRecord {
                display_name: "Sarah Jones".to_string(),
                id: 111,
                access_hash: 1,
            },
            ContactRecord {
                display_name: "Sarah Miller".to_string(),
                id: 222,
                access_hash: 2,
            },
            ContactRecord {
                display_name: "Andrew Heath".to_string(),
                id: 333,
                access_hash: 3,
            },
        ]
    }

    #[test]
    fn find_best_contact_match_exact_full_name() {
        let contacts = contact_records();
        let c = find_best_contact_match("Andrew Heath", &contacts).unwrap();
        assert_eq!(c.display_name, "Andrew Heath");
    }

    #[test]
    fn find_best_contact_match_unique_first_name() {
        let contacts = contact_records();
        let c = find_best_contact_match("andrew", &contacts).unwrap();
        assert_eq!(c.display_name, "Andrew Heath");
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
        assert!(find_best_contact_match("nobody", &contacts).is_err());
    }

    #[test]
    fn run_confirmed_rejects_an_empty_edited_body() {
        let cfg = TelegramConfig {
            api_id: 1,
            api_hash: "x".to_string(),
            session_path: std::path::PathBuf::from("/nonexistent/does-not-matter.session"),
        };
        let (ok, message) = run_confirmed("telegram sarah running late", Some("   "), &cfg);
        assert!(!ok);
        assert_eq!(message, "Message is empty -- not sending");
    }
}
