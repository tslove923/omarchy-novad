//! Execute shell commands from voice input. Port of nova-npu's
//! `ai/commands/terminal.py`, Linux patterns only (nova also blocked
//! several Windows-specific dangerous patterns and allowlisted several
//! PowerShell read-only cmdlets; omarchy-novad has no Windows target).
//!
//! Terminal commands always require user confirmation in the popup
//! before execution, UNLESS they match [`is_safe_readonly`] -- see
//! `router::route`, which only calls [`run`] directly for the safe
//! case and routes everything else through
//! `RouteResult::NeedsConfirmation` first.

use std::process::Command;
use std::time::Duration;

/// Dangerous patterns blocked unconditionally, regardless of
/// confirmation -- these never reach the popup at all, matching
/// nova's `is_blocked` gate inside `run_terminal` itself. Checked as
/// plain substring/prefix tests rather than regex: omarchy-novad has no
/// regex crate dependency yet and every one of nova's patterns here
/// is expressible without one (word-boundary regexes like `\bsudo\b`
/// become "contains sudo as a whole word", checked via
/// `contains_word`).
const BLOCKED_WORDS: &[&str] = &[
    "mkfs", "chmod", // narrowed further below (777 + /)
    "sudo", "shutdown", "reboot", "poweroff", "halt",
];

fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .any(|tok| tok == word)
}

fn is_blocked(command: &str) -> bool {
    let lower = command.to_lowercase();

    for w in BLOCKED_WORDS {
        if contains_word(&lower, w) {
            return true;
        }
    }

    // rm -rf / -f / -r / --force / --recursive (any order/spacing of
    // the flag)
    if let Some(rm_pos) = lower.find("rm ") {
        let after = &lower[rm_pos + 3..];
        let dangerous_flag = after.split_whitespace().take(3).any(|tok| {
            (tok.starts_with('-')
                && !tok.starts_with("--")
                && (tok.contains('f') || tok.contains('r')))
                || tok == "--force"
                || tok == "--recursive"
        });
        if dangerous_flag {
            return true;
        }
        // rm -<flags> / or ~ or $HOME
        let target_root = after
            .split_whitespace()
            .any(|tok| tok == "/" || tok == "~" || tok == "$home");
        if target_root && after.trim_start().starts_with('-') {
            return true;
        }
    }

    if lower.contains("mkfs") {
        return true;
    }
    if lower.contains("dd ") && lower.contains("of=/dev/") {
        return true;
    }
    if lower.contains("> /dev/sd") {
        return true;
    }
    if lower.contains("chmod") && lower.contains("777") && lower.contains(" /") {
        return true;
    }
    if lower.contains("chown") && lower.contains(" /") {
        return true;
    }
    if contains_word(&lower, "su") && lower.contains("su -") {
        return true;
    }
    if lower.contains(":(){") || lower.contains(": () {") {
        return true; // fork bomb
    }
    if contains_word(&lower, "init") && (lower.contains("init 0") || lower.contains("init 6")) {
        return true;
    }
    if lower.contains("systemctl")
        && ["start", "stop", "enable", "disable", "mask"]
            .iter()
            .any(|op| lower.contains(&format!("systemctl {op}")))
    {
        return true;
    }
    let pipes_to_shell = ["| sh", "|sh", "| bash", "|bash"]
        .iter()
        .any(|p| lower.contains(p));
    if (lower.contains("curl") || lower.contains("wget")) && pipes_to_shell {
        return true;
    }

    false
}

/// Known safe read-only commands -- skip confirmation for these.
const SAFE_PREFIXES: &[&str] = &[
    "ls",
    "ll",
    "la",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "wc",
    "du",
    "df",
    "pwd",
    "whoami",
    "hostname",
    "uname",
    "date",
    "cal",
    "uptime",
    "free",
    "top",
    "htop",
    "btop",
    "ps",
    "pgrep",
    "ip addr",
    "ip link",
    "ip route",
    "ping",
    "dig",
    "nslookup",
    "traceroute",
    "echo",
    "printf",
    "file",
    "stat",
    "which",
    "type",
    "command -v",
    "find",
    "locate",
    "fd",
    "grep",
    "rg",
    "ag",
    "tree",
    "env",
    "printenv",
    "sensors",
    "lsblk",
    "lsusb",
    "lspci",
    "journalctl",
    "dmesg",
];

