//! External handoff to OpenClaw — port of nova-npu's
//! `ai/commands/coding_bridge.py` (routes anything the local
//! classifier can't handle itself to a real reasoning/coding agent),
//! scoped to what's actually available here: a CLI bridge script
//! (`openclaw-handoff`, `~/.local/bin/`) rather than nova's themed
//! Electron chat window + REST API popup integration.
//!
//! The bridge script (not this file) owns the actual OpenClaw
//! transport (gateway WebSocket URL + token, from
//! `~/.config/openclaw-novad.env`, chmod 600 -- the script's own
//! path, unrelated to this crate's rename to omarchy-novad) — this
//! module only knows how to invoke it and interpret its exit code,
//! matching the shell-out pattern `app_launcher`/`web` already use
//! for their own external processes.
//!
//! ## `continue_in_herdr`: opening a real interactive session
//!
//! `handoff` below is a one-shot `openclaw agent --message` call --
//! fast, and (confirmed live) exempt from the device-pairing gate
//! described next. `openclaw tui`, the *interactive* terminal UI, is
//! not: it's a persistent "operator" WebSocket session, and the
//! gateway requires a human (or a scripted stand-in, see
//! `crate::config::OpenClawConfig::approve_device_command`) to approve
//! its device identity once before it'll connect -- confirmed live as
//! a known, currently-unresolved upstream limitation for
//! token-authenticated remote clients
//! (<https://github.com/openclaw/openclaw/issues/29908>), not a local
//! misconfiguration; the one documented workaround
//! (`gateway.controlUi.allowInsecureAuth`) is itself reported buggy
//! for reverse-proxied deployments like this one
//! (<https://github.com/openclaw/openclaw/issues/1679>).
//!
//! That gate is exactly why this is a separate, explicit "continue in
//! Herdr" action rather than folded into the automatic wake-word
//! handoff: a hands-free trigger that can silently need a human to
//! approve a device somewhere isn't hands-free. Once approved, though,
//! the device identity persists for future launches from the same
//! machine (confirmed live) -- it's a one-time bootstrap cost, not a
//! per-session tax, so `approve_device_command` only fires when the
//! gateway actually reports a pending request.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use tungstenite::Message;

/// Words that can sit between "open" and "claw" in an ASR-mangled
/// "openclaw" without meaning an OpenClaw command -- "open her claw" is
/// the cat, "open the claw" is a machine part. Shared by
/// [`looks_like_external_command`] (which decides *whether* an utterance
/// addresses OpenClaw) and [`strip_external_preamble`] (which decides
/// *what the user actually asked* once it does).
const FILLER_EXCLUSIONS: &[&str] = &[
    "her", "his", "its", "my", "your", "our", "their", "the", "a", "an", "this", "that", "these",
    "those",
];

/// Recovery check, same shape as `bluebubbles::looks_like_message_command`
/// / `telegram::looks_like_telegram_command`: does `text` look like it's
/// addressing OpenClaw specifically, even through ASR noise, so
/// `pipeline.rs` can recover a misclassified MEMORY_RETURN back into
/// `Intent::External`? Observed live: voxtype transcribed "...ask
/// openclaw what..." as "Ask open. Claw. what..." -- a stray sentence
/// break inserted mid-word -- which the classifier read as MEMORY_RETURN
/// since punctuation-mangled "open. claw." doesn't visually resemble its
/// own name. Strips punctuation first so that exact case is caught, and
/// checks a leading window of words rather than requiring the trigger to
/// be literally the first word -- unlike "text"/"message", people
/// naturally lead with a verb ("ask openclaw...", "tell openclaw...",
/// "hey openclaw...") rather than putting "openclaw" itself first.
pub fn looks_like_external_command(text: &str) -> bool {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    // Anywhere in the utterance, not just the leading words -- found
    // live: "What's the status with Home Assistant? Ask OpenClaw."
    // says it as a trailing afterthought, not a leading trigger word,
    // and still unambiguously means "hand this off". A leading-only
    // window was the right shape for the original ASR-mangled-trigger-
    // word failure this was built for ("Ask open. Claw. what..."), but
    // it's too narrow for how people actually phrase these.
    if cleaned.contains("openclaw") || cleaned.contains("open claw") {
        return true;
    }

    // "open <word> claw" -- observed live: voxtype transcribed "ask
    // OpenClaw to give a status on the home assistant" as "ask open
    // cloud claw to give it a status on a status on the home
    // assistant", inserting a word between the two halves of the name.
    // A general "open X claw" window would also match "open her claw"
    // (a real false positive -- the cat opens its claw), so the
    // intervening token is only allowed when it isn't a
    // pronoun/possessive/article: "cloud" isn't one, "her" is. One
    // intervening token covers the observed case; widen if a longer
    // insertion ever shows up live. (FILLER_EXCLUSIONS is module-level,
    // shared with strip_external_preamble below.)
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    for i in 0..tokens.len().saturating_sub(2) {
        if tokens[i] == "open" && tokens[i + 2] == "claw" && !FILLER_EXCLUSIONS.contains(&tokens[i + 1])
        {
            return true;
        }
    }
    false
}

