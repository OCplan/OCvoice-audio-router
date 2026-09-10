# OCvoice Audio Router

The desktop companion for [OCvoice](https://ocvoice.dk). It routes translated
sound to multi-channel audio interfaces and sends your video with translated
audio to your YouTube broadcasts.

## Download

**YouTube setup is implemented in source version 0.4.0 but is not released yet.**
The macOS release is waiting for the developer account's Apple agreement to be
renewed before notarization can complete. Do not use an older app for the
YouTube setup described below.

The current [v0.3.2 release](https://github.com/OCplan/OCvoice-audio-router/releases/tag/v0.3.2)
supports multi-channel audio only.

The complete 0.4.0 release will contain these files:

| Computer | Planned file |
|---|---|
| Mac with Apple silicon (M-series) | `OCvoice-Audio-Router-macOS-arm64.dmg` |
| Mac with Intel processor | `OCvoice-Audio-Router-macOS-x64.dmg` |
| Windows (64-bit) | `OCvoice-Audio-Router-Windows.zip` |

For the complete release, open the Mac DMG, drag **OCvoice Audio Router** to
Applications, and open it. On Windows, extract the entire ZIP and open
`ocvoice-audio-router.exe`; keep all three files together. The Windows program
is not Authenticode-signed. macOS publication requires signing and notarization.

The app appears in your menu bar or system tray and starts when you log in.

## Send translated broadcasts to YouTube

Use the computer that sends your camera picture, for example the computer
running OBS. Keep OCvoice Audio Router open while broadcasting.

1. In OCvoice, connect your YouTube channel and generate a one-time code to
   connect a computer.
2. Open **YouTube setup / Opsætning** from the Audio Router menu. This opens
   [the local setup page](http://127.0.0.1:9876/setup). Choose English or Dansk.
3. Enter the code and click **Connect computer / Forbind computer**. The page
   shows your connected organization. No backend address is needed.
4. Click **Start connection / Start forbindelse**. Choose languages and create
   the translated broadcasts in OCvoice. Check their visibility before sending
   video. The video receiver starts when OCvoice supplies a broadcast job.
5. In **OBS → Settings → Stream**, choose **Custom**, set **Server** to
   `rtmp://127.0.0.1:1935/live` and **Stream Key** to `ocvoice`. Copy buttons
   are available on the setup page. Use H.264 video and start streaming in OBS.
   The stream key here is a local label; it is not your YouTube stream key.
6. Open each YouTube viewer link from OCvoice. Check the picture, translated
   language and sound together. A running app connection alone does not prove
   video is arriving or YouTube is live.
7. End the broadcasts in OCvoice, stop streaming in OBS, and click **Stop
   connection** in the app's setup page.

Other video programs must support sending RTMP video with the same fields.
The app does not discover cameras automatically. To connect another OCvoice
account, stop the connection and enter a new code. If the page cannot reach the
app, open Audio Router and reload the page. If app components are missing,
reinstall the complete download.

Watching your existing YouTube video with translated sound **inside OCvoice**
does not require this desktop app.

## Multi-channel audio

Browsers usually expose stereo output. Audio Router lets OCvoice route each
language to a channel pair on a multi-channel USB interface. Open your OCvoice
broadcast page, select the interface and assign a channel pair per language.
A simple speaker/headphone setup can use the browser directly.

## Local API

The HTTP server listens on `127.0.0.1:9876`.

| Route | Method | Purpose |
|---|---|---|
| `/health` | GET | App version and availability |
| `/devices` | GET | Audio devices and channel counts |
| `/play` | POST | Play audio on a device/channel pair |
| `/stop` | POST | Stop device/channel playback |
| `/setup` | GET | Local YouTube setup page (also `/setup.js`, `/setup.css`) |
| `/restream/setup` | GET | Safe setup status and OBS fields |
| `/restream/pair` | POST | Pair using `{ "code": "…" }` |
| `/restream/start` | POST | Start the engine connection |
| `/restream/stop` | POST | Stop the engine connection |
| `/restream/status` | GET | Engine supervisor status |

The legacy audio routes allow cross-origin browser requests. Setup and restream
control require a localhost Host and reject remote Origins. They do not enable
CORS. Open the local setup page to pair; do not POST from the product website.
Device credentials are never returned by the API. The public product backend
is supplied by the app.

## Build and test

Install [Rust](https://rustup.rs), then run `cargo build --release`, `cargo test`
and `cargo clippy --all-targets -- -D warnings`. Audio runs on a dedicated thread
using cpal; the HTTP service uses Axum. Release CI bundles the pinned engine and
video binaries, verifies their digests and capabilities, and signs/notarizes
macOS bundles. A raw Cargo build alone does not include these components.

For development, `PORT`, `RESTREAM_CONVEX_URL`, `RESTREAM_ENGINE_PATH`,
`RESTREAM_FFMPEG_PATH` and `RESTREAM_RTMP_PORT` override defaults. The ignored
`setup_browser_preview` test serves synthetic pairing data on port 19876 for
manual UI checks; it never contacts the product backend.

Before a release, update `Cargo.toml`, `Cargo.lock` and both version fields in
`resources/macos/Info.plist`, then run `scripts/check-version-tag.sh v0.4.0`.
The updater selects versioned app releases and ignores dependency releases.

## License

[GNU GPL v3.0](LICENSE). [Bundled video component source and build information](https://github.com/OCplan/OCvoice-audio-router/releases/tag/ffmpeg-deps-9.0.1).
Built by [OCplan ApS](https://ocplan.dk).
