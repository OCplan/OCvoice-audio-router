use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::sync::Arc;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::CorsLayer;

// ── Types ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct DeviceInfo {
    index: usize,
    name: String,
    max_channels: u16,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: &'static str,
}

#[derive(Deserialize)]
struct PlayRequest {
    url: String,
    device_index: usize,
    channel_pair: usize,
}

#[derive(Serialize)]
struct PlayResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
struct StopRequest {
    device_index: usize,
    channel_pair: Option<usize>,
}

/// Commands sent from HTTP handlers to the audio thread
enum AudioCmd {
    Play {
        device_index: usize,
        channel_pair: usize,
        pcm: Vec<f32>,
        channels: u16,
        sample_rate: u32,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Stop {
        device_index: usize,
        channel_pair: Option<usize>,
        reply: oneshot::Sender<()>,
    },
}

#[derive(Clone)]
struct AppState {
    audio_tx: mpsc::UnboundedSender<AudioCmd>,
}

// ── Audio Thread ─────────────────────────────────────────────────────
// cpal::Stream is !Send, so all stream management lives on one thread.

fn run_audio_thread(mut rx: mpsc::UnboundedReceiver<AudioCmd>) {
    use std::collections::HashMap;

    // Stream handles keyed by (device_index, channel_pair)
    let mut streams: HashMap<(usize, usize), cpal::Stream> = HashMap::new();

    // Block on receiving commands (this runs on a dedicated OS thread)
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            AudioCmd::Play {
                device_index,
                channel_pair,
                pcm,
                channels,
                sample_rate,
                reply,
            } => {
                let result = (|| -> Result<cpal::Stream, String> {
                    let host = cpal::default_host();
                    let device = host
                        .output_devices()
                        .map_err(|e| format!("Enumerate failed: {e}"))?
                        .nth(device_index)
                        .ok_or(format!("Device {device_index} not found"))?;

                    let config = cpal::StreamConfig {
                        channels,
                        sample_rate: cpal::SampleRate(sample_rate),
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let pcm = Arc::new(pcm);
                    let position = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    let pos_cb = position.clone();
                    let pcm_cb = pcm.clone();

                    let stream = device
                        .build_output_stream(
                            &config,
                            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                let pos =
                                    pos_cb.load(std::sync::atomic::Ordering::Relaxed);
                                let total = pcm_cb.len();
                                for (i, sample) in data.iter_mut().enumerate() {
                                    let idx = pos + i;
                                    *sample = if idx < total { pcm_cb[idx] } else { 0.0 };
                                }
                                let new_pos = (pos + data.len()).min(total);
                                pos_cb.store(new_pos, std::sync::atomic::Ordering::Relaxed);
                            },
                            |err| eprintln!("[AudioRouter] Stream error: {err}"),
                            None,
                        )
                        .map_err(|e| format!("Build stream failed: {e}"))?;

                    stream.play().map_err(|e| format!("Play failed: {e}"))?;
                    Ok(stream)
                })();

                match result {
                    Ok(stream) => {
                        // Drop any existing stream on same device+pair
                        streams.insert((device_index, channel_pair), stream);
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            AudioCmd::Stop {
                device_index,
                channel_pair,
                reply,
            } => {
                match channel_pair {
                    Some(pair) => {
                        streams.remove(&(device_index, pair));
                    }
                    None => {
                        streams.retain(|&(dev, _), _| dev != device_index);
                    }
                }
                let _ = reply.send(());
            }
        }
    }
}

// ── Handlers ─────────────────────────────────────────────────────────

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn list_devices() -> Result<Json<Vec<DeviceInfo>>, (StatusCode, String)> {
    let host = cpal::default_host();
    let mut devices = Vec::new();

    for (i, device) in host
        .output_devices()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to enumerate devices: {e}"),
            )
        })?
        .enumerate()
    {
        let name = device.name().unwrap_or_else(|_| format!("Device {i}"));
        let max_channels = device
            .supported_output_configs()
            .map(|configs| configs.map(|c| c.channels()).max().unwrap_or(0))
            .unwrap_or(0);

        devices.push(DeviceInfo {
            index: i,
            name,
            max_channels,
        });
    }

    Ok(Json(devices))
}