/// Strips a leading "ask openclaw"/"have openclaw" routing preamble from
/// `text`, leaving the actual command for the conversation loop's first
/// turn. [`looks_like_external_command`] decides *whether* an utterance
/// addresses OpenClaw; this decides *what the user actually asked* once
/// it does. The classifier's own `argument` can't do this job -- observed
/// live, it echoes the preamble back verbatim ("ask open claw what's
/// planned for day dinner tonight?") -- so this strips the preamble
/// deterministically instead, preserving the rest of the transcript
/// word-for-word (original casing and punctuation).
///
/// Handles the same ASR manglings as [`looks_like_external_command`]:
/// "openclaw", "open claw", and "open <word> claw" (with the same
/// [`FILLER_EXCLUSIONS`]). The reference is only treated as a preamble
/// when it's at the start of the utterance or preceded by a command/
/// address verb ("ask", "have", "tell", "get", "hey", ...) -- "tell me
/// about openclaw" and "how does openclaw work" are questions *about*
/// OpenClaw, not routing preambles, and are left untouched. A trailing
/// "ask openclaw" afterthought ("What's the status with Home Assistant?
/// Ask OpenClaw.") is stripped too, since the reference is still
/// preceded by a command verb.
pub fn strip_external_preamble(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return text.to_string();
    }

    // Lowercased, punctuation-stripped view of each word for matching --
    // same normalization `looks_like_external_command` uses.
    let cleaned: Vec<String> = words
        .iter()
        .map(|w| {
            w.to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric())
                .collect()
        })
        .collect();

    // Locate the OpenClaw reference as a [start, end) word span: a single
    // "openclaw" word, or an "open claw" / "open <word> claw" sequence.
    let mut ref_span: Option<(usize, usize)> = None;
    for (i, w) in cleaned.iter().enumerate() {
        if w == "openclaw" {
            ref_span = Some((i, i + 1));
            break;
        }
    }
    if ref_span.is_none() {
        for i in 0..cleaned.len().saturating_sub(1) {
            if cleaned[i] == "open" && cleaned[i + 1] == "claw" {
                ref_span = Some((i, i + 2));
                break;
            }
        }
    }
    if ref_span.is_none() {
        for i in 0..cleaned.len().saturating_sub(2) {
            if cleaned[i] == "open"
                && cleaned[i + 2] == "claw"
                && !FILLER_EXCLUSIONS.contains(&cleaned[i + 1].as_str())
            {
                ref_span = Some((i, i + 3));
                break;
            }
        }
    }
    let Some((ref_start, ref_end)) = ref_span else {
        return text.to_string();
    };

    // Expand the span backward over command/address verbs ("ask
    // openclaw", "please have openclaw", ...).
    const ROUTING_VERBS: &[&str] = &["ask", "have", "tell", "get", "hey", "hi", "yo", "please"];
    let mut span_start = ref_start;
    while span_start > 0 && ROUTING_VERBS.contains(&cleaned[span_start - 1].as_str()) {
        span_start -= 1;
    }

    // Only a routing preamble gets stripped: the reference at the very
    // start, or preceded by a command/address verb. Anything else is a
    // question *about* OpenClaw and stays as-is.
    let is_preamble = ref_start == 0 || span_start < ref_start;
    if !is_preamble {
        return text.to_string();
    }

    // Expand the span forward over connector words ("ask openclaw to
    // give status" → "give status").
    const CONNECTORS: &[&str] = &[
        "to", "please", "can", "could", "would", "will", "you", "do", "does", "did", "may",
        "might", "shall", "should",
    ];
    let mut span_end = ref_end;
    while span_end < words.len() && CONNECTORS.contains(&cleaned[span_end].as_str()) {
        span_end += 1;
    }

    // The preamble covers the whole utterance ("ask openclaw" alone) --
    // keep the original rather than returning an empty string.
    if span_start == 0 && span_end >= words.len() {
        return text.to_string();
    }

    // Reassemble: words before the span + words after it.
    let mut result = Vec::new();
    result.extend_from_slice(&words[..span_start]);
    result.extend_from_slice(&words[span_end..]);
    result.join(" ")
}

