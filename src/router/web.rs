//! Web search and website open. Port of nova-npu's
//! `ai/commands/web_search.py` and `ai/commands/web_open.py`, scoped
//! to the two intents the classifier already distinguishes
//! (`WEB_SEARCH` vs `OPEN_WEBSITE`) — the Python originals also had
//! `looks_like_website_command`/`_extract_search_query` regex
//! recovery for when the classifier mislabeled something as
//! `EXTERNAL`, but `EXTERNAL` falls to `RouteResult::Unhandled` here
//! (see router/mod.rs), so that recovery layer has nothing to recover
//! from.
//!
//! `_extract_search_query`'s *other* job -- stripping the leading verb
//! ("search", "search for", "google", "look up", ...) off the query
//! text -- was still needed even once the EXTERNAL-recovery half of it
//! was dropped: the classifier's ARGUMENT is copied verbatim from the
//! utterance (see `classify::SYSTEM_PROMPT`), so a correctly-classified
//! `WEB_SEARCH` still carries whatever trigger words the user led with.
//! Observed live: "search google news" classified correctly as
//! `WEB_SEARCH` but searched for the literal phrase "search google
//! news" instead of "google news". `strip_search_prefix` below covers
//! that, mirroring `bluebubbles`/`telegram`'s own
//! LEADING_PHRASES/fuzzy-verb-stripping pattern rather than reviving
//! the Python regex.

use std::process::{Command, Stdio};

/// Spoken name -> URL, same list as nova's `_SITE_MAP`.
const SITES: &[(&str, &str)] = &[
    ("youtube", "https://youtube.com"),
    ("github", "https://github.com"),
    ("reddit", "https://reddit.com"),
    ("twitter", "https://twitter.com"),
    ("x", "https://x.com"),
    ("google", "https://google.com"),
    ("gmail", "https://mail.google.com"),
    ("google drive", "https://drive.google.com"),
    ("google docs", "https://docs.google.com"),
    ("google maps", "https://maps.google.com"),
    ("amazon", "https://amazon.com"),
    ("wikipedia", "https://wikipedia.org"),
    ("stack overflow", "https://stackoverflow.com"),
    ("stackoverflow", "https://stackoverflow.com"),
    ("netflix", "https://netflix.com"),
    ("twitch", "https://twitch.tv"),
    ("linkedin", "https://linkedin.com"),
    ("facebook", "https://facebook.com"),
    ("instagram", "https://instagram.com"),
    ("spotify", "https://open.spotify.com"),
    ("chatgpt", "https://chat.openai.com"),
    ("claude", "https://claude.ai"),
    ("hacker news", "https://news.ycombinator.com"),
];

/// Verb/filler phrases that precede the actual search terms in a
/// natural "search the web" command -- e.g. "search google news" means
/// search for "google news", not the whole phrase including "search".
/// The classifier's ARGUMENT is copied verbatim from the utterance (see
/// `classify::SYSTEM_PROMPT`), so it still carries whatever trigger verb
/// the user led with; nothing upstream strips it. Checked longest-first,
/// same reasoning as `bluebubbles::LEADING_PHRASES` /
/// `telegram::LEADING_PHRASES` -- "search for " must be tried before the
/// shorter "search " so "search for the best pizza" doesn't leave a
/// stray "for" on the front of the query.
const LEADING_PHRASES: &[&str] = &[
    "search the web for ",
    "search the internet for ",
    "search google for ",
    "google search for ",
    "web search for ",
    "search for ",
    "look up ",
    "look for ",
    "web search ",
    "search ",
    "google ",
];

/// The single-word verbs among `LEADING_PHRASES` eligible for fuzzy
/// matching (see `fuzzy_search_trigger`) -- same restriction as
/// `telegram::SINGLE_WORD_TRIGGERS`: only whole standalone verbs, not
/// multi-word phrases.
const SINGLE_WORD_TRIGGERS: &[&str] = &["search", "google"];

/// Plain O(n*m) Levenshtein (edit) distance -- see
/// `bluebubbles::levenshtein`'s docs for why this exists at all (ASR
/// mis-hearings of the leading command verb). Duplicated rather than
/// shared: the trigger word sets differ enough per channel that a
/// shared helper would need to thread more through its signature than
/// it saves -- same reasoning `telegram::levenshtein` gives for its own
/// copy.
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

fn fuzzy_search_trigger(word: &str) -> bool {
    let word = word.to_lowercase();
    SINGLE_WORD_TRIGGERS.iter().any(|&trigger| {
        let dist = levenshtein(&word, trigger);
        dist > 0 && dist <= max_fuzzy_distance(trigger.len())
    })
}

