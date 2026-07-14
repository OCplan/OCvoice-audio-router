use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use crate::AppState;

const TAIL_LIMIT: usize = 50;
const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_GRACE: Duration = Duration::from_secs(2);
const RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_RESTARTS: usize = 5;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

type SharedChild = Arc<tokio::sync::Mutex<Child>>;

#[derive(Clone)]
pub(crate) struct RestreamSupervisor {
    inner: Arc<Mutex<RestreamInner>>,
    operation_lock: Arc<tokio::sync::Mutex<()>>,
}

struct RestreamInner {
    config: RestreamConfig,
    persisted: PersistedConfig,
    status: RestreamStatus,
    child: Option<SharedChild>,
    supervisor_task: Option<JoinHandle<()>>,
    tail: VecDeque<String>,
    stopping: bool,
    restarts: VecDeque<Instant>,
    next_backoff: Duration,
}

#[derive(Clone, Debug)]
enum RestreamStatus {
    Stopped,
    Starting,
    Running { since_unix: u64 },
    Errored { message: String },
}

struct RestreamConfig {
    convex_site_url: Option<String>,
    engine_path: PathBuf,
    ffmpeg_path: Option<PathBuf>,
    rtmp_port: u16,
    persistence_path: PathBuf,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct PersistedConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    convex_site_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    org_name: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct PairInfo {
    #[serde(rename = "orgName")]
    org_name: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestreamStatusView {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<u64>,
    device_linked: bool,
    convex_configured: bool,
    tail: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct PairRequest {
    code: String,
    name: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct OkResponse {
    ok: bool,
}

#[derive(Serialize)]
pub(crate) struct ErrorResponse {
    error: String,
}

impl RestreamConfig {
    fn load() -> (Self, PersistedConfig) {
        let base = user_config_dir();
        Self::load_at(base.join("OCvoiceAudioRouter").join("restream.json"))
    }

    fn load_at(persistence_path: PathBuf) -> (Self, PersistedConfig) {
        let persisted = load_persisted(&persistence_path).unwrap_or_else(|error| {
            eprintln!("[Restream] Could not load persisted config: {error}");
            PersistedConfig::default()
        });
        let convex_site_url = std::env::var("RESTREAM_CONVEX_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| persisted.convex_site_url.clone());
        let engine_path = std::env::var_os("RESTREAM_ENGINE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(default_engine_path);
        let ffmpeg_path = std::env::var_os("RESTREAM_FFMPEG_PATH")
            .map(PathBuf::from)
            .or_else(|| bundled_ffmpeg_path(&engine_path));
        let rtmp_port = std::env::var("RESTREAM_RTMP_PORT")
            .ok()
            .and_then(|port| port.parse().ok())
            .unwrap_or(1935);

        (
            Self {
                convex_site_url,
                engine_path,
                ffmpeg_path,
                rtmp_port,
                persistence_path,
            },
            persisted,
        )
    }
}

fn user_config_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library")
            .join("Application Support");
    }

    #[cfg(target_os = "windows")]
    if let Some(app_data) = std::env::var_os("APPDATA") {
        return PathBuf::from(app_data);
    }

    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

impl RestreamSupervisor {
    pub(crate) fn load() -> Self {
        let (config, persisted) = RestreamConfig::load();
        Self::new(config, persisted)
    }

    fn new(config: RestreamConfig, persisted: PersistedConfig) -> Self {
        let mut tail = VecDeque::new();
        let status = if config.engine_path.is_file() {
            RestreamStatus::Stopped
        } else {
            let message = missing_engine_message(&config.engine_path);
            tail.push_back(format!("[supervisor] {message}"));
            RestreamStatus::Errored { message }
        };

        Self {
            inner: Arc::new(Mutex::new(RestreamInner {
                config,
                persisted,
                status,
                child: None,
                supervisor_task: None,
                tail,
                stopping: false,
                restarts: VecDeque::new(),
                next_backoff: Duration::from_secs(1),
            })),
            operation_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) async fn pair(
        &self,
        code: String,
        name: Option<String>,
    ) -> Result<PairInfo, String> {
        if code.trim().is_empty() {
            return Err("pairing code must not be empty".to_string());
        }

        let (convex_url, engine_path) = {
            let inner = self.lock_inner();
            let convex_url = inner
                .config
                .convex_site_url
                .clone()
                .ok_or_else(|| "RESTREAM_CONVEX_URL is not configured".to_string())?;
            ensure_engine_exists(&inner.config.engine_path)?;
            (convex_url, inner.config.engine_path.clone())
        };

        let mut command = Command::new(&engine_path);
        command
            .kill_on_drop(true)
            .arg("pair")
            .arg("--convex")
            .arg(&convex_url)
            .arg("--code")
            .arg(code);
        if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
            command.arg("--name").arg(name);
        }

        let output = tokio::time::timeout(PAIR_TIMEOUT, command.output())
            .await
            .map_err(|_| "restream pairing timed out after 30 seconds".to_string())?
            .map_err(|error| format!("failed to run restream engine for pairing: {error}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = sanitize_tail_line(
                stderr
                    .trim()
                    .lines()
                    .last()
                    .unwrap_or("engine exited unsuccessfully"),
                None,
            );
            return Err(format!("restream pairing failed: {detail}"));
        }

        let stdout = String::from_utf8(output.stdout)
            .map_err(|_| "restream pairing returned non-UTF-8 output".to_string())?;
        let lines: Vec<&str> = stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        let marker_index = lines
            .iter()
            .position(|line| *line == "Device token (store it — shown once):")
            .ok_or_else(|| {
                "restream pairing response did not include the device-token marker".to_string()
            })?;
        let token = lines
            .last()
            .filter(|token| marker_index < lines.len() - 1 && !token.is_empty())
            .ok_or_else(|| "restream pairing response did not include a device token".to_string())?
            .to_string();
        let org_name = lines
            .iter()
            .find_map(|line| {
                line.strip_prefix("Paired with ")
                    .and_then(|value| value.strip_suffix('.'))
            })
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                "restream pairing response did not include an organization name".to_string()
            })?
            .to_string();

        let persistence_path = {
            let mut inner = self.lock_inner();
            inner.persisted.convex_site_url = Some(convex_url);
            inner.persisted.device_token = Some(token.clone());
            inner.persisted.org_name = Some(org_name.clone());
            let path = inner.config.persistence_path.clone();
            save_persisted(&path, &inner.persisted)?;
            path
        };
        let prefix = if token.chars().count() > 6 {
            token.chars().take(6).collect()
        } else {
            "<redacted>".to_string()
        };
        println!(
            "[Restream] Paired with {org_name}; stored device token {prefix}… at {}",
            persistence_path.display()
        );

        Ok(PairInfo { org_name })
    }

    pub(crate) async fn start(&self) -> Result<(), String> {
        let _operation_guard = self.operation_lock.lock().await;
        {
            let mut inner = self.lock_inner();
            if matches!(
                inner.status,
                RestreamStatus::Running { .. } | RestreamStatus::Starting
            ) || inner.supervisor_task.is_some()
            {
                return Ok(());
            }
            validate_start_config(&inner)?;
            inner.stopping = false;
            inner.restarts.clear();
            inner.next_backoff = Duration::from_secs(1);
            inner.status = RestreamStatus::Starting;
        }

        if let Err(error) = self.spawn_run_process().await {
            self.set_error(error.clone());
            return Err(error);
        }

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let supervisor = self.clone();
        let task = tokio::spawn(async move {
            if ready_rx.await.is_err() {
                return;
            }
            supervisor.supervise().await;
        });
        self.lock_inner().supervisor_task = Some(task);
        let _ = ready_tx.send(());
        Ok(())
    }

    async fn spawn_run_process(&self) -> Result<(), String> {
        let (engine_path, convex_url, token, ffmpeg_path, rtmp_port) = {
            let inner = self.lock_inner();
            validate_start_config(&inner)?;
            (
                inner.config.engine_path.clone(),
                inner.config.convex_site_url.clone().unwrap_or_default(),
                inner.persisted.device_token.clone().unwrap_or_default(),
                inner.config.ffmpeg_path.clone(),
                inner.config.rtmp_port,
            )
        };

        let mut command = Command::new(&engine_path);
        command
            .kill_on_drop(true)
            .arg("run")
            .arg("--convex")
            .arg(convex_url)
            .arg("--token")
            .arg(&token)
            .arg("--port")
            .arg(rtmp_port.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(path) = ffmpeg_path {
            command.arg("--ffmpeg").arg(path);
        }

        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start restream engine: {error}"))?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(tokio::sync::Mutex::new(child));
        {
            let mut inner = self.lock_inner();
            inner.child = Some(child);
            inner.status = RestreamStatus::Running {
                since_unix: unix_now(),
            };
        }

        if let Some(stdout) = stdout {
            self.spawn_tail_reader(stdout, "stdout");
        }
        if let Some(stderr) = stderr {
            self.spawn_tail_reader(stderr, "stderr");
        }
        Ok(())
    }

    fn spawn_tail_reader<R>(&self, stream: R, label: &'static str)
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                supervisor.push_tail(format!("[{label}] {line}"));
            }
        });
    }

    async fn supervise(&self) {
        loop {
            let child = {
                let inner = self.lock_inner();
                if inner.stopping {
                    break;
                }
                inner.child.clone()
            };

            let failure = if let Some(child) = child {
                loop {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    if self.lock_inner().stopping {
                        self.clear_supervisor_task();
                        return;
                    }
                    match child.lock().await.try_wait() {
                        Ok(None) => continue,
                        Ok(Some(status)) => {
                            break format!("restream engine exited unexpectedly with {status}");
                        }
                        Err(error) => break format!("failed to monitor restream engine: {error}"),
                    }
                }
            } else {
                "restream engine was not running".to_string()
            };

            {
                let mut inner = self.lock_inner();
                inner.child = None;
                inner.status = RestreamStatus::Errored {
                    message: failure.clone(),
                };
            }
            self.push_tail(format!("[supervisor] {failure}"));

            let Some(delay) = self.next_restart_delay() else {
                break;
            };
            self.push_tail(format!(
                "[supervisor] Restarting in {} second(s)",
                delay.as_secs()
            ));
            tokio::time::sleep(delay).await;
            if self.lock_inner().stopping {
                break;
            }
            self.lock_inner().status = RestreamStatus::Starting;

            if let Err(error) = self.spawn_run_process().await {
                self.set_error(error.clone());
                self.push_tail(format!("[supervisor] {error}"));
            }
        }
        self.clear_supervisor_task();
    }

    fn next_restart_delay(&self) -> Option<Duration> {
        let mut inner = self.lock_inner();
        let now = Instant::now();
        while inner
            .restarts
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) > RESTART_WINDOW)
        {
            inner.restarts.pop_front();
        }
        inner.restarts.push_back(now);
        if inner.restarts.len() > MAX_RESTARTS {
            let message = format!("restream engine exceeded {MAX_RESTARTS} restarts in 10 minutes");
            inner.status = RestreamStatus::Errored {
                message: message.clone(),
            };
            push_tail_inner(&mut inner, format!("[supervisor] {message}"));
            return None;
        }

