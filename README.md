# OCvoice Audio Router

A lightweight desktop companion for [OCvoice](https://ocvoice.dk) that enables multi-channel audio routing to professional audio interfaces.

## Why?

Web browsers limit audio output to stereo (2 channels) per device. This small app runs locally and receives audio from the OCvoice web app, routing each language translation to specific channel pairs on your multi-channel audio interface — no virtual audio cables needed.

```
OCvoice Web App (browser)
  → HTTP to localhost:9876
  → Audio Router decodes MP3 → PCM
  → Routes to specific channel pair on your interface
  → e.g. Arabic → Ch 1-2, Farsi → Ch 3-4, Turkish → Ch 5-6
```

## Download

Grab the latest release for your platform from [Releases](https://github.com/OCplan/ocvoice-audio-router/releases):

- **macOS (Apple Silicon / M1+)**: `ocvoice-audio-router-macos-arm64`
- **macOS (Intel)**: `ocvoice-audio-router-macos-x64`
- **Windows**: `ocvoice-audio-router-windows-x64.exe`

## Usage

1. Download and run the binary — it starts a local server on port 9876
2. Open your OCvoice broadcast page
3. Multi-channel devices automatically appear in the device selector under "Audio Router"
4. Assign each language to a different channel pair

The OCvoice web app detects the router automatically. No configuration needed.

## When do you need this?

| Setup | Need this? |
|---|---|
| **Dante Virtual Soundcard** | No — each Dante channel appears as a separate browser device |
| **USB interface (EVO 8, Scarlett 18i20, etc.)** | Yes — enables multi-channel routing |
| **Simple setup (one language, speakers/headphones)** | No — browser handles it directly |

## API

The router exposes a simple HTTP API on `localhost:9876`:

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Check if router is running |
| `/devices` | GET | List output devices with real channel counts |
| `/play` | POST | Play audio URL on a specific device + channel pair |
| `/stop` | POST | Stop playback on a device/channel |

## Building from source

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build --release

# The binary is at target/release/ocvoice-audio-router
```

## How it works

- Uses [cpal](https://github.com/RustAudio/cpal) for cross-platform audio device access (CoreAudio on macOS, WASAPI on Windows)
- Decodes MP3 via [symphonia](https://github.com/pdeljanov/Symphonia)
- HTTP server via [axum](https://github.com/tokio-rs/axum)
- Audio streams managed on a dedicated OS thread (cpal requirement)
- Release binary is ~3-5MB with no runtime dependencies

## License

GNU General Public License v3.0 — see [LICENSE](LICENSE).

## Part of OCvoice

Built by [OCplan ApS](https://ocplan.dk). OCvoice is a real-time translation system for multilingual church services.
