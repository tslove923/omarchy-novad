mod popup;
mod serve;
mod wake;

use std::sync::mpsc;

use clap::{Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use wake::detector::{Detector, WakeWordListener, DEFAULT_PATIENCE, DEFAULT_THRESHOLD};

const OMAPILOT_PLUGIN_ID: &str = "io.github.spencerbull.omapilot";

#[derive(Parser)]
#[command(name = "novad", about = "NPU-accelerated wake word daemon for voxtype")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen for the wake word and hand off to voxtype (or a
    /// configured assistant) on detection.
    Detect {
        #[arg(long, default_value = "hey_jarvis")]
        wakeword: String,
        #[arg(long, default_value = "NPU")]
        device: String,
        #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
        threshold: f32,
        #[arg(long, default_value_t = DEFAULT_PATIENCE)]
        patience: usize,
        /// Seconds of near-silence after speech before auto-stopping a
        /// plain voxtype recording. Not used for the omapilot/custom
        /// triggers below — those own their own session lifecycle.
        #[arg(long, default_value_t = 1.5)]
        silence_timeout_secs: f32,
        /// What the wake word triggers.
        ///
        /// "voxtype" (default when OmaPilot isn't installed): plain
        /// dictation — `voxtype record start`, then `record stop` on
        /// the silence timeout above.
        ///
        /// "omapilot" (default when OmaPilot's plugin dir is present):
        /// runs the same IPC action its own Super+A keybind uses
        /// (`omarchy-shell -q io.github.spencerbull.omapilot
        /// voiceToggle`) — OmaPilot owns the rest of the flow
        /// (listening, the assistant conversation, dictation via
        /// voxtype internally). One shell-out per detection, no
        /// start/stop pairing on novad's side.
        ///
        /// Anything else is run verbatim as a shell command on
        /// detection, same one-shot semantics as "omapilot".
        #[arg(long)]
        on_detect: Option<String>,
    },

    /// Serve a local model over an OpenAI-compatible HTTP API
    /// (`/v1/chat/completions`) so OmaPilot — or anything else that
    /// speaks that protocol — can use it as a local provider. See
    /// docs/omapilot-local-provider.md for the models.json entry.
    Serve {
        /// Path to an OpenVINO IR model directory (e.g. a downloaded
        /// `OpenVINO/*-int4-ov` repo).
        #[arg(long)]
        model: std::path::PathBuf,
        #[arg(long, default_value = "GPU")]
        device: String,
        /// Id reported in API responses and /v1/models (this is what
        /// goes in the OmaPilot models.json "id" field).
        #[arg(long, default_value = "novad-local")]
        model_id: String,
        #[arg(long, default_value_t = 8420)]
        port: u16,
    },

    /// One-time setup tasks.
    Setup {
        #[command(subcommand)]
        what: SetupCommand,
    },

    /// Send one action to a running `novad detect`'s standalone popup
    /// (approve/deny a pending command, insert/cancel a dictation
    /// review). Called by the popup's own button clicks via
    /// `Quickshell.Io.Process` — not normally run by hand.
    Respond {
        #[arg(value_parser = ["insert", "cancel", "approve", "deny"])]
        action: String,
    },

    /// Cycle the standalone popup through sample states with no wake
    /// word / mic / model involved — for developing and testing the
    /// QML popup in isolation. Waits for `novad respond approve|deny`
    /// during the Confirming phase; press Ctrl+C to stop.
    PopupDemo,
}