/// All wake-word-triggered handoffs share one conversation so OpenClaw
/// keeps context across turns (nova's own `coding_bridge.py` did the
/// same with its in-process `_history` list) -- omarchy-novad doesn't have a
/// per-session/per-user conversation concept yet, so this is the
/// simplest thing that gives real continuity today. Revisit if omarchy-novad
/// ever needs to distinguish separate voice "conversations" (e.g. a
/// timeout-based reset, or multiple concurrent users).
const CONVERSATION_ID: &str = "voice";

// ────────────────────────── streaming gateway client ──────────────────────────
//
// `handoff` above shells out to the `openclaw agent` CLI, which is
// final-only: it prints the complete reply when the turn finishes and
// nothing before. The conversation panel needs the reply *as it's
// produced* (see `crate::converse`'s module docs), so this module also
// speaks the gateway WebSocket protocol directly -- the same transport
// the CLI itself uses, just with the streamed `agent` events surfaced
// instead of discarded. Protocol verified live against this gateway
// (2026-08-27): `connect.challenge` → device-signed `connect` →
// `hello-ok`, then `chat.send` streams `agent` events with
// `stream:"assistant"` and `data:{text,delta}` (cumulative + delta)
// until a `stream:"lifecycle"` `data.phase:"end"` event. No explicit
// event subscription is needed -- `chat.send` alone streams to the
// connection.

/// The gateway WebSocket connection type -- `MaybeTlsStream` because
/// the gateway is `wss://` (TLS via tungstenite's rustls feature).
type WsStream = tungstenite::stream::MaybeTlsStream<std::net::TcpStream>;
type Ws = tungstenite::WebSocket<WsStream>;

/// How long to wait for *any* frame before declaring the gateway hung.
/// The gateway sends a `tick` keepalive roughly every 15s, so 90s
/// means ~6 missed keepalives -- generous for a slow agent turn (the
/// handoff itself still has no timeout; a real multi-minute turn is
/// fine as long as *some* frame arrives), but a real backstop against
/// a wedged socket that the CLI path's `--timeout 86400` used to be.
const WS_READ_TIMEOUT: Duration = Duration::from_secs(90);