pub fn is_safe_readonly(command: &str) -> bool {
    let stripped = command.trim();
    SAFE_PREFIXES
        .iter()
        .any(|prefix| stripped == *prefix || stripped.starts_with(&format!("{prefix} ")))
}

pub fn run(command: &str) -> (bool, String) {
    let clean = command.trim();
    if clean.is_empty() {
        return (false, "No command provided".to_string());
    }

    if is_blocked(clean) {
        tracing::warn!("[router:terminal] BLOCKED dangerous command: {clean}");
        return (
            false,
            "\u{26a0} Blocked: command matches a safety filter".to_string(),
        );
    }

    tracing::debug!("[router:terminal] executing: {clean}");

    let result = run_with_timeout(clean, Duration::from_secs(15));
    match result {
        Some(output) => {
            let stdout = output.stdout.trim();
            let stderr = output.stderr.trim();
            if output.success {
                let text = if stdout.is_empty() {
                    "(no output)".to_string()
                } else {
                    truncate(stdout, 2000)
                };
                (true, text)
            } else {
                let msg = if !stderr.is_empty() {
                    stderr
                } else if !stdout.is_empty() {
                    stdout
                } else {
                    "command failed"
                };
                (false, truncate(msg, 1000))
            }
        }
        None => (false, "Command timed out (15s limit)".to_string()),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}\n\u{2026} (truncated)", &s[..max])
    }
}

struct CommandOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Runs `command` via `sh -c`, waiting up to `timeout` before killing
/// it and returning `None`. `std::process` has no built-in timeout, so
/// this polls `try_wait` rather than blocking indefinitely on `wait`
/// like the untimed calls elsewhere in this crate (media_control,
/// system_control) can afford to.
fn run_with_timeout(command: &str, timeout: Duration) -> Option<CommandOutput> {
    use std::io::Read;
    use std::process::Stdio;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    };

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }

    Some(CommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_rm_rf() {
        assert!(is_blocked("rm -rf /home/user/project"));
        assert!(is_blocked("rm -fr ~/Downloads"));
        assert!(is_blocked("rm --recursive --force /tmp/x"));
    }

    #[test]
    fn allows_plain_rm() {
        assert!(!is_blocked("rm file.txt"));
        assert!(!is_blocked("rm -i confirmed.txt"));
    }

    #[test]
    fn blocks_sudo_as_whole_word() {
        assert!(is_blocked("sudo rm file.txt"));
        // "sudoku" contains "sudo" as a substring but not as a whole
        // word -- contains_word must not false-positive on it.
        assert!(!is_blocked("echo sudoku"));
    }

    #[test]
    fn blocks_curl_pipe_shell_but_not_curl_alone() {
        assert!(is_blocked("curl https://example.com/install.sh | sh"));
        assert!(is_blocked("curl -s https://example.com | bash"));
        assert!(!is_blocked("curl -s https://example.com/data.json"));
        // The bug this regression-tests: an operator-precedence slip
        // once made ANY command containing "|sh" get blocked, curl/
        // wget or not (e.g. this find command, which has no shell
        // pipe at all -- "|sh" only appears here because "history" and
        // "sh" collide in the substring, not because of an actual `|`
        // pipe to a shell).
        assert!(!is_blocked("find . -iname '*history|sh*'"));
    }

    #[test]
    fn blocks_fork_bomb() {
        assert!(is_blocked(":(){ :|:& };:"));
    }

    #[test]
    fn blocks_dangerous_systemctl_ops() {
        assert!(is_blocked("systemctl stop sshd"));
        assert!(is_blocked("systemctl disable firewalld"));
    }

    #[test]
    fn allows_readonly_systemctl() {
        assert!(!is_blocked("systemctl status sshd"));
    }

    #[test]
    fn safe_readonly_matches_known_prefixes() {
        assert!(is_safe_readonly("ls -la"));
        assert!(is_safe_readonly("ps"));
        assert!(is_safe_readonly("grep foo bar.txt"));
        assert!(!is_safe_readonly("rm file.txt"));
        // Prefix must be a whole leading word, not any substring --
        // "lsof" isn't "ls" plus an argument.
        assert!(!is_safe_readonly("lsof -i :8080"));
    }
}
