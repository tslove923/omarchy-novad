mod wake;

use std::sync::mpsc;

use clap::{Parser, Subcommand};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use wake::detector::{Detector, WakeWordListener, DEFAULT_PATIENCE, DEFAULT_THRESHOLD};

#[derive(Parser)]
#[command(name = "novad", about = "NPU-accelerated wake word daemon for voxtype")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen for the wake word and hand off to voxtype on detection.
    ///
    /// Phase 1: no intent classification yet. Every detection starts a
    /// plain voxtype recording and stops it after a short silence
    /// timeout — the same "wake word -> dictation" flow as nova-npu's
    /// original, minus the AI routing layered on top in later phases.
    Detect {
        #[arg(long, default_value = "hey_jarvis")]
        wakeword: String,
        #[arg(long, default_value = "NPU")]
        device: String,
        #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
        threshold: f32,
        #[arg(long, default_value_t = DEFAULT_PATIENCE)]
        patience: usize,
        /// Seconds of near-silence after speech before auto-stopping the
        /// recording (mirrors nova's wake-word silence timeout).
        #[arg(long, default_value_t = 1.5)]
        silence_timeout_secs: f32,
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
        } => run_detect(
            &wakeword,
            &device,
            threshold,
            patience,
            silence_timeout_secs,
        ),
    }
}

fn cache_dir() -> std::path::PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("novad")
        .join("wake-cache")
}

fn run_detect(
    wakeword: &str,
    device: &str,
    threshold: f32,
    patience: usize,
    silence_timeout_secs: f32,
) -> anyhow::Result<()> {
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
    let mut recording = false;
    let mut last_speech = std::time::Instant::now();
    let silence_timeout = std::time::Duration::from_secs_f32(silence_timeout_secs);

    for samples in rx {
        pending.extend_from_slice(&samples);
        while pending.len() >= chunk_samples {
            let chunk: Vec<i16> = pending.drain(..chunk_samples).collect();

            if !recording {
                if let Some(detection) = listener.feed(&chunk)? {
                    println!("\n[novad] Wake word detected! score={:.3}", detection.score);
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
            } else {
                // Cheap RMS gate for the silence-timeout — not a real
                // VAD, just enough to know "still talking" vs "quiet".
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

fn rms_i16(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
    ((sum_sq / samples.len() as f64).sqrt()) as f32
}