/// Hands `utterance` off to OpenClaw over the gateway WebSocket and
/// calls `on_text` with the cumulative reply-so-far as each chunk
/// streams in -- the live-output path `converse::run_handoff_with_progress`
/// uses (see that function's docs for how the chunks reach the panel).
/// Returns `(success, reply_or_error)` with the same contract as
/// [`handoff`]: `reply` is OpenClaw's full answer, ready to show as-is.
///
/// Same conversation as `handoff` (`CONVERSATION_ID`), so a turn sent
/// here continues the same OpenClaw session the wake-word handoff and
/// the Herdr TUI share.
pub fn handoff_streaming(utterance: &str, on_text: impl Fn(&str)) -> (bool, String) {
    let clean = utterance.trim();
    if clean.is_empty() {
        return (false, "Nothing to hand off".to_string());
    }

    let Some((url, token)) = gateway_credentials() else {
        return (
            false,
            "No OpenClaw gateway credentials found (checked $OPENCLAW_NOVAD_ENV or \
             ~/.config/openclaw-novad.env)"
                .to_string(),
        );
    };
    let Some((device_id, public_key_pem, private_key_pem)) = device_identity() else {
        return (
            false,
            "No OpenClaw device identity found (~/.openclaw/identity/device.json)"
                .to_string(),
        );
    };

    let (mut ws, _) = match tungstenite::connect(&url) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("[router:openclaw] ws connect failed: {e}");
            return (false, "Couldn't connect to the OpenClaw gateway".to_string());
        }
    };
    set_ws_read_timeout(&mut ws, WS_READ_TIMEOUT);

    // 1. The gateway opens with a `connect.challenge` event carrying a
    //    nonce we must sign with the device identity keypair.
    let Some(nonce) = read_challenge(&mut ws) else {
        return (
            false,
            "The OpenClaw gateway didn't send a connect challenge".to_string(),
        );
    };

    // 2. Reply with `connect`, device-signed. The signed payload is
    //    the gateway's v3 format -- a `|`-joined string the server
    //    reconstructs from the connect params and verifies against the
    //    device's public key (see the module docs and the probe that
    //    confirmed it live). `signatureToken` is the gateway token,
    //    matching what the CLI itself signs with (verified in the
    //    gateway-client dist: `signatureToken = authToken ?? ...`).
    let signed_at_ms = now_ms();
    let scopes = ["operator.admin", "operator.read", "operator.write"];
    let payload = build_device_auth_payload_v3(
        &device_id,
        "cli",
        "cli",
        "operator",
        &scopes,
        signed_at_ms,
        &token,
        &nonce,
        "linux",
        "desktop",
    );
    let Some(signature) = sign_ed25519(&private_key_pem, &payload) else {
        return (false, "Couldn't sign the gateway connect request".to_string());
    };
    let Some(public_key) = public_key_base64url(&public_key_pem) else {
        return (false, "Couldn't read the device public key".to_string());
    };
    let connect_req = serde_json::json!({
        "type": "req", "id": "c1", "method": "connect",
        "params": {
            "minProtocol": 4, "maxProtocol": 4,
            "client": { "id": "cli", "version": "2026.7.1-2", "platform": "linux",
                        "deviceFamily": "desktop", "mode": "cli" },
            "role": "operator",
            "scopes": scopes,
            "caps": [],
            "auth": { "token": token },
            "locale": "en-US",
            "userAgent": "omarchy-novad/0.1",
            "device": { "id": device_id, "publicKey": public_key, "signature": signature,
                        "signedAt": signed_at_ms, "nonce": nonce },
        }
    });
    if let Err(e) = ws.send(Message::Text(connect_req.to_string().into())) {
        tracing::warn!("[router:openclaw] connect send failed: {e}");
        return (false, "Couldn't send the gateway connect request".to_string());
    }

    // 3. Wait for `hello-ok` (or a rejection).
    if !wait_for_hello_ok(&mut ws) {
        return (false, "The OpenClaw gateway rejected the connection".to_string());
    }

    // 4. Send the message. `idempotencyKey` is the gateway's
    //    deduplication key for side-effecting methods -- a unique
    //    per-request value, same role the CLI's random UUID plays.
    let run_id = format!("novad-{}-{}", now_ms(), std::process::id());
    let send_req = serde_json::json!({
        "type": "req", "id": "c2", "method": "chat.send",
        "params": {
            "sessionKey": format!("agent:main:novad:{CONVERSATION_ID}"),
            "message": clean,
            "idempotencyKey": run_id,
        }
    });
    if let Err(e) = ws.send(Message::Text(send_req.to_string().into())) {
        tracing::warn!("[router:openclaw] chat.send failed: {e}");
        return (false, "Couldn't send the message to OpenClaw".to_string());
    }

    // 5. Stream `agent` events until the run ends. `data.text` is the
    //    cumulative reply (verified live: "P" then "PONG"); fall back
    //    to appending `data.delta` if a gateway ever omits `text`.
    let mut full = String::new();
    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("[router:openclaw] ws read failed: {e}");
                break;
            }
        };
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["type"] == "res" && v["ok"] == false {
                    let err = v["error"]["message"].as_str().unwrap_or("unknown error");
                    tracing::warn!("[router:openclaw] gateway error: {err}");
                    full.clear();
                    break;
                }
                if v["type"] != "event" {
                    continue;
                }
                match v["event"].as_str() {
                    Some("agent") => {
                        let payload = &v["payload"];
                        match payload["stream"].as_str() {
                            Some("assistant") => {
                                let data = &payload["data"];
                                if let Some(text) = data["text"].as_str() {
                                    if !text.is_empty() {
                                        full = text.to_string();
                                        on_text(text);
                                    }
                                } else if let Some(delta) = data["delta"].as_str() {
                                    if !delta.is_empty() {
                                        full.push_str(delta);
                                        on_text(&full);
                                    }
                                }
                            }
                            Some("lifecycle") => {
                                if payload["data"]["phase"].as_str() == Some("end") {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                    Some("chat") => {
                        // Belt-and-suspenders completion signal -- the
                        // `agent` lifecycle "end" event is the primary
                        // one, but a `chat` "final" event also marks
                        // the turn done.
                        if v["payload"]["state"].as_str() == Some("final") {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p));
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    if full.trim().is_empty() {
        (false, "The external assistant replied with nothing".to_string())
    } else {
        (true, full)
    }
}

/// Reads frames until the gateway's `connect.challenge` event and
/// returns its nonce.
fn read_challenge(ws: &mut Ws) -> Option<String> {
    loop {
        let msg = ws.read().ok()?;
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = serde_json::from_str(&t).ok()?;
                if v["type"] == "event" && v["event"] == "connect.challenge" {
                    return v["payload"]["nonce"].as_str().map(str::to_string);
                }
            }
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p));
            }
            Message::Close(_) => return None,
            _ => {}
        }
    }
}

