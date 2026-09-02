# OCvoice Audio Router — notes for agents working in this repo

`README.md` is the user-facing document: what the app is for, how to download and
run it, and how to build it from source. This file does not repeat any of that.
It covers the things a change here can get wrong that reading the README would
not warn you about.

## 0. This repository is public

`OCplan/OCvoice-audio-router` is a public GitHub repository with public
releases. Anything committed is published in the same second — no secrets, no
internal filesystem paths, no names of private repositories. Assume every line
you write here is read by strangers, because it is.

Releases are irreversible. Tagging or publishing is a decision that is made
explicitly and separately; it is never a step you take on your own initiative
because a change looked finished.

## 1. The HTTP surface is larger than the README table

The README lists four routes. The server binds `127.0.0.1:$PORT` (default
`9876`, `src/main.rs`) and serves eight:

| Route | Method | Group |
|---|---|---|
| `/health` | GET | legacy audio |
| `/devices` | GET | legacy audio |
| `/play` | POST | legacy audio |
| `/stop` | POST | legacy audio |
| `/restream/pair` | POST | restream control |
| `/restream/start` | POST | restream control |
| `/restream/stop` | POST | restream control |
| `/restream/status` | GET | restream, ungated |

If you add a route, decide which group it belongs to before you write it. That
choice is the whole of §2.

## 2. Invariant: the two groups have different origin policy, on purpose

**Changes to CORS or `Origin`/`Host` handling in `run_http_server` are the
product's security behaviour, not cleanup.** The asymmetry below looks like an
oversight to anyone tidying the router, and it is not one.

- The four legacy audio routes carry a permissive `CorsLayer` (any origin,
  method, header). The OCvoice web page calls them cross-origin from the
  browser, so they have to be reachable that way.
- The `CorsLayer` is applied to `legacy_routes` *before* `/restream` is nested
  onto it, so it does not extend to the restream routes. That ordering is load
  bearing: reordering those builder calls silently widens the restream surface
  to every origin.
- The three restream *control* routes additionally sit behind
  `restream_origin_guard`, which requires a local `Host` and rejects a
  non-local `Origin` with `403`. Restream control is deliberately
  unauthenticated for the native localhost client, and this guard is what
  stands in place of that authentication.
- `/restream/status` is registered outside the guard and is read-only.

Making the two groups uniform — in either direction — changes what the shipped
app allows. It is a decision with a rationale, never a refactor. The same goes
for relaxing `is_local_host` / `is_local_origin`, which decide what "local"
means for the whole guard.

`restream_request_origin_guard_allows` is factored out of the middleware purely
so this policy is testable, and `origin_guard_tests` in `src/main.rs` is the
executable statement of it. **If a change of yours requires editing those tests,
you are changing the policy, not the code** — say so out loud in the commit
message rather than adjusting the assertions to match new behaviour.

## 3. The version lives in three places and only one of them is the tag

| Where | Reaches |
|---|---|
| `version` in `Cargo.toml` | `env!("CARGO_PKG_VERSION")` → tray label, `/health`, startup banner, **and the auto-update comparison against the latest GitHub release** |
| the `v*` git tag | `CFBundleVersion` / `CFBundleShortVersionString`, written by `scripts/bundle-macos.sh`, on tag builds only |
| the literal in `resources/macos/Info.plist` | those same two fields when there is no tag (a `workflow_dispatch` build) |

Tagging without bumping `Cargo.toml` ships an app that names itself with the old
number and compares updates against the old number, so it cannot see the release
it is. Releases `v0.3.1` and `v0.3.2` both went out that way.

`scripts/check-version-tag.sh` now refuses any build where those three
disagree, and it runs as the `version-gate` job before the build matrix and as a
dependency of `release`. Run it yourself before tagging:

```bash
scripts/check-version-tag.sh v0.3.3   # exits non-zero unless everything agrees
```

**A version bump is three files in one commit:** `Cargo.toml`,
`resources/macos/Info.plist`, and `Cargo.lock` (which `cargo build` updates for
you — commit it).

## 4. What CI runs, and what it does not

`.github/workflows/release.yml` is the only workflow. Job graph:
`version-gate` → `build` (three matrix slices) → `release` (tag refs only).

- `cargo test` runs on the arm64 macOS slice and the Windows slice. It is
  skipped on the x64 macOS slice, which is cross-compiled on an arm64 runner and
  cannot execute its own test binaries.
- `cargo clippy --all-targets -- -D warnings` runs on the arm64 macOS slice
  only. The Windows-only `cfg` paths have never been linted; widening it belongs
  in a change that first shows it green there.
- **`cargo fmt --check` is deliberately not in CI, because the tree does not
  satisfy it.** Run it and you will see pre-existing drift in code unrelated to
  whatever you are doing. Do not run `cargo fmt` as a drive-by: it reformats
  untouched code and buries your actual change in the diff. Format the lines you
  touched, by hand, in the surrounding style. Cleaning the whole tree is its own
  commit, and should be its own commit.
- A manual `workflow_dispatch` run builds and uploads workflow artifacts. It
  does **not** create a GitHub Release — the `release` job is gated on the ref
  being a `v*` tag. Use dispatch freely to test a build; it publishes nothing.

## 5. The shipped artifact is not a single binary

The README still calls the Windows release a standalone `.exe`. It has not been
one since the restream engine was bundled:

- Windows: the zip contains `ocvoice-audio-router.exe`,
  `ocvoice-restream-engine.exe` and `ffmpeg.exe`.
- macOS: the `.app` carries the engine and `ffmpeg` in `Contents/Resources`,
  each nested-signed and notarized with the router.

Both sidecars are fetched at build time and gated fail-closed before anything is
signed: the engine against a pinned tag and a pinned SHA-256 (the `ENGINE_*`
block at the top of the workflow — the tag and all three digests are one unit
and move together), and `ffmpeg` against two checks — it must not be
`--enable-nonfree`, because a non-redistributable build cannot legally ship
inside a GPL-3.0 app, and it must support `rtmps`, because the push target needs
TLS.

The `FFMPEG_URL_*` defaults still point at a `--enable-nonfree` build and will
therefore be rejected by that gate. Repointing them at redistributable GPL
builds with pinned digests is a deliberate piece of work of its own — it is a
precondition for the next release, not a detail to fix in passing while doing
something else.

## 6. Working notes

- `RESTREAM_ENGINE_PATH`, `RESTREAM_FFMPEG_PATH`, `RESTREAM_CONVEX_URL`,
  `RESTREAM_RTMP_PORT` and `PORT` override the defaults; the supervisor test
  uses the first of these to run against a stub engine, which is why it needs no
  real engine binary present.
- The supervisor test is `#[cfg(all(test, unix))]` and compiles out on Windows.
  Keep it that way: it drives a shell stub and sends signals.
- Audio streams live on a dedicated OS thread because cpal requires it. Moving
  that work onto the async runtime is not a simplification.