        let delay = inner.next_backoff;
        inner.next_backoff = (inner.next_backoff * 2).min(MAX_BACKOFF);
        Some(delay)
    }

    pub(crate) async fn stop(&self) -> Result<(), String> {
        let _operation_guard = self.operation_lock.lock().await;
        let (child, task) = {
            let mut inner = self.lock_inner();
            inner.stopping = true;
            (inner.child.clone(), inner.supervisor_task.take())
        };

        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }

        let stop_result = if let Some(child) = child {
            stop_child(&child).await
        } else {
            Ok(())
        };

        let mut inner = self.lock_inner();
        inner.child = None;
        inner.stopping = false;
        inner.restarts.clear();
        inner.next_backoff = Duration::from_secs(1);
        if let Err(error) = &stop_result {
            inner.status = RestreamStatus::Errored {
                message: error.clone(),
            };
            push_tail_inner(&mut inner, format!("[supervisor] {error}"));
        } else {
            inner.status = RestreamStatus::Stopped;
        }
        stop_result
    }

    pub(crate) async fn status(&self) -> RestreamStatusView {
        let inner = self.lock_inner();
        let engine_missing = !inner.config.engine_path.is_file()
            && !matches!(inner.status, RestreamStatus::Running { .. });
        let (status, since) = if engine_missing {
            ("errored", None)
        } else {
            match &inner.status {
                RestreamStatus::Stopped => ("stopped", None),
                RestreamStatus::Starting => ("starting", None),
                RestreamStatus::Running { since_unix } => ("running", Some(*since_unix)),
                RestreamStatus::Errored { message } => {
                    debug_assert!(!message.is_empty());
                    ("errored", None)
                }
            }
        };
        let mut tail: Vec<String> = inner.tail.iter().cloned().collect();
        if engine_missing {
            let message = format!(
                "[supervisor] {}",
                missing_engine_message(&inner.config.engine_path)
            );
            if tail.last() != Some(&message) {
                tail.push(message);
                if tail.len() > TAIL_LIMIT {
                    tail.remove(0);
                }
            }
        }

        RestreamStatusView {
            status,
            since,
            device_linked: inner.persisted.device_token.is_some(),
            convex_configured: inner.config.convex_site_url.is_some(),
            tail,
        }
    }

    fn set_error(&self, message: String) {
        self.lock_inner().status = RestreamStatus::Errored { message };
    }

    fn clear_supervisor_task(&self) {
        self.lock_inner().supervisor_task = None;
    }

    fn push_tail(&self, line: String) {
        let mut inner = self.lock_inner();
        let sanitized = sanitize_tail_line(&line, inner.persisted.device_token.as_deref());
        push_tail_inner(&mut inner, sanitized);
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, RestreamInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn validate_start_config(inner: &RestreamInner) -> Result<(), String> {
    ensure_engine_exists(&inner.config.engine_path)?;
    if inner.config.convex_site_url.is_none() {
        return Err("RESTREAM_CONVEX_URL is not configured".to_string());
    }
    if inner.persisted.device_token.is_none() {
        return Err("restream device is not paired".to_string());
    }
    Ok(())
}

fn ensure_engine_exists(path: &Path) -> Result<(), String> {
    if path.is_file() {
        Ok(())
    } else {
        Err(missing_engine_message(path))
    }
}

fn missing_engine_message(path: &Path) -> String {
    format!("restream engine binary not found at {}", path.display())
}

// cfg-gated `return`s per target — needless_return would break the non-matching
// branches, so keep them and silence the lint for this fn only.
#[allow(clippy::needless_return)]
fn default_engine_path() -> PathBuf {
    let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));

    #[cfg(target_os = "macos")]
    {
        return executable
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."))
            .join("Resources")
            .join("ocvoice-restream-engine");
    }

    #[cfg(target_os = "windows")]
    {
        return executable
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("ocvoice-restream-engine.exe");
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("ocvoice-restream-engine")
}