/// Reads frames until the `hello-ok` response to our `connect` request
/// (true), or a rejection/close (false).
fn wait_for_hello_ok(ws: &mut Ws) -> bool {
    loop {
        let msg = match ws.read() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("[router:openclaw] ws read during connect failed: {e}");
                return false;
            }
        };
        match msg {
            Message::Text(t) => {
                let v: serde_json::Value = match serde_json::from_str(&t) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["type"] == "res" {
                    if v["ok"] == true && v["payload"]["type"] == "hello-ok" {
                        return true;
                    }
                    if v["ok"] == false {
                        let err = v["error"]["message"].as_str().unwrap_or("unknown error");
                        tracing::warn!("[router:openclaw] connect rejected: {err}");
                        return false;
                    }
                }
            }
            Message::Ping(p) => {
                let _ = ws.send(Message::Pong(p));
            }
            Message::Close(_) => return false,
            _ => {}
        }
    }
}

/// Sets a read timeout on the WebSocket's underlying TCP stream so a
/// wedged gateway (no frames at all, not even `tick` keepalives) can't
/// hang the handoff thread forever -- see `WS_READ_TIMEOUT`.
fn set_ws_read_timeout(ws: &mut Ws, dur: Duration) {
    use tungstenite::stream::MaybeTlsStream;
    let tcp: Option<&std::net::TcpStream> = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => Some(s),
        // Only `Plain`/`Rustls` exist with the `rustls-tls-native-roots`
        // feature (no `native-tls`); the enum is non-exhaustive upstream,
        // so a wildcard arm is required regardless.
        MaybeTlsStream::Rustls(s) => Some(s.get_ref()),
        _ => None,
    };
    if let Some(tcp) = tcp {
        let _ = tcp.set_read_timeout(Some(dur));
    }
}

/// Loads the device identity keypair the gateway requires for the
/// connect handshake -- `~/.openclaw/identity/device.json`, the same
/// file the `openclaw` CLI itself uses. Returns
/// `(device_id, public_key_pem, private_key_pem)`.
fn device_identity() -> Option<(String, String, String)> {
    let path = dirs::home_dir()?.join(".openclaw/identity/device.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    Some((
        v["deviceId"].as_str()?.to_string(),
        v["publicKeyPem"].as_str()?.to_string(),
        v["privateKeyPem"].as_str()?.to_string(),
    ))
}

/// Milliseconds since the Unix epoch -- the `signedAt`/`signedAtMs`
/// field the gateway's device-auth payload expects.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The gateway's v3 device-auth payload: a `|`-joined string the
/// server reconstructs from the connect params and verifies against
/// the device's public key. Mirrors the CLI's
/// `buildDeviceAuthPayloadV3` exactly (verified in the gateway-client
/// dist and live against this gateway).
fn build_device_auth_payload_v3(
    device_id: &str,
    client_id: &str,
    client_mode: &str,
    role: &str,
    scopes: &[&str],
    signed_at_ms: u64,
    token: &str,
    nonce: &str,
    platform: &str,
    device_family: &str,
) -> String {
    let scopes = scopes.join(",");
    let platform = normalize_device_metadata(platform);
    let device_family = normalize_device_metadata(device_family);
    format!(
        "v3|{device_id}|{client_id}|{client_mode}|{role}|{scopes}|{signed_at_ms}|{token}|{nonce}|{platform}|{device_family}"
    )
}

/// Lowercases a device-metadata string for the auth payload, matching
/// the CLI's `normalizeDeviceMetadataForAuth` (uppercase ASCII → lower;
/// empty stays empty).
fn normalize_device_metadata(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    trimmed.chars().map(|c| c.to_ascii_lowercase()).collect()
}

/// Ed25519-signs `payload` with the device's PKCS#8 PEM private key
/// and returns the base64url (no padding) signature the gateway
/// expects.
fn sign_ed25519(private_key_pem: &str, payload: &str) -> Option<String> {
    use ed25519_dalek::pkcs8::DecodePrivateKey;
    use ed25519_dalek::Signer;
    let key = ed25519_dalek::SigningKey::from_pkcs8_pem(private_key_pem).ok()?;
    let sig = key.sign(payload.as_bytes());
    Some(base64_url_no_pad(&sig.to_bytes()))
}

/// Extracts the raw Ed25519 public key from its PEM and returns it
/// base64url (no padding) -- the `device.publicKey` field the gateway
/// uses to verify the connect signature.
fn public_key_base64url(public_key_pem: &str) -> Option<String> {
    use ed25519_dalek::pkcs8::DecodePublicKey;
    let key = ed25519_dalek::VerifyingKey::from_public_key_pem(public_key_pem).ok()?;
    Some(base64_url_no_pad(&key.to_bytes()))
}