async fn play_audio(
    State(state): State<AppState>,
    Json(req): Json<PlayRequest>,
) -> Result<Json<PlayResponse>, (StatusCode, String)> {
    // 1. Resolve device config (on current thread — device enumeration is Send)
    let host = cpal::default_host();
    let device = host
        .output_devices()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Enumerate failed: {e}"),
            )
        })?
        .nth(req.device_index)
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!("Device {} not found", req.device_index),
        ))?;

    let device_name = device.name().unwrap_or_default();
    let needed_channels = ((req.channel_pair + 1) * 2) as u16;

    let config_range = device
        .supported_output_configs()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No output configs: {e}"),
            )
        })?
        .filter(|c| c.channels() >= needed_channels)
        .max_by_key(|c| c.channels())
        .ok_or((
            StatusCode::BAD_REQUEST,
            format!(
                "Device '{}' doesn't support {} channels (need pair {})",
                device_name, needed_channels, req.channel_pair
            ),
        ))?;

    let out_channels = config_range.channels();
    let sample_rate = config_range.max_sample_rate().0;

    // 2. Fetch audio over HTTP (async, on tokio runtime)
    let audio_bytes = reqwest::get(&req.url)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Fetch failed: {e}")))?
        .bytes()
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Read bytes failed: {e}")))?;

    // 3. Decode MP3 → interleaved stereo PCM
    let samples = decode_mp3_to_samples(&audio_bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Decode: {e}")))?;

    // 4. Map stereo PCM into multi-channel buffer
    let total_ch = out_channels as usize;
    let left_ch = req.channel_pair * 2;
    let right_ch = left_ch + 1;
    let frame_count = samples.len() / 2;
    let mut pcm = vec![0.0f32; frame_count * total_ch];

    for frame in 0..frame_count {
        let l = samples[frame * 2];
        let r = if frame * 2 + 1 < samples.len() {
            samples[frame * 2 + 1]
        } else {
            l
        };
        pcm[frame * total_ch + left_ch] = l;
        pcm[frame * total_ch + right_ch] = r;
    }

    // 5. Send to audio thread for playback
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCmd::Play {
            device_index: req.device_index,
            channel_pair: req.channel_pair,
            pcm,
            channels: out_channels,
            sample_rate,
            reply: reply_tx,
        })
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Audio thread not running".to_string(),
            )
        })?;

    match reply_rx.await {
        Ok(Ok(())) => {
            println!(
                "[AudioRouter] Playing on '{}' ch {}-{} (pair {})",
                device_name,
                left_ch + 1,
                right_ch + 1,
                req.channel_pair
            );
            Ok(Json(PlayResponse {
                ok: true,
                error: None,
            }))
        }
        Ok(Err(e)) => Err((StatusCode::INTERNAL_SERVER_ERROR, e)),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "Audio thread dropped reply".to_string(),
        )),
    }
}

async fn stop_playback(
    State(state): State<AppState>,
    Json(req): Json<StopRequest>,
) -> Result<Json<PlayResponse>, (StatusCode, String)> {
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCmd::Stop {
            device_index: req.device_index,
            channel_pair: req.channel_pair,
            reply: reply_tx,
        })
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Audio thread not running".to_string(),
            )
        })?;

    let _ = reply_rx.await;
    Ok(Json(PlayResponse {
        ok: true,
        error: None,
    }))
}

// ── MP3 Decoder ──────────────────────────────────────────────────────

fn decode_mp3_to_samples(data: &[u8]) -> Result<Vec<f32>, String> {
    let cursor = Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Probe failed: {e}"))?;

    let mut format = probed.format;
    let track = format.default_track().ok_or("No default track")?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Decoder init failed: {e}"))?;

    let mut all_samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => return Err(format!("Packet read error: {e}")),
        };

        if packet.track_id() != track_id {
            continue;
        }

        let decoded = decoder
            .decode(&packet)
            .map_err(|e| format!("Decode error: {e}"))?;
        let spec = *decoded.spec();
        let num_frames = decoded.capacity();

        let mut sample_buf = SampleBuffer::<f32>::new(num_frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        all_samples.extend_from_slice(sample_buf.samples());
    }

    Ok(all_samples)
}

// ── Main ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9876);

    // Spawn a dedicated OS thread for audio (cpal::Stream is !Send)
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || run_audio_thread(audio_rx));

    let state = AppState { audio_tx };

    let app = Router::new()
        .route("/health", get(health))
        .route("/devices", get(list_devices))
        .route("/play", post(play_audio))
        .route("/stop", post(stop_playback))
        .layer(
            CorsLayer::new()
                .allow_origin(tower_http::cors::Any)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any),
        )
        .with_state(state);

    // Print device list on startup
    let host = cpal::default_host();
    println!("\n  OCvoice Audio Router v{}", env!("CARGO_PKG_VERSION"));
    println!("  ─────────────────────────────────");
    if let Ok(devices) = host.output_devices() {
        for (i, dev) in devices.enumerate() {
            let name = dev.name().unwrap_or_else(|_| "?".into());
            let max_ch = dev
                .supported_output_configs()
                .map(|c| c.map(|c| c.channels()).max().unwrap_or(0))
                .unwrap_or(0);
            println!("  [{i}] {name} ({max_ch}ch)");
        }
    }
    println!("\n  Listening on http://localhost:{port}");
    println!("  Your web app will detect this automatically.\n");

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("Failed to bind port");

    axum::serve(listener, app).await.expect("Server error");
}
