//! Web search and website open. Port of nova-npu's
//! `ai/commands/web_search.py` and `ai/commands/web_open.py`, scoped
//! to the two intents the classifier already distinguishes
//! (`WEB_SEARCH` vs `OPEN_WEBSITE`) — the Python originals also had
//! `looks_like_website_command`/`_extract_search_query` regex
//! recovery for when the classifier mislabeled something as
//! `EXTERNAL`, but `EXTERNAL` falls to `RouteResult::Unhandled` here
//! (see router/mod.rs), so that recovery layer has nothing to recover
//! from.

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

pub fn search(query: &str) -> (bool, String) {
    let clean = query.trim().trim_matches(|c| c == '"' || c == '\'');
    if clean.is_empty() {
        return (false, "No search query provided".to_string());
    }
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