fn base64_url_no_pad(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// How long to wait after launching/relaunching `openclaw tui` before
/// checking whether it connected -- generous enough to cover a real
/// WebSocket handshake + gateway auth round-trip, short enough that a
/// hung launch doesn't stall this indefinitely (this only ever blocks
/// a synchronous CLI/popup call, same budget class as `HANDOFF_TIMEOUT`
/// above, not the daemon's own event loop).
const CONNECT_SETTLE: Duration = Duration::from_secs(3);

/// Substring `openclaw tui` prints when the gateway needs a human (or
/// `approve_device_command`) to approve this device's pairing request
/// before it'll connect -- see this module's doc comment.
const DEVICE_APPROVAL_MARKER: &str = "Device approval needed";

/// Opens `openclaw tui` in a new Herdr tab, attached to the same
/// gateway session `handoff` uses (`agent:main:novad:CONVERSATION_ID`)
/// so it picks up right where the automatic handoff's reply left off --
/// an explicit "continue this conversation" action (see this module's
/// doc comment for why it's not part of the automatic handoff path).
/// Mirrors OmaPilot's own `continueInHerdr` in spirit: hand authority
/// to a real interactive session instead of a flash-and-gone popup
/// summary.
pub fn continue_in_herdr(cfg: Option<&crate::config::OpenClawConfig>) -> (bool, String) {
    let Some((url, token)) = gateway_credentials() else {
        return (
            false,
            "No OpenClaw gateway credentials found (checked $OPENCLAW_NOVAD_ENV or \
             ~/.config/openclaw-novad.env)"
                .to_string(),
        );
    };

    let Some(script_path) = write_launch_script(&url, &token) else {
        return (false, "Couldn't write the Herdr launch script".to_string());
    };

    let Some(pane_id) = open_herdr_tab() else {
        return (
            false,
            "Couldn't open a Herdr tab -- is herdr running?".to_string(),
        );
    };

    run_in_pane(&pane_id, &script_path);
    std::thread::sleep(CONNECT_SETTLE);

    if pane_shows(&pane_id, DEVICE_APPROVAL_MARKER) {
        match cfg.and_then(|c| c.approve_device_command.as_deref()) {
            Some(approve_cmd) => {
                tracing::info!(
                    "[router:openclaw] device approval pending -- running configured \
                     approve_device_command"
                );
                match Command::new("sh").arg("-c").arg(approve_cmd).status() {
                    Ok(status) if status.success() => {
                        // Relaunch to actually connect with the
                        // now-approved identity -- the pending tui
                        // process left disconnected by the pairing
                        // gate doesn't retry on its own.
                        std::thread::sleep(Duration::from_secs(1));
                        run_in_pane(&pane_id, &script_path);
                        std::thread::sleep(CONNECT_SETTLE);
                    }
                    Ok(status) => {
                        tracing::warn!("[router:openclaw] approve_device_command exited {status}");
                    }
                    Err(e) => {
                        tracing::warn!(
                            "[router:openclaw] failed to run approve_device_command: {e}"
                        );
                    }
                }
            }
            None => {
                tracing::info!(
                    "[router:openclaw] device approval pending, no approve_device_command \
                     configured -- left in Herdr for the user to approve"
                );
            }
        }
    }

    (true, "Opened in Herdr".to_string())
}

/// Reads `OPENCLAW_GATEWAY_URL`/`OPENCLAW_GATEWAY_TOKEN` from the same
/// env file `openclaw-handoff` sources (`$OPENCLAW_NOVAD_ENV`, default
/// `~/.config/openclaw-novad.env`) -- not duplicated into
/// `config.toml`, since that would just be a second place for the
/// same credential to drift out of sync.
fn gateway_credentials() -> Option<(String, String)> {
    let path = std::env::var_os("OPENCLAW_NOVAD_ENV")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".config/openclaw-novad.env")
        });
    let content = std::fs::read_to_string(&path)
        .inspect_err(|e| tracing::warn!("[router:openclaw] reading {path:?}: {e}"))
        .ok()?;

    let mut url = None;
    let mut token = None;
    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("OPENCLAW_GATEWAY_URL=") {
            url = Some(v.trim_matches('"').to_string());
        } else if let Some(v) = line.strip_prefix("OPENCLAW_GATEWAY_TOKEN=") {
            token = Some(v.trim_matches('"').to_string());
        }
    }
    Some((url?, token?))
}

/// Writes a small self-contained launch script (mode 700 -- it embeds
/// the gateway token, same sensitivity as `openclaw-novad.env` itself)
/// under `$XDG_RUNTIME_DIR/omarchy-novad/`, same convention
/// `main.rs::transcript_path` already uses. A real file rather than an
/// inline command string: `herdr pane run` re-lexes its trailing
/// arguments at the target shell, which mangles quoting for anything
/// containing its own `--flag value` pairs (found live).
fn write_launch_script(url: &str, token: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("openclaw-herdr.sh");

    let script = format!(
        "#!/usr/bin/env bash\nexec openclaw tui --session agent:main:novad:{CONVERSATION_ID} \
         --url {url:?} --token {token:?}\n"
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    file.write_all(script.as_bytes()).ok()?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }

    Some(path)
}

