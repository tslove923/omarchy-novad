//! Local registry of OpenClaw session keys this daemon has created, so
//! the conversation panel's session picker can list recent
//! conversations to resume. OpenClaw itself has no "list sessions for
//! this agent" API -- a session key (`agent:main:novad:<key>`, see
//! `router::openclaw`) is just an opaque string the gateway will
//! happily create or continue on first use, so *something* has to
//! remember which ones this daemon has minted for a picker to show
//! anything at all.
//!
//! Same filesystem-JSON convention as `popup`/`conversation` (this
//! module's only reader is the QML session-picker's `FileView`),
//! scoped to `$XDG_RUNTIME_DIR` like the rest of this project's daemon
//! state -- deliberately NOT persisted anywhere more durable: losing
//! the list on logout/reboot just means the picker starts empty and
//! the next conversation mints a fresh session, which is the common
//! case anyway (see `docs/design-notes/conversation-flow-redesign.md`
//! and `converse::run`'s "usually a new session" framing).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// The bare key -- `router::openclaw` wraps it as
    /// `agent:main:novad:<key>` when it actually talks to the gateway.
    /// Not the full session key itself, so every record here stays
    /// tied to this one agent/prefix without repeating it.
    pub key: String,
    /// Empty until the session's first turn is known (see `touch`) --
    /// the picker falls back to showing the key/timestamp for a
    /// still-empty label rather than waiting on this.
    pub label: String,
    pub created_at_ms: u64,
    pub last_active_ms: u64,
}

fn registry_path() -> PathBuf {
    runtime_dir().join("sessions.json")
}

fn runtime_dir() -> PathBuf {
    // Same fallback shape as popup::runtime_dir / conversation::runtime_dir.
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn load() -> Vec<SessionRecord> {
    std::fs::read_to_string(registry_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write-temp-then-rename, not a truncate-in-place -- see
/// `conversation::write_state`'s doc comment for why (a rapid
/// truncate-and-rewrite of the same inode can permanently wedge
/// Quickshell's `FileView` watch).
fn save(records: &[SessionRecord]) {
    let path = registry_path();
    match serde_json::to_string(records) {
        Ok(json) => {
            let tmp_path = path.with_file_name(format!(
                "{}.tmp",
                path.file_name().unwrap_or_default().to_string_lossy()
            ));
            let result =
                std::fs::write(&tmp_path, json).and_then(|_| std::fs::rename(&tmp_path, &path));
            if let Err(e) = result {
                tracing::warn!("failed to write session registry to {path:?}: {e}");
            }
        }
        Err(e) => tracing::warn!("failed to serialize session registry: {e}"),
    }
}

/// Recency-ordered (most recently active first) -- what the picker
/// dropdown shows top to bottom.
pub fn list() -> Vec<SessionRecord> {
    let mut records = load();
    records.sort_by_key(|r| std::cmp::Reverse(r.last_active_ms));
    records
}

/// Mints a fresh session key and registers it -- the default path for
/// every new conversation (a wake-word activation, a manual "New
/// Session" pick, or a `converse start` with no `--session` override).
/// Timestamp-suffixed: unique, sorts naturally, and keeps the same
/// `voice-` prefix every session key has always used (`CONVERSATION_ID`
/// used to be the constant `"voice"`; only the timestamp suffix is
/// new).
pub fn new_session() -> String {
    let now = now_ms();
    let key = format!("voice-{now}");
    let mut records = load();
    records.push(SessionRecord {
        key: key.clone(),
        label: String::new(),
        created_at_ms: now,
        last_active_ms: now,
    });
    save(&records);
    key
}

/// Bumps `key`'s `last_active_ms` to now (so the picker's recency
/// ordering reflects real activity, not just creation time) and fills
/// in `label_hint` as its display label the first time this is called
/// for it. Adds a registry entry if `key` isn't one `new_session`
/// minted -- e.g. `--session` was given a hand-typed key -- so it
/// still shows up in the picker.
pub fn touch(key: &str, label_hint: &str) {
    let mut records = load();
    let now = now_ms();
    match records.iter_mut().find(|r| r.key == key) {
        Some(r) => {
            r.last_active_ms = now;
            if r.label.is_empty() {
                r.label = label_for(label_hint);
            }
        }
        None => records.push(SessionRecord {
            key: key.to_string(),
            label: label_for(label_hint),
            created_at_ms: now,
            last_active_ms: now,
        }),
    }
    save(&records);
}

/// Trims `hint` down to a picker-friendly label -- the first turn's
/// utterance is usually the most recognizable thing about a session,
/// but a full multi-sentence request would blow out a dropdown row.
fn label_for(hint: &str) -> String {
    const MAX_CHARS: usize = 40;
    let trimmed = hint.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.chars().count() <= MAX_CHARS {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(MAX_CHARS).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Every test touches the same $XDG_RUNTIME_DIR-derived file, so
    // they can't run concurrently -- same guard pattern this crate
    // uses wherever a test needs an isolated env var / shared file
    // (see e.g. config::tests).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_isolated_runtime_dir<T>(f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile_dir();
        // SAFETY: serialized by ENV_LOCK, no other thread reads env vars here.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        let result = f();
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn tempfile_dir() -> PathBuf {
        std::env::temp_dir().join(format!(
            "omarchy-novad-sessions-test-{}-{}",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn new_session_registers_and_lists_with_empty_label() {
        with_isolated_runtime_dir(|| {
            let key = new_session();
            let records = list();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].key, key);
            assert_eq!(records[0].label, "");
        });
    }

    #[test]
    fn touch_fills_label_once_and_keeps_it_on_later_touches() {
        with_isolated_runtime_dir(|| {
            let key = new_session();
            touch(&key, "what's the weather like");
            touch(&key, "a completely different later utterance");
            let records = list();
            assert_eq!(records[0].label, "what's the weather like");
        });
    }

    #[test]
    fn touch_registers_an_unknown_key_too() {
        with_isolated_runtime_dir(|| {
            touch("hand-typed-key", "hello");
            let records = list();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].key, "hand-typed-key");
        });
    }

    #[test]
    fn list_sorts_most_recently_active_first() {
        with_isolated_runtime_dir(|| {
            let older = new_session();
            std::thread::sleep(std::time::Duration::from_millis(2));
            let newer = new_session();
            std::thread::sleep(std::time::Duration::from_millis(2));
            touch(&older, "resurfaced by activity"); // bump older back to the top
            let records = list();
            assert_eq!(records[0].key, older);
            assert_eq!(records[1].key, newer);
        });
    }

    #[test]
    fn label_for_truncates_long_hints() {
        let long = "x".repeat(100);
        let label = label_for(&long);
        assert!(label.chars().count() <= 41); // 40 + the ellipsis char
        assert!(label.ends_with('…'));
    }
}