fn bundled_ffmpeg_path(engine_path: &Path) -> Option<PathBuf> {
    let filename = if cfg!(target_os = "windows") {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    let path = engine_path.parent()?.join(filename);
    path.is_file().then_some(path)
}

fn load_persisted(path: &Path) -> Result<PersistedConfig, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid JSON in {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedConfig::default())
        }
        Err(error) => Err(format!("could not read {}: {error}", path.display())),
    }
}

fn save_persisted(path: &Path, persisted: &PersistedConfig) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(persisted)
        .map_err(|error| format!("could not serialize restream config: {error}"))?;

    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn sanitize_tail_line(line: &str, token: Option<&str>) -> String {
    let has_long_credential_like_word = line.split_whitespace().any(|word| {
        word.len() >= 32
            && word
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "-_.".contains(character))
    });
    if line.to_ascii_lowercase().contains("token")
        || token.is_some_and(|token| line.contains(token))
        || has_long_credential_like_word
    {
        "[engine] [redacted token line]".to_string()
    } else {
        line.to_string()
    }
}

fn push_tail_inner(inner: &mut RestreamInner, line: String) {
    inner.tail.push_back(line);
    while inner.tail.len() > TAIL_LIMIT {
        inner.tail.pop_front();
    }
}

async fn stop_child(child: &SharedChild) -> Result<(), String> {
    let mut child = child.lock().await;

    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let sent_sigterm = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .is_ok_and(|status| status.success());
        if !sent_sigterm {
            child
                .start_kill()
                .map_err(|error| format!("failed to kill restream engine: {error}"))?;
        }
    }

    #[cfg(not(unix))]
    child
        .start_kill()
        .map_err(|error| format!("failed to kill restream engine: {error}"))?;

    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("failed to wait for restream engine: {error}")),
        Err(_) => {
            child
                .start_kill()
                .map_err(|error| format!("failed to force-kill restream engine: {error}"))?;
            child
                .wait()
                .await
                .map(|_| ())
                .map_err(|error| format!("failed to reap restream engine: {error}"))
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn error_status(error: &str) -> StatusCode {
    if error.contains("not configured")
        || error.contains("not paired")
        || error.contains("must not be empty")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    }
}