fn alphanumeric_only(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Strips a single leading verb/filler phrase off `query`, if present --
/// an exact `LEADING_PHRASES` match, or a bare/fuzzy-mis-heard trigger
/// word with nothing but that word itself (see `fuzzy_search_trigger`).
/// Only ever strips once: the result isn't fed back through again, so
/// "search google news" (which starts with a `LEADING_PHRASES` entry,
/// "search ") comes out as "google news" rather than being stripped a
/// second time down to just "news" by the standalone "google " entry
/// later in the same list -- "google news" is the actual site/
/// publication the user meant to search for.
///
/// The second check (bare trigger word, exact or fuzzy) only ever fires
/// when the first one didn't: every `LEADING_PHRASES` entry for a
/// `SINGLE_WORD_TRIGGERS` word already includes its trailing space
/// (e.g. "search "), so an exact trigger word followed by more content
/// is always caught there first. This only covers the leftover case --
/// the trigger word with nothing following it at all ("search" alone),
/// where there's no trailing space for `LEADING_PHRASES` to match.
fn strip_search_prefix(query: &str) -> &str {
    let trimmed = query.trim();
    let lower = trimmed.to_lowercase();
    if let Some(phrase) = LEADING_PHRASES.iter().find(|p| lower.starts_with(**p)) {
        return trimmed[phrase.len()..].trim();
    }
    if let Some(first) = trimmed.split_whitespace().next() {
        let normalized = alphanumeric_only(first).to_lowercase();
        let is_trigger = SINGLE_WORD_TRIGGERS.contains(&normalized.as_str())
            || fuzzy_search_trigger(&normalized);
        if is_trigger {
            return trimmed[first.len()..].trim_start();
        }
    }
    trimmed
}

fn xdg_open(url: &str) -> (bool, String) {
    match Command::new("xdg-open")
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => (true, url.to_string()),
        Err(e) => (false, format!("xdg-open failed: {e}")),
    }
}

/// Percent-encode for a URL query component. `url`/`urlencoding` isn't
/// already a omarchy-novad dependency and this only needs to handle plain
/// spoken-text queries, so a small hand-rolled encoder is enough
/// rather than pulling in a crate for it.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Cleans `query` into the literal text to search for: strips a single
/// leading verb/filler phrase (see `strip_search_prefix`) and any
/// wrapping quotes, reporting `None` for an empty result rather than
/// searching for nothing. Split out from `search` itself so the query-
/// building logic can be tested without spawning a real browser via
/// `xdg_open`.
fn clean_query(query: &str) -> Option<&str> {
    let clean = strip_search_prefix(query).trim_matches(|c| c == '"' || c == '\'');
    if clean.is_empty() {
        None
    } else {
        Some(clean)
    }
}

pub fn search(query: &str) -> (bool, String) {
    let Some(clean) = clean_query(query) else {
        return (false, "No search query provided".to_string());
    };
    let url = format!("https://www.google.com/search?q={}", percent_encode(clean));
    tracing::debug!("[router:web] search {clean:?} -> {url}");
    let (ok, _) = xdg_open(&url);
    (
        ok,
        if ok {
            format!("Searching for: {clean}")
        } else {
            format!("Search failed: {clean}")
        },
    )
}

fn resolve_site(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    if let Some((_, url)) = SITES.iter().find(|(k, _)| *k == lower) {
        return url.to_string();
    }
    // Already a URL, or close enough (e.g. "example.com") -- pass
    // through with a scheme so xdg-open treats it as a web address
    // rather than trying to resolve it as a local file/protocol.
    if lower.starts_with("http://") || lower.starts_with("https://") {
        lower
    } else {
        format!("https://{lower}")
    }
}

pub fn open_site(name: &str) -> (bool, String) {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return (false, "No site specified".to_string());
    }
    let url = resolve_site(trimmed);
    tracing::debug!("[router:web] open {trimmed:?} -> {url}");
    let (ok, _) = xdg_open(&url);
    (
        ok,
        if ok {
            format!("Opening {trimmed}")
        } else {
            format!("Failed to open {trimmed}")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_bare_search_verb_from_the_bug_report() {
        // Exact live failure: "hey jarvis, search google news" (wake
        // word already stripped before this point) classified correctly
        // as WEB_SEARCH, but the ARGUMENT was copied verbatim, so this
        // searched Google for the literal phrase "search google news"
        // instead of "google news".
        assert_eq!(clean_query("search google news"), Some("google news"));
    }

    #[test]
    fn strips_search_for_connector() {
        assert_eq!(
            clean_query("search for the best pizza near me"),
            Some("the best pizza near me")
        );
    }

    #[test]
    fn strips_web_search_phrasing() {
        assert_eq!(
            clean_query("web search python tutorials"),
            Some("python tutorials")
        );
        assert_eq!(
            clean_query("web search for python tutorials"),
            Some("python tutorials")
        );
    }

    #[test]
    fn strips_look_up_phrasing() {
        assert_eq!(
            clean_query("look up the weather in paris"),
            Some("the weather in paris")
        );
    }

    #[test]
    fn strips_bare_leading_google() {
        assert_eq!(clean_query("google the mona lisa"), Some("the mona lisa"));
    }

    #[test]
    fn only_strips_once_so_google_news_survives_intact() {
        // "search " is the LEADING_PHRASES match, not "google " -- the
        // result isn't fed back through the stripper a second time, so
        // "google news" (the actual publication the user meant) isn't
        // reduced further to just "news".
        assert_eq!(clean_query("search google news"), Some("google news"));
        assert_eq!(clean_query("google news"), Some("news"));
    }

    #[test]
    fn strips_fuzzy_mis_heard_search_verb() {
        // Same category of ASR slip observed live for BlueBubbles' "text"
        // -> "tax" (see bluebubbles.rs) and Telegram's "telegram" ->
        // "telegran" (see telegram.rs).
        assert_eq!(clean_query("serch google news"), Some("google news"));
    }

    #[test]
    fn leaves_a_query_with_no_trigger_word_alone() {
        assert_eq!(
            clean_query("best pizza near me"),
            Some("best pizza near me")
        );
    }

    #[test]
    fn strips_wrapping_quotes_after_the_verb() {
        assert_eq!(
            clean_query("search \"best pizza near me\""),
            Some("best pizza near me")
        );
    }

    #[test]
    fn empty_or_verb_only_query_returns_none() {
        assert_eq!(clean_query(""), None);
        assert_eq!(clean_query("search"), None);
        assert_eq!(clean_query("search "), None);
    }
}
