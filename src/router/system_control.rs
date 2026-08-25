//! Volume and brightness control. Port of nova-npu's
//! `ai/commands/system_control.py`, Linux path only (`wpctl` /
//! `brightnessctl` — both confirmed present on this system; nova's
//! Windows backends via nircmd/pycaw have no novad equivalent).

use std::process::Command;

fn run_cmd(program: &str, args: &[&str]) -> (bool, String) {
    match Command::new(program).args(args).output() {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

enum Category {
    Volume,
    Brightness,
    Unknown,
}

/// Mirrors nova's `_parse_action`: natural language -> (category, direction).
/// `direction` is one of "up", "down", "mute", "unmute", "max", "min".
fn parse_action(action: &str) -> (Category, &'static str) {
    let lower = action.to_lowercase();

    let is_volume = ["volume", "sound", "audio", "mute", "unmute"]
        .iter()
        .any(|w| lower.contains(w));
    if is_volume {
        if lower.contains("mute") && !lower.contains("unmute") {
            return (Category::Volume, "mute");
        }
        if lower.contains("unmute") {
            return (Category::Volume, "unmute");
        }
        if ["up", "increase", "raise", "louder", "higher"]
            .iter()
            .any(|w| lower.contains(w))
        {
            return (Category::Volume, "up");
        }
        if ["down", "decrease", "lower", "quieter", "softer"]
            .iter()
            .any(|w| lower.contains(w))
        {
            return (Category::Volume, "down");
        }
        if ["max", "maximum", "full"].iter().any(|w| lower.contains(w)) {
            return (Category::Volume, "max");
        }
        if ["min", "minimum"].iter().any(|w| lower.contains(w)) {
            return (Category::Volume, "min");
        }
        return (Category::Volume, "up");
    }

    let is_brightness = ["brightness", "bright", "screen", "display", "dim"]
        .iter()
        .any(|w| lower.contains(w));
    if is_brightness {
        if ["up", "increase", "raise", "brighter", "higher"]
            .iter()
            .any(|w| lower.contains(w))
        {
            return (Category::Brightness, "up");
        }
        if ["down", "decrease", "lower", "dim", "darker"]
            .iter()
            .any(|w| lower.contains(w))
        {
            return (Category::Brightness, "down");
        }
        if ["max", "maximum", "full"].iter().any(|w| lower.contains(w)) {
            return (Category::Brightness, "max");
        }
        if ["min", "minimum"].iter().any(|w| lower.contains(w)) {
            return (Category::Brightness, "min");
        }
        return (Category::Brightness, "up");
    }

    (Category::Unknown, "")
}

fn volume(direction: &str) -> (bool, String) {
    if !which("wpctl") {
        return (
            false,
            "wpctl not found (WirePlumber not installed)".to_string(),
        );
    }
    let sink = "@DEFAULT_AUDIO_SINK@";
    match direction {
        "mute" => {
            let (ok, out) = run_cmd("wpctl", &["set-mute", sink, "1"]);
            (
                ok,
                if ok {
                    "Muted".to_string()
                } else {
                    format!("Mute failed: {out}")
                },
            )
        }
        "unmute" => {
            let (ok, out) = run_cmd("wpctl", &["set-mute", sink, "0"]);
            (
                ok,
                if ok {
                    "Unmuted".to_string()
                } else {
                    format!("Unmute failed: {out}")
                },
            )
        }
        "max" => {
            let (ok, out) = run_cmd("wpctl", &["set-volume", sink, "1.0"]);
            (
                ok,
                if ok {
                    "Volume set to 100%".to_string()
                } else {
                    format!("Failed: {out}")
                },
            )
        }
        "min" => {
            let (ok, out) = run_cmd("wpctl", &["set-volume", sink, "0.0"]);
            (
                ok,
                if ok {
                    "Volume set to 0%".to_string()
                } else {
                    format!("Failed: {out}")
                },
            )
        }
        _ => {
            let step = if direction == "up" { "5%+" } else { "5%-" };
            let (ok, out) = run_cmd("wpctl", &["set-volume", sink, step]);
            (
                ok,
                if ok {
                    format!("Volume {direction}")
                } else {
                    format!("Volume change failed: {out}")
                },
            )
        }
    }
}

fn brightness(direction: &str) -> (bool, String) {
    if !which("brightnessctl") {
        return (false, "brightnessctl not found".to_string());
    }
    match direction {
        "max" => {
            let (ok, out) = run_cmd("brightnessctl", &["set", "100%"]);
            (
                ok,
                if ok {
                    "Brightness set to 100%".to_string()
                } else {
                    format!("Failed: {out}")
                },
            )
        }
        "min" => {
            let (ok, out) = run_cmd("brightnessctl", &["set", "5%"]);
            (
                ok,
                if ok {
                    "Brightness set to minimum".to_string()
                } else {
                    format!("Failed: {out}")
                },
            )
        }
        _ => {
            let step = if direction == "up" { "10%+" } else { "10%-" };
            let (ok, out) = run_cmd("brightnessctl", &["set", step]);
            (
                ok,
                if ok {
                    format!("Brightness {direction}")
                } else {
                    format!("Brightness change failed: {out}")
                },
            )
        }
    }
}

pub fn run(action: &str) -> (bool, String) {
    let (category, direction) = parse_action(action);
    tracing::debug!("[router:sys] action={action:?} direction={direction:?}");
    match category {
        Category::Volume => volume(direction),
        Category::Brightness => brightness(direction),
        Category::Unknown => (false, format!("Unknown system control: {action}")),
    }
}
