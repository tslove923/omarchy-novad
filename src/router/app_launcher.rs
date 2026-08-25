//! Launch desktop applications. Port of nova-npu's
//! `ai/commands/app_launcher.py`, Linux/Hyprland path only — novad has
//! no Windows target, unlike nova.

use std::process::{Command, Stdio};

/// Common aliases: spoken name -> executable/command. Not exhaustive —
/// nova's own list wasn't either; unrecognized names fall through to
/// the normalized name itself and let the launcher try (see
/// `resolve`), same as the Python original.
const ALIASES: &[(&str, &str)] = &[
    ("firefox", "firefox"),
    ("zen", "zen-browser"),
    ("zen browser", "zen-browser"),
    ("chrome", "google-chrome-stable"),
    ("google chrome", "google-chrome-stable"),
    ("chromium", "chromium"),
    ("brave", "brave"),
    ("edge", "microsoft-edge"),
    ("microsoft edge", "microsoft-edge"),
    ("terminal", "kitty"),
    ("kitty", "kitty"),
    ("alacritty", "alacritty"),
    ("foot", "foot"),
    ("konsole", "konsole"),
    ("code", "code"),
    ("vs code", "code"),
    ("vscode", "code"),
    ("visual studio code", "code"),
    ("vim", "kitty vim"),
    ("neovim", "kitty nvim"),
    ("emacs", "emacs"),
    ("discord", "discord"),
    ("slack", "slack"),
    ("telegram", "telegram-desktop"),
    ("signal", "signal-desktop"),
    ("spotify", "spotify-launcher"),
    ("spotify launcher", "spotify-launcher"),
    ("vlc", "vlc"),
    ("mpv", "mpv"),
];

fn which(bin: &str) -> bool {
    // First word only -- entries like "kitty vim" are "kitty" with an
    // argument, and it's kitty's presence that matters for `which`.
    let Some(prog) = bin.split_whitespace().next() else {
        return false;
    };
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(prog).is_file()))
        .unwrap_or(false)
}

fn resolve(app_name: &str) -> String {
    let lower = app_name.trim().to_lowercase();
    if let Some((_, cmd)) = ALIASES.iter().find(|(k, _)| *k == lower) {
        return cmd.to_string();
    }
    if which(&lower) {
        return lower;
    }
    let hyphenated = lower.replace(' ', "-");
    if which(&hyphenated) {
        return hyphenated;
    }
    lower
}

pub fn open_app(app_name: &str) -> (bool, String) {
    let cmd = resolve(app_name);
    tracing::debug!("[router:app] launching {cmd:?} (from {app_name:?})");

    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let Some((program, args)) = parts.split_first() else {
        return (false, format!("Failed to launch {app_name}: empty command"));
    };

    let result = if which("hyprctl") {
        Command::new("hyprctl")
            .args(["dispatch", "exec", &cmd])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    } else {
        Command::new(program)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };

    match result {
        Ok(_) => (true, format!("Launched {app_name}")),
        Err(e) => {
            tracing::warn!("[router:app] failed to launch {cmd:?}: {e}");
            (false, format!("Failed to launch {app_name}: {e}"))
        }
    }
}
