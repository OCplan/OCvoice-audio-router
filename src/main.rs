use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::sync::{mpsc, oneshot};
use tower_http::cors::CorsLayer;
use tray_item::TrayItem;

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
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
struct StopRequest {
    device_index: usize,
    channel_pair: Option<usize>,
}

// ── Tray Types ──────────────────────────────────────────────────────

enum TrayUpdate {
    UpdateAvailable { tag: String, url: String },
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
}

// ── Mixer ────────────────────────────────────────────────────────────
// One stream per device, mixing all active channel pairs in its callback.

/// Per-pair audio data: stereo interleaved samples + playback position
struct PairAudio {
    /// Stereo interleaved PCM (L, R, L, R, ...)
    samples: Arc<Vec<f32>>,
    /// Current read position (in stereo samples, so position/2 = frame)
    position: usize,
}

/// Shared mixer state for one device — locked by the stream callback
struct DeviceMixer {
    /// Active channel pair playback slots
    pairs: HashMap<usize, PairAudio>,
}

/// Per-device state held by the audio thread
struct DeviceState {
    mixer: Arc<Mutex<DeviceMixer>>,
    #[allow(dead_code)]
    stream: cpal::Stream, // held to keep stream alive
    channels: u16,
    sample_rate: u32,
}

/// Commands sent from HTTP handlers to the audio thread
enum AudioCmd {
    Play {
        device_index: usize,
        channel_pair: usize,
        /// Stereo interleaved PCM
        stereo_pcm: Vec<f32>,
        channels: u16,
        sample_rate: u32,
        duration_ms: u64,
        reply: oneshot::Sender<Result<u64, String>>,
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

// ── Auto-Start ──────────────────────────────────────────────────────

/// Install auto-start on login (runs once, idempotent)
fn setup_autostart() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[AutoStart] Could not determine executable path: {e}");
            return;
        }
    };
    let exe_path = exe.to_string_lossy();

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        let agents_dir = format!("{home}/Library/LaunchAgents");
        let plist_path = format!("{agents_dir}/dk.ocplan.audio-router.plist");

        if std::path::Path::new(&plist_path).exists() {
            println!("  Auto-start: already configured");
            return;
        }

        // Ensure LaunchAgents directory exists
        let _ = std::fs::create_dir_all(&agents_dir);

        let plist = format!(
r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>dk.ocplan.audio-router</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe_path}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>StandardOutPath</key>
    <string>{home}/Library/Logs/ocvoice-audio-router.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/Library/Logs/ocvoice-audio-router.log</string>
</dict>
</plist>"#
        );

        match std::fs::write(&plist_path, plist) {
            Ok(_) => println!("  Auto-start: installed (will start on login)"),
            Err(e) => eprintln!("  Auto-start: failed to write plist: {e}"),
        }
    }

    #[cfg(target_os = "windows")]
    {
        use std::process::Command;

        // Check if already registered
        let check = Command::new("reg")
            .args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "OCvoiceAudioRouter"])
            .output();

        if let Ok(output) = check {
            if output.status.success() {
                println!("  Auto-start: already configured");
                return;
            }
        }

        let result = Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v", "OCvoiceAudioRouter",
                "/t", "REG_SZ",
                "/d", &exe_path,
                "/f",
            ])
            .output();

        match result {
            Ok(output) if output.status.success() => {
                println!("  Auto-start: installed (will start on login)");
            }
            Ok(output) => {
                eprintln!("  Auto-start: registry write failed: {}", String::from_utf8_lossy(&output.stderr));
            }
            Err(e) => eprintln!("  Auto-start: failed: {e}"),
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = exe_path;
        println!("  Auto-start: not supported on this platform");
    }
}

// ── Audio Thread ─────────────────────────────────────────────────────
// cpal::Stream is !Send, so all stream management lives on one thread.
// We maintain ONE stream per device. The stream callback mixes all
// active channel pairs from the shared DeviceMixer.