pub(crate) async fn pair(
    State(state): State<AppState>,
    Json(request): Json<PairRequest>,
) -> Result<Json<PairInfo>, (StatusCode, Json<ErrorResponse>)> {
    state
        .restream
        .pair(request.code, request.name)
        .await
        .map(Json)
        .map_err(|error| (error_status(&error), Json(ErrorResponse { error })))
}

pub(crate) async fn start(
    State(state): State<AppState>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .restream
        .start()
        .await
        .map(|()| Json(OkResponse { ok: true }))
        .map_err(|error| (error_status(&error), Json(ErrorResponse { error })))
}

pub(crate) async fn stop(
    State(state): State<AppState>,
) -> Result<Json<OkResponse>, (StatusCode, Json<ErrorResponse>)> {
    state
        .restream
        .stop()
        .await
        .map(|()| Json(OkResponse { ok: true }))
        .map_err(|error| (error_status(&error), Json(ErrorResponse { error })))
}

pub(crate) async fn status(State(state): State<AppState>) -> Json<RestreamStatusView> {
    Json(state.restream.status().await)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn restream_supervisor_lifecycle_and_auto_restart() {
        let _env_guard = ENV_LOCK.lock().await;
        let test_dir = std::env::temp_dir().join(format!(
            "ocvoice-restream-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&test_dir).unwrap();
        let engine_path = test_dir.join("stub-engine.sh");
        let pid_path = test_dir.join("engine.pid");
        fs::write(
            &engine_path,
            "#!/bin/sh\nif [ \"$1\" = \"run\" ]; then\n  echo $$ > \"$STUB_PID_FILE\"\n  echo 'stub engine running'\n  trap 'exit 0' TERM INT\n  while :; do sleep 1; done\nfi\nexit 2\n",
        )
        .unwrap();
        fs::set_permissions(&engine_path, fs::Permissions::from_mode(0o700)).unwrap();
        // SAFETY: this test serializes all environment access through ENV_LOCK.
        unsafe {
            std::env::set_var("RESTREAM_ENGINE_PATH", &engine_path);
            std::env::set_var("STUB_PID_FILE", &pid_path);
        }

        let persistence_path = test_dir.join("config").join("restream.json");
        let persisted = PersistedConfig {
            convex_site_url: Some("https://example.invalid".to_string()),
            device_token: Some("test-device-token-secret".to_string()),
            org_name: Some("Test Org".to_string()),
        };
        save_persisted(&persistence_path, &persisted).unwrap();
        let (config, persisted) = RestreamConfig::load_at(persistence_path);
        let supervisor = RestreamSupervisor::new(config, persisted);

        // Start → wait for the stub to actually write its pid (proves it ran;
        // status flips to Running on spawn, before the child executes, so gate
        // on the pid file rather than status to avoid a race).
        supervisor.start().await.unwrap();
        let first_pid = wait_for_pid(&pid_path, None, Duration::from_secs(3)).await;
        wait_for_status(&supervisor, "running", Duration::from_secs(2)).await;

        // Deliberate stop → Stopped, and it must NOT auto-restart.
        supervisor.stop().await.unwrap();
        wait_for_status(&supervisor, "stopped", Duration::from_secs(2)).await;

        // Start again, then KILL the child out from under the supervisor →
        // it must auto-restart with a fresh pid.
        supervisor.start().await.unwrap();
        let restarted_pid = wait_for_pid(&pid_path, Some(first_pid), Duration::from_secs(3)).await;
        let killed = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(restarted_pid.to_string())
            .status()
            .unwrap();
        assert!(killed.success());

        let after_crash_pid =
            wait_for_pid(&pid_path, Some(restarted_pid), Duration::from_secs(6)).await;
        assert_ne!(restarted_pid, after_crash_pid);
        wait_for_status(&supervisor, "running", Duration::from_secs(2)).await;
        supervisor.stop().await.unwrap();

        // SAFETY: this test serializes all environment access through ENV_LOCK.
        unsafe {
            std::env::remove_var("RESTREAM_ENGINE_PATH");
            std::env::remove_var("STUB_PID_FILE");
        }
        fs::remove_dir_all(test_dir).unwrap();
    }

    async fn wait_for_status(supervisor: &RestreamSupervisor, expected: &str, timeout: Duration) {
        let result = tokio::time::timeout(timeout, async {
            loop {
                if supervisor.status().await.status == expected {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await;
        assert!(result.is_ok(), "timed out waiting for {expected}");
    }

    async fn wait_for_pid(path: &Path, previous: Option<u32>, timeout: Duration) -> u32 {
        tokio::time::timeout(timeout, async {
            loop {
                if let Ok(contents) = fs::read_to_string(path) {
                    if let Ok(pid) = contents.trim().parse::<u32>() {
                        if Some(pid) != previous {
                            return pid;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("timed out waiting for stub engine pid")
    }
}