/// Creates a new Herdr tab and returns its root pane id, or `None` if
/// `herdr` isn't running/reachable.
fn open_herdr_tab() -> Option<String> {
    let output = Command::new("herdr")
        .args(["tab", "create", "--label", "OpenClaw", "--focus"])
        .output()
        .inspect_err(|e| tracing::warn!("[router:openclaw] failed to spawn herdr: {e}"))
        .ok()?;
    if !output.status.success() {
        tracing::warn!(
            "[router:openclaw] herdr tab create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json["result"]["root_pane"]["pane_id"]
        .as_str()
        .map(str::to_string)
}

/// Runs `script_path` in `pane_id` -- fire-and-forget, same as a user
/// typing the command and pressing Enter.
fn run_in_pane(pane_id: &str, script_path: &std::path::Path) {
    let status = Command::new("herdr")
        .args(["pane", "run", pane_id])
        .arg(script_path)
        .status();
    if let Err(e) = status {
        tracing::warn!("[router:openclaw] herdr pane run failed: {e}");
    }
}

/// Whether `pane_id`'s current terminal content contains `needle` --
/// used to check for the device-approval prompt after a launch.
fn pane_shows(pane_id: &str, needle: &str) -> bool {
    match Command::new("herdr")
        .args(["pane", "read", pane_id])
        .output()
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout).contains(needle),
        Err(e) => {
            tracing::warn!("[router:openclaw] herdr pane read failed: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        base64_url_no_pad, build_device_auth_payload_v3, looks_like_external_command,
        normalize_device_metadata, strip_external_preamble,
    };

    #[test]
    fn catches_the_actual_live_mishearing() {
        // Observed live: voxtype transcribed this exact utterance, and
        // the classifier read it as MEMORY_RETURN.
        assert!(looks_like_external_command(
            "Ask open. Claw. what photo is on the photo frame right now When and where was it \
             taken and who is in it?"
        ));
    }

    #[test]
    fn catches_common_phrasings() {
        assert!(looks_like_external_command("openclaw what's the capital of France"));
        assert!(looks_like_external_command("ask openclaw to write a python script"));
        assert!(looks_like_external_command("hey openclaw, check the cluster"));
        assert!(looks_like_external_command("tell open claw to check the logs"));
    }

    #[test]
    fn does_not_fire_on_unrelated_text() {
        assert!(!looks_like_external_command("turn on the living room lights"));
        assert!(!looks_like_external_command("text mom I'm running late"));
        assert!(!looks_like_external_command(
            "what's the weather like today"
        ));
    }

    #[test]
    fn catches_a_trailing_mention_too() {
        // Observed live, reproduced three times in a row: saying
        // "openclaw" as a trailing afterthought rather than a leading
        // trigger word still unambiguously means "hand this off" --
        // and the classifier read every one of these as HOME_ASSISTANT
        // (not MEMORY_RETURN), since the utterance also names a topic
        // that intent handles.
        assert!(looks_like_external_command(
            "What's the status with Home Assistant? Ask OpenClaw."
        ));
        assert!(looks_like_external_command(
            "Ask OpenClaw what's the status with Home Assistant."
        ));
    }

    #[test]
    fn catches_an_inserted_word_between_open_and_claw() {
        // Observed live: voxtype transcribed "ask OpenClaw to give a
        // status on the home assistant" as "ask open cloud claw to give
        // it a status on a status on the home assistant" -- an inserted
        // word between the two halves of the name. The classifier read
        // it as HOME_ASSISTANT (the utterance also names that topic) and
        // the pre-fix matcher missed it entirely, so the request was
        // handled as a device command instead of handed off.
        assert!(looks_like_external_command(
            "ask open cloud claw to give it a status on a status on the home assistant"
        ));
    }

    #[test]
    fn does_not_fire_on_words_that_only_coincidentally_appear_in_sequence() {
        // "open" and "claw" appear back to back in spirit but not as
        // the literal substring "open claw" -- "her" splits them --
        // shouldn't false-positive as an OpenClaw command.
        assert!(!looks_like_external_command(
            "remind me to buy a new cat scratching post because the cat likes to open her claw \
             on the couch"
        ));
    }

    #[test]
    fn strips_a_leading_ask_openclaw_preamble() {
        assert_eq!(
            strip_external_preamble("ask openclaw to give status on home assistant"),
            "give status on home assistant"
        );
        assert_eq!(
            strip_external_preamble("Ask OpenClaw what's the status with Home Assistant."),
            "what's the status with Home Assistant."
        );
        assert_eq!(
            strip_external_preamble("have openclaw check the weather"),
            "check the weather"
        );
        assert_eq!(
            strip_external_preamble("hey openclaw, write a python script"),
            "write a python script"
        );
        assert_eq!(
            strip_external_preamble("please ask openclaw to fix the photo frame"),
            "fix the photo frame"
        );
    }

    #[test]
    fn strips_the_asr_mangled_preamble_too() {
        // Same live mishearings looks_like_external_command catches.
        assert_eq!(
            strip_external_preamble("ask open cloud claw to give it a status on the home assistant"),
            "give it a status on the home assistant"
        );
        assert_eq!(
            strip_external_preamble("Ask open. Claw. what photo is on the photo frame"),
            "what photo is on the photo frame"
        );
    }

    #[test]
    fn strips_a_trailing_ask_openclaw_afterthought() {
        // Observed live: "What's the status with Home Assistant? Ask
        // OpenClaw." -- the reference is a trailing afterthought, still
        // preceded by a command verb, so it gets stripped too.
        assert_eq!(
            strip_external_preamble("What's the status with Home Assistant? Ask OpenClaw."),
            "What's the status with Home Assistant?"
        );
    }

    #[test]
    fn leaves_questions_about_openclaw_alone() {
        // "tell me about openclaw" / "how does openclaw work" are
        // questions *about* OpenClaw, not routing preambles -- the
        // reference isn't preceded by a command/address verb, so the
        // utterance is left untouched.
        assert_eq!(
            strip_external_preamble("tell me about openclaw"),
            "tell me about openclaw"
        );
        assert_eq!(
            strip_external_preamble("how does openclaw work"),
            "how does openclaw work"
        );
    }

    #[test]
    fn leaves_text_without_a_preamble_alone() {
        assert_eq!(
            strip_external_preamble("turn on the living room lights"),
            "turn on the living room lights"
        );
        assert_eq!(strip_external_preamble(""), "");
        // "ask openclaw" alone has nothing left after the preamble --
        // keep the original rather than returning an empty string.
        assert_eq!(
            strip_external_preamble("ask openclaw"),
            "ask openclaw"
        );
    }

    #[test]
    fn device_auth_payload_v3_matches_the_gateway_format() {
        // The exact `|`-joined shape the gateway reconstructs and
        // verifies -- mirrors the CLI's buildDeviceAuthPayloadV3.
        let payload = build_device_auth_payload_v3(
            "dev-123",
            "cli",
            "cli",
            "operator",
            &["operator.admin", "operator.read"],
            1737264000000,
            "tok",
            "nonce-abc",
            "Linux",
            "Desktop",
        );
        assert_eq!(
            payload,
            "v3|dev-123|cli|cli|operator|operator.admin,operator.read|1737264000000|tok|nonce-abc|linux|desktop"
        );
    }

    #[test]
    fn device_auth_payload_v3_normalizes_metadata_case() {
        // Uppercase metadata is lowercased (the CLI's
        // normalizeDeviceMetadataForAuth); empty stays empty.
        let payload = build_device_auth_payload_v3(
            "dev", "cli", "cli", "operator", &[], 1, "", "n", "LINUX", "",
        );
        assert_eq!(payload, "v3|dev|cli|cli|operator||1||n|linux|");
    }

    #[test]
    fn normalize_device_metadata_lowercases_ascii_only() {
        assert_eq!(normalize_device_metadata("Linux"), "linux");
        assert_eq!(normalize_device_metadata("  Desktop  "), "desktop");
        assert_eq!(normalize_device_metadata(""), "");
        assert_eq!(normalize_device_metadata("   "), "");
        // Non-ASCII is left alone (matches the CLI's regex on [A-Z]).
        assert_eq!(normalize_device_metadata("Ünïcode"), "Ünïcode");
    }

    #[test]
    fn base64_url_no_pad_omits_padding_and_uses_url_alphabet() {
        // "f" is 0x66 → base64 "Zg==" → URL-safe no-pad "Zg".
        assert_eq!(base64_url_no_pad(b"f"), "Zg");
        // 3 bytes → no padding needed anyway.
        assert_eq!(base64_url_no_pad(b"abc"), "YWJj");
        // Bytes that would produce "+" and "/" in standard base64 use
        // "-" and "_" instead: 0xfb 0xef 0xff → "++//" → "--__".
        assert_eq!(base64_url_no_pad(&[0xfb, 0xef, 0xff]), "--__");
    }
}