fn run_audio_thread(mut rx: mpsc::UnboundedReceiver<AudioCmd>) {
    // One DeviceState per device_index — stream is reused across all pairs
    let mut devices: HashMap<usize, DeviceState> = HashMap::new();

    // Track active pair count for logging
    let active_pairs = Arc::new(AtomicUsize::new(0));

    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            AudioCmd::Play {
                device_index,
                channel_pair,
                stereo_pcm,
                channels,
                sample_rate,
                duration_ms,
                reply,
            } => {
                // Ensure we have a stream for this device
                let needs_new_stream = match devices.get(&device_index) {
                    None => true,
                    Some(ds) => ds.channels != channels || ds.sample_rate != sample_rate,
                };

                if needs_new_stream {
                    // Drop old stream if config changed
                    devices.remove(&device_index);

                    let result = (|| -> Result<DeviceState, String> {
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

                        let mixer = Arc::new(Mutex::new(DeviceMixer {
                            pairs: HashMap::new(),
                        }));

                        let mixer_cb = mixer.clone();
                        let total_ch = channels as usize;

                        let stream = device
                            .build_output_stream(
                                &config,
                                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                    // Zero the output buffer
                                    for s in data.iter_mut() {
                                        *s = 0.0;
                                    }

                                    let mut mx = mixer_cb.lock().unwrap();
                                    let frame_count = data.len() / total_ch;
                                    let mut finished_pairs = Vec::new();

                                    for (&pair_idx, pair) in mx.pairs.iter_mut() {
                                        let left_ch = pair_idx * 2;
                                        let right_ch = left_ch + 1;

                                        if left_ch >= total_ch || right_ch >= total_ch {
                                            continue;
                                        }

                                        let stereo_len = pair.samples.len();
                                        for frame in 0..frame_count {
                                            let stereo_pos = pair.position + frame * 2;
                                            if stereo_pos >= stereo_len {
                                                finished_pairs.push(pair_idx);
                                                break;
                                            }
                                            let l = pair.samples[stereo_pos];
                                            let r = if stereo_pos + 1 < stereo_len {
                                                pair.samples[stereo_pos + 1]
                                            } else {
                                                l
                                            };
                                            // Mix into output (additive — pairs don't overlap channels)
                                            data[frame * total_ch + left_ch] += l;
                                            data[frame * total_ch + right_ch] += r;
                                        }

                                        // Advance position
                                        let frames_played = frame_count.min(
                                            (stereo_len - pair.position + 1) / 2,
                                        );
                                        pair.position += frames_played * 2;

                                        // Check if done
                                        if pair.position >= stereo_len
                                            && !finished_pairs.contains(&pair_idx)
                                        {
                                            finished_pairs.push(pair_idx);
                                        }
                                    }

                                    for pair_idx in finished_pairs {
                                        mx.pairs.remove(&pair_idx);
                                    }
                                },
                                |err| eprintln!("[AudioRouter] Stream error: {err}"),
                                None,
                            )
                            .map_err(|e| format!("Build stream failed: {e}"))?;

                        stream.play().map_err(|e| format!("Play failed: {e}"))?;

                        Ok(DeviceState {
                            mixer,
                            stream,
                            channels,
                            sample_rate,
                        })
                    })();

                    match result {
                        Ok(ds) => {
                            devices.insert(device_index, ds);
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                            continue;
                        }
                    }
                }

                // Insert pair audio into the device mixer
                if let Some(ds) = devices.get(&device_index) {
                    let mut mx = ds.mixer.lock().unwrap();
                    mx.pairs.insert(
                        channel_pair,
                        PairAudio {
                            samples: Arc::new(stereo_pcm),
                            position: 0,
                        },
                    );
                    let pair_count = mx.pairs.len();
                    active_pairs.store(pair_count, Ordering::Relaxed);
                    let _ = reply.send(Ok(duration_ms));
                } else {
                    let _ = reply.send(Err("Device state missing".to_string()));
                }
            }
            AudioCmd::Stop {
                device_index,
                channel_pair,
                reply,
            } => {
                match channel_pair {
                    Some(pair) => {
                        if let Some(ds) = devices.get(&device_index) {
                            let mut mx = ds.mixer.lock().unwrap();
                            mx.pairs.remove(&pair);
                            if mx.pairs.is_empty() {
                                drop(mx);
                                devices.remove(&device_index);
                            }
                        }
                    }
                    None => {
                        devices.remove(&device_index);
                    }
                }
                active_pairs.store(
                    devices.values().map(|ds| ds.mixer.lock().unwrap().pairs.len()).sum(),
                    Ordering::Relaxed,
                );
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
    let stereo_samples = decode_mp3_to_samples(&audio_bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Decode: {e}")))?;

    // Calculate duration from decoded samples
    let frame_count = stereo_samples.len() / 2;
    let duration_ms = (frame_count as u64 * 1000) / sample_rate as u64;

    // 4. Send stereo PCM to audio thread (mixer handles channel mapping)
    let (reply_tx, reply_rx) = oneshot::channel();
    state
        .audio_tx
        .send(AudioCmd::Play {
            device_index: req.device_index,
            channel_pair: req.channel_pair,
            stereo_pcm: stereo_samples,
            channels: out_channels,
            sample_rate,
            duration_ms,
            reply: reply_tx,
        })
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Audio thread not running".to_string(),
            )
        })?;

    match reply_rx.await {
        Ok(Ok(dur_ms)) => {
            println!(
                "[AudioRouter] Playing on '{}' ch {}-{} (pair {}) duration {}ms",
                device_name,
                req.channel_pair * 2 + 1,
                req.channel_pair * 2 + 2,
                req.channel_pair,
                dur_ms
            );
            Ok(Json(PlayResponse {
                ok: true,
                duration_ms: Some(dur_ms),
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
        duration_ms: None,
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

// ── HTTP Server ─────────────────────────────────────────────────────

async fn run_http_server(audio_tx: mpsc::UnboundedSender<AudioCmd>, port: u16) {
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

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("Failed to bind port");

    axum::serve(listener, app).await.expect("Server error");
}

// ── Auto-Update ─────────────────────────────────────────────────────

async fn check_for_update() -> Option<(String, String)> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.github.com/repos/OCplan/ocvoice-audio-router/releases/latest")
        .header("User-Agent", "ocvoice-audio-router")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let release: GitHubRelease = resp.json::<GitHubRelease>().await.ok()?;
    let remote_tag = release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name);
    let current = env!("CARGO_PKG_VERSION");

    let remote_ver = semver::Version::parse(remote_tag).ok()?;
    let current_ver = semver::Version::parse(current).ok()?;

    if remote_ver > current_ver {
        Some((release.tag_name, release.html_url))
    } else {
        None
    }
}

async fn update_check_loop(tray_tx: std::sync::mpsc::Sender<TrayUpdate>) {
    // Check on startup
    if let Some((tag, url)) = check_for_update().await {
        println!("[Update] New version available: {tag}");
        let _ = tray_tx.send(TrayUpdate::UpdateAvailable { tag, url });
    }

    // Then every 4 hours
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(4 * 60 * 60)).await;
        if let Some((tag, url)) = check_for_update().await {
            println!("[Update] New version available: {tag}");
            let _ = tray_tx.send(TrayUpdate::UpdateAvailable { tag, url });
        }
    }
}

// ── System Tray ─────────────────────────────────────────────────────

fn run_tray(device_count: usize, tray_rx: std::sync::mpsc::Receiver<TrayUpdate>) {
    let mut tray = TrayItem::new(
        "\u{1f50a}", // 🔊
        tray_item::IconSource::Resource(""),
    )
    .expect("Failed to create tray item");

    let version = env!("CARGO_PKG_VERSION");
    let _ = tray.add_label(&format!("OCvoice Audio Router v{version}"));
    let _ = tray.add_label(&format!("{device_count} audio device{}", if device_count == 1 { "" } else { "s" }));

    // Store update URL behind a mutex so the menu callback can read it
    let update_url: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let update_url_click = update_url.clone();

    // Spawn a thread to receive update notifications and store the URL
    std::thread::spawn(move || {
        while let Ok(TrayUpdate::UpdateAvailable { tag, url }) = tray_rx.recv() {
            println!("[Tray] Update indicator set for {tag}");
            *update_url.lock().unwrap() = Some(url);
        }
    });

    // ── Platform-specific menu construction + event loop ──

    #[cfg(target_os = "macos")]
    {
        let _ = tray.add_label(""); // visual gap (no separator on macOS)
        let _ = tray.add_menu_item("Check for Update", move || {
            let url = update_url_click.lock().unwrap();
            if let Some(ref u) = *url {
                let _ = open::that(u);
            } else {
                let _ = open::that("https://github.com/OCplan/ocvoice-audio-router/releases");
            }
        });
        let _ = tray.add_label(""); // visual gap
        tray.inner_mut().add_quit_item("Quit");

        // display() calls NSApp().run() — blocks forever on the main thread
        tray.inner_mut().display();
    }

    #[cfg(target_os = "windows")]
    {
        let _ = tray.inner_mut().add_separator();
        let _ = tray.add_menu_item("Check for Update", move || {
            let url = update_url_click.lock().unwrap();
            if let Some(ref u) = *url {
                let _ = open::that(u);
            } else {
                let _ = open::that("https://github.com/OCplan/ocvoice-audio-router/releases");
            }
        });
        let _ = tray.inner_mut().add_separator();
        let _ = tray.add_menu_item("Quit", || {
            std::process::exit(0);
        });

        // On Windows, tray-item runs its own message loop thread.
        // Park the main thread to keep the process alive.
        loop {
            std::thread::park();
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = update_url_click; // suppress unused warning
        // No tray support — just park the main thread
        loop {
            std::thread::park();
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────
// Main thread: tray-item event loop (AppKit requires main thread on macOS)
// OS thread #1: audio mixer (cpal, !Send)
// OS thread #2: tokio runtime → axum HTTP server + update checker

fn main() {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9876);

    // Print device list on startup
    let host = cpal::default_host();
    println!("\n  OCvoice Audio Router v{}", env!("CARGO_PKG_VERSION"));
    println!("  ─────────────────────────────────");
    let mut device_count = 0;
    if let Ok(devices) = host.output_devices() {
        for (i, dev) in devices.enumerate() {
            let name = dev.name().unwrap_or_else(|_| "?".into());
            let max_ch = dev
                .supported_output_configs()
                .map(|c| c.map(|c| c.channels()).max().unwrap_or(0))
                .unwrap_or(0);
            println!("  [{i}] {name} ({max_ch}ch)");
            device_count = i + 1;
        }
    }

    // Set up auto-start on login
    setup_autostart();

    // Spawn audio thread (cpal::Stream is !Send)
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || run_audio_thread(audio_rx));

    // Create channel for tray updates from the update checker
    let (tray_tx, tray_rx) = std::sync::mpsc::channel();

    // Spawn tokio runtime on a background thread for HTTP server + update checker
    let audio_tx_clone = audio_tx.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        rt.block_on(async {
            tokio::spawn(update_check_loop(tray_tx));
            run_http_server(audio_tx_clone, port).await;
        });
    });

    println!("\n  Listening on http://localhost:{port}");
    println!("  Your web app will detect this automatically.\n");

    // Main thread: run tray event loop (blocks forever)
    run_tray(device_count, tray_rx);
}
