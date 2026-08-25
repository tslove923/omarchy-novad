mod classify;
mod pipeline;
mod popup;
mod router;
mod serve;
mod wake;

use std::sync::mpsc;

use clap::{Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use wake::detector::{Detector, WakeWordListener, DEFAULT_PATIENCE, DEFAULT_THRESHOLD};

const OMAPILOT_PLUGIN_ID: &str = "io.github.spencerbull.omapilot";

#[derive(Parser)]
#[command(
    name = "omarchy-novad",
    about = "NPU-accelerated wake word daemon for voxtype"
)]
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
        /// Base URL of a running `omarchy-novad serve` instance, used to
        /// classify each dictated utterance (see pipeline.rs). Only
        /// consulted under the standalone "voxtype" trigger — the
        /// omapilot/custom triggers below don't classify anything
        /// themselves.
        #[arg(long, default_value = "http://127.0.0.1:8420")]
        classify_base_url: String,
        #[arg(long, default_value = "qwen3-1.7b-instruct")]
        classify_model_id: String,
        /// What the wake word triggers.
        ///
        /// "voxtype" (the default): the standalone pipeline (see
        /// pipeline.rs) — `voxtype record start`, wait for it to
        /// auto-stop on its own silence-timeout and transcribe, then
        /// classify and route the result, driving omarchy-novad's popup
        /// through the whole thing.
        ///
        /// "omapilot": runs the same IPC action its own Super+A
        /// keybind uses (`omarchy-shell -q
        /// io.github.spencerbull.omapilot voiceToggle`) — OmaPilot
        /// owns the rest of the flow (listening, the assistant
        /// conversation, dictation via voxtype internally). One
        /// shell-out per detection, no start/stop pairing or
        /// classify/route/popup involvement on omarchy-novad's side.
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

    /// Send one action to a running `omarchy-novad detect`'s standalone popup
    /// (approve/deny a pending command, insert/cancel a dictation
    /// review). Called by the popup's own button clicks via
    /// `Quickshell.Io.Process` — not normally run by hand.
    Respond {
        #[arg(value_parser = ["insert", "cancel", "approve", "deny"])]
        action: String,
    },

    /// Cycle the standalone popup through sample states with no wake
    /// word / mic / model involved — for developing and testing the
    /// QML popup in isolation. Waits for `omarchy-novad respond approve|deny`
    /// during the Confirming phase; press Ctrl+C to stop.
    PopupDemo,

    /// Classify one utterance and print the result. Talks to a running
    /// `omarchy-novad serve` instance — no dedicated classifier model, see
    /// src/classify/mod.rs for why.
    Classify {
        text: String,
        #[arg(long, default_value = "http://127.0.0.1:8420")]
        base_url: String,
        #[arg(long, default_value = "qwen3-coder-30b-a3b")]
        model_id: String,
    },
}

#[derive(Subcommand)]
enum SetupCommand {
    /// Prepare a stock openWakeWord phrase for `omarchy-novad detect` (fetches
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
            classify_base_url,
            classify_model_id,
            on_detect,
        } => run_detect(
            &wakeword,
            &device,
            threshold,
            patience,
            &classify_base_url,
            &classify_model_id,
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
        Command::Classify {
            text,
            base_url,
            model_id,
        } => run_classify(&text, &base_url, &model_id),
    }
}

fn run_classify(text: &str, base_url: &str, model_id: &str) -> anyhow::Result<()> {
    let classifier = classify::Classifier::new(base_url, model_id);
    let result = classifier.classify(text)?;
    println!("intent:   {}", result.intent);
    println!("argument: {}", result.argument);
    println!("latency:  {:.2}s", result.latency.as_secs_f32());
    Ok(())
}