#[derive(Subcommand)]
enum SetupCommand {
    /// Prepare a stock openWakeWord phrase for `novad detect` (fetches
    /// the model and removes an ONNX If-node the NPU can't run, via a
    /// throwaway `uv`-managed Python env — nothing installed
    /// persistently). Not for training a new phrase — see the wake
    /// module's docs for that.
    WakeModel {
        #[arg(default_value = "hey_jarvis")]
        wakeword: String,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Detect {
            wakeword,
            device,
            threshold,
            patience,
            silence_timeout_secs,
            on_detect,
        } => run_detect(
            &wakeword,
            &device,
            threshold,
            patience,
            silence_timeout_secs,
            on_detect,
        ),
        Command::Serve {
            model,
            device,
            model_id,
            port,
        } => {
            let config = serve::ServeConfig {
                model_path: model,
                device,
                cache_dir: serve_cache_dir(),
                model_id,
                port,
            };
            tokio::runtime::Runtime::new()?.block_on(serve::run(config))
        }
        Command::Setup {
            what: SetupCommand::WakeModel { wakeword },
        } => Ok(wake::setup::run(&wakeword)?),
        Command::Respond { action } => popup::respond(&action),
        Command::PopupDemo => run_popup_demo(),
    }
}

fn run_popup_demo() -> anyhow::Result<()> {
    use popup::{ControlServer, PopupAction, PopupPhase, PopupState};
    use std::time::Duration;

    let rx = ControlServer::spawn()?;
    println!("[novad] Popup demo running. Launch the popup with:");
    println!("  qs -p {}/quickshell", env!("CARGO_MANIFEST_DIR"));
    println!("[novad] Ctrl+C to stop.\n");

    loop {
        let steps: [(PopupPhase, &str, Option<&str>); 6] = [
            (PopupPhase::Listening, "", None),
            (PopupPhase::Recording, "", None),
            (PopupPhase::Transcribing, "open firefox and check my calendar", None),
            (PopupPhase::Classifying, "open firefox and check my calendar", None),
            (
                PopupPhase::Confirming,
                "Run: firefox --new-window calendar.google.com",
                Some("Open Firefox to your calendar?"),
            ),
            (PopupPhase::Ready, "Done.", None),
        ];

        for (phase, text, confirm_label) in steps {
            println!("[novad] phase -> {phase:?}");
            popup::write_state(&PopupState { phase, text: text.to_string(), confirm_label: confirm_label.map(String::from) });

            if phase == PopupPhase::Confirming {
                println!("[novad] waiting for 'novad respond approve|deny'...");
                match rx.recv() {
                    Ok(PopupAction::Approve) => println!("[novad] approved"),
                    Ok(PopupAction::Deny) => {
                        println!("[novad] denied");
                        popup::write_state(&PopupState { phase: PopupPhase::Idle, text: String::new(), confirm_label: None });
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    Ok(other) => println!("[novad] got {other:?} (expected approve/deny here)"),
                    Err(_) => return Ok(()),
                }
            } else {
                std::thread::sleep(Duration::from_secs(2));
            }
        }

        popup::write_state(&PopupState::default());
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn serve_cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("novad")
        .join("llm-cache")
}

fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("novad")
        .join("wake-cache")
}

/// A wake-word detection either starts a plain voxtype recording
/// (start now, stop on silence) or fires a one-shot command that owns
/// its own session lifecycle (e.g. OmaPilot's voiceToggle).
enum Trigger {
    VoxtypeDictation,
    OneShotCommand(String),
}

fn resolve_trigger(on_detect: Option<String>) -> Trigger {
    match on_detect.as_deref() {
        Some("voxtype") => Trigger::VoxtypeDictation,
        Some("omapilot") => Trigger::OneShotCommand(omapilot_voice_toggle_cmd()),
        Some(custom) => Trigger::OneShotCommand(custom.to_string()),
        None => {
            // Auto-detect: if OmaPilot's plugin directory exists, prefer
            // it — richer assistant flow, and it already integrates
            // with voxtype for dictation internally. Otherwise fall
            // back to plain voxtype dictation.
            let omapilot_dir = dirs::config_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("omarchy")
                .join("plugins")
                .join(OMAPILOT_PLUGIN_ID);
            if omapilot_dir.is_dir() {
                println!(
                    "[novad] OmaPilot plugin detected at {} — wake word will trigger it.",
                    omapilot_dir.display()
                );
                Trigger::OneShotCommand(omapilot_voice_toggle_cmd())
            } else {
                println!("[novad] OmaPilot not installed — wake word will trigger plain voxtype dictation.");
                Trigger::VoxtypeDictation
            }
        }
    }
}

fn omapilot_voice_toggle_cmd() -> String {
    format!("omarchy-shell -q {OMAPILOT_PLUGIN_ID} voiceToggle")
}

fn run_detect(
    wakeword: &str,
    device: &str,
    threshold: f32,
    patience: usize,
    silence_timeout_secs: f32,
    on_detect: Option<String>,
) -> anyhow::Result<()> {
    let trigger = resolve_trigger(on_detect);
    let detector = Detector::new(wakeword, device, &cache_dir(), threshold, patience)?;
    let mut listener = WakeWordListener::new(detector, None);

    println!("[novad] Listening for '{wakeword}' (device={device}, threshold={threshold})...");
    println!("[novad] Ctrl+C to stop.");

    // cpal mic capture -> mpsc channel -> chunked feed into the detector.
    // Kept off the audio callback thread: NPU inference (even ~2ms/frame)
    // has no place running inside a realtime audio callback.
    let host = cpal::default_host();
    let device_in = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
    let config = cpal::StreamConfig {
        channels: 1,
        sample_rate: cpal::SampleRate(listener.sample_rate()),
        buffer_size: cpal::BufferSize::Default,
    };

    let (tx, rx) = mpsc::channel::<Vec<i16>>();
    let stream = device_in.build_input_stream(
        &config,
        move |data: &[f32], _| {
            let samples: Vec<i16> = data.iter().map(|&s| (s * i16::MAX as f32) as i16).collect();
            let _ = tx.send(samples);
        },
        |err| tracing::error!("audio stream error: {err}"),
        None,
    )?;
    stream.play()?;

    let chunk_samples = listener.chunk_samples();
    let mut pending: Vec<i16> = Vec::with_capacity(chunk_samples * 2);
    let mut recording = false; // only meaningful for Trigger::VoxtypeDictation
    let mut last_speech = std::time::Instant::now();
    let silence_timeout = std::time::Duration::from_secs_f32(silence_timeout_secs);

    for samples in rx {
        pending.extend_from_slice(&samples);
        while pending.len() >= chunk_samples {
            let chunk: Vec<i16> = pending.drain(..chunk_samples).collect();

            if !recording {
                if let Some(detection) = listener.feed(&chunk)? {
                    println!("\n[novad] Wake word detected! score={:.3}", detection.score);
                    match &trigger {
                        Trigger::VoxtypeDictation => {
                            if let Err(e) = std::process::Command::new("voxtype")
                                .args(["record", "start"])
                                .status()
                            {
                                tracing::error!("failed to run 'voxtype record start': {e}");
                                continue;
                            }
                            recording = true;
                            last_speech = std::time::Instant::now();
                        }
                        Trigger::OneShotCommand(cmd) => {
                            run_shell(cmd);
                            listener.reset();
                        }
                    }
                }
            } else {
                // Cheap RMS gate for the silence-timeout — not a real
                // VAD, just enough to know "still talking" vs "quiet".
                // Only reached under Trigger::VoxtypeDictation.
                let rms = rms_i16(&chunk);
                if rms > 300.0 {
                    last_speech = std::time::Instant::now();
                }
                if last_speech.elapsed() >= silence_timeout {
                    if let Err(e) = std::process::Command::new("voxtype")
                        .args(["record", "stop"])
                        .status()
                    {
                        tracing::error!("failed to run 'voxtype record stop': {e}");
                    }
                    recording = false;
                    listener.reset();
                }
            }
        }
    }

    Ok(())
}

fn run_shell(cmd: &str) {
    let result = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match result {
        Ok(_) => tracing::info!("Ran on-detect trigger: {cmd}"),
        Err(e) => tracing::error!("Failed to run on-detect trigger '{cmd}': {e}"),
    }
}

fn rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum_sq / samples.len() as f64).sqrt()) as f32
}