fn run_popup_demo() -> anyhow::Result<()> {
    use popup::{ControlServer, PopupAction, PopupPhase, PopupState};
    use std::time::Duration;

    let rx = ControlServer::spawn()?;
    println!("[omarchy-novad] Popup demo running. Launch the popup with:");
    println!("  qs -p {}/quickshell", env!("CARGO_MANIFEST_DIR"));
    println!("[omarchy-novad] Ctrl+C to stop.\n");

    loop {
        let steps: [(PopupPhase, &str, Option<&str>); 6] = [
            (PopupPhase::Listening, "", None),
            (PopupPhase::Recording, "", None),
            (
                PopupPhase::Transcribing,
                "open firefox and check my calendar",
                None,
            ),
            (
                PopupPhase::Classifying,
                "open firefox and check my calendar",
                None,
            ),
            (
                PopupPhase::Confirming,
                "Run: firefox --new-window calendar.google.com",
                Some("Open Firefox to your calendar?"),
            ),
            (PopupPhase::Ready, "Done.", None),
        ];

        for (phase, text, confirm_label) in steps {
            println!("[omarchy-novad] phase -> {phase:?}");
            popup::write_state(&PopupState {
                phase,
                text: text.to_string(),
                confirm_label: confirm_label.map(String::from),
            });

            if phase == PopupPhase::Confirming {
                println!("[omarchy-novad] waiting for 'omarchy-novad respond approve|deny'...");
                match rx.recv() {
                    Ok(PopupAction::Approve) => println!("[omarchy-novad] approved"),
                    Ok(PopupAction::Deny) => {
                        println!("[omarchy-novad] denied");
                        popup::write_state(&PopupState {
                            phase: PopupPhase::Idle,
                            text: String::new(),
                            confirm_label: None,
                        });
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    Ok(other) => {
                        println!("[omarchy-novad] got {other:?} (expected approve/deny here)")
                    }
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
        .join("omarchy-novad")
        .join("llm-cache")
}

fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad")
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
            // Default: the standalone flow (voxtype dictation, and
            // eventually omarchy-novad's own classify/popup pipeline). Does
            // NOT auto-detect OmaPilot's plugin directory anymore --
            // that dir persists across a disable (uninstalling isn't
            // required to turn it off, see shell.json), so its mere
            // presence on disk was never a reliable "OmaPilot is what
            // I want" signal. Pass `--on-detect omapilot` explicitly
            // to opt back into it.
            println!("[omarchy-novad] wake word will trigger standalone voxtype dictation (pass --on-detect omapilot to use OmaPilot instead).");
            Trigger::VoxtypeDictation
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
    classify_base_url: &str,
    classify_model_id: &str,
    on_detect: Option<String>,
) -> anyhow::Result<()> {
    let trigger = resolve_trigger(on_detect);
    let detector = Detector::new(wakeword, device, &cache_dir(), threshold, patience)?;
    let mut listener = WakeWordListener::new(detector, None);

    println!(
        "[omarchy-novad] Listening for '{wakeword}' (device={device}, threshold={threshold})..."
    );
    println!("[omarchy-novad] Ctrl+C to stop.");

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

    let pipeline_cfg = pipeline::PipelineConfig {
        classify_base_url: classify_base_url.to_string(),
        classify_model_id: classify_model_id.to_string(),
        voxtype_binary: "voxtype".to_string(),
        transcript_path: transcript_path(),
        voxtype_state_path: voxtype_state_path(),
    };

    let chunk_samples = listener.chunk_samples();
    let mut pending: Vec<i16> = Vec::with_capacity(chunk_samples * 2);

    for samples in rx {
        pending.extend_from_slice(&samples);
        while pending.len() >= chunk_samples {
            let chunk: Vec<i16> = pending.drain(..chunk_samples).collect();

            if let Some(detection) = listener.feed(&chunk)? {
                println!(
                    "\n[omarchy-novad] Wake word detected! score={:.3}",
                    detection.score
                );
                match &trigger {
                    // Blocks this thread for the whole session (record
                    // -> transcribe -> classify -> route -> popup) --
                    // deliberately: it means no audio chunk is fed to
                    // the detector while a session is already running,
                    // so there's no risk of double-triggering on the
                    // recording's own audio the way a non-blocking
                    // design would need extra state to prevent. Queued
                    // mic samples just wait in `rx` until this returns.
                    Trigger::VoxtypeDictation => pipeline::run_session(&pipeline_cfg),
                    Trigger::OneShotCommand(cmd) => run_shell(cmd),
                }
                listener.reset();
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

/// Where the standalone pipeline tells voxtype to write each
/// dictation's transcript (`voxtype record start --file=<path>`),
/// scoped under omarchy-novad's own runtime dir rather than voxtype's so a
/// stale file from a crashed session can't be mistaken for voxtype's
/// own state.
fn transcript_path() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("omarchy-novad");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("dictation.txt")
}

/// voxtype's own state file (`"idle"` / `"recording"` /
/// `"transcribing"` / `"streaming"`) -- see voxtype's
/// `state_file = "auto"` config default, which resolves to exactly
/// this path.
fn voxtype_state_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("voxtype")
        .join("state")
}
