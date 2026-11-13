use std::{
    collections::HashMap,
    env, fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::{net::TcpListener, process::Command, sync::Mutex, time::timeout};
use tracing::{Instrument, error, info, warn};
use tracing_subscriber::EnvFilter;

const MAX_BODY: usize = 16 * 1024;
const SIGNATURE_WINDOW: u64 = 300;
const COOLDOWN: Duration = Duration::from_secs(30);
static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

struct AppState {
    secret: Vec<u8>,
    script: PathBuf,
    script_log: Option<PathBuf>,
    gate: Arc<Mutex<TriggerGate>>,
}

#[derive(Default)]
struct TriggerGate {
    last_attempt: Option<Instant>,
    seen: HashMap<[u8; 32], Instant>,
}

type ApiError = (StatusCode, &'static str);

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let listen: SocketAddr = env::var("DEPLOYHOOK_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8787".into())
        .parse()?;
    if !listen.ip().is_loopback() {
        return Err(io::Error::other("Webhook must listen on a loopback address").into());
    }
    let secret = env::var("DEPLOYHOOK_SECRET")?.into_bytes();
    // 配置与 CI 使用同一个字符串，不进行隐式裁剪或解码。
    if secret.len() < 32 || secret.len() > 256 || !secret.iter().all(u8::is_ascii_graphic) {
        return Err(io::Error::other(
            "Secret must contain 32-256 printable ASCII bytes without whitespace",
        )
        .into());
    }

    let script = PathBuf::from(env::var("DEPLOYHOOK_SCRIPT")?);
    if !script.is_absolute() || !fs::metadata(&script)?.is_file() {
        return Err(io::Error::other("Update script must be an absolute path to a file").into());
    }
    let script_log = env::var("DEPLOYHOOK_SCRIPT_LOG")
        .ok()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    if script_log
        .as_ref()
        .is_some_and(|path| !path.is_absolute() || path == &script)
    {
        return Err(io::Error::other(
            "Script log must be an absolute path different from the script",
        )
        .into());
    }
    let state = Arc::new(AppState {
        secret,
        script,
        script_log,
        gate: Arc::new(Mutex::new(TriggerGate::default())),
    });
    let listener = TcpListener::bind(listen).await?;
    info!(%listen, script = %state.script.display(), "Webhook service started");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/webhook", post(webhook))
        .route("/healthz", get(|| async { Json("ok") }))
        .layer(middleware::from_fn(log_request))
        .with_state(state)
}

async fn log_request(request: Request, next: Next) -> Response {
    let request_id = REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let span = tracing::info_span!("http_request", request_id, method = %request.method(), path = request.uri().path());
    async move {
        let start = Instant::now();
        let response = next.run(request).await;
        info!(
            status = response.status().as_u16(),
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Request completed"
        );
        response
    }
    .instrument(span)
    .await
}

async fn webhook(
    State(state): State<Arc<AppState>>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    let (timestamp, supplied_signature) = signature_headers(&parts.headers)?;
    // 限制实际读取的字节数，不能只信任 Content-Length；慢速请求最多占用五秒。
    let body = timeout(Duration::from_secs(5), to_bytes(body, MAX_BODY))
        .await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "request body timeout"))?
        .map_err(|_| {
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                "invalid or oversized request body",
            )
        })?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "clock unavailable"))?
        .as_secs();
    let fingerprint = authenticate(&state.secret, timestamp, &body, supplied_signature, now)
        .map_err(|_| {
            warn!("Webhook authentication failed");
            (StatusCode::UNAUTHORIZED, "invalid signature or timestamp")
        })?;

    // 后台任务持锁直到脚本退出，HTTP 返回或断开连接都不会提前释放互斥。
    let mut gate = state
        .gate
        .clone()
        .try_lock_owned()
        .map_err(|_| (StatusCode::CONFLICT, "update is already running"))?;
    let instant = Instant::now();
    gate.check(fingerprint, instant)?;
    // 仅在认证、去重和互斥检查后打开日志，避免被拒绝的请求清空上次输出。
    let (stdout, stderr) = script_output(state.script_log.as_deref()).map_err(|error| {
        error!(%error, "Could not open script log");
        (StatusCode::SERVICE_UNAVAILABLE, "could not open script log")
    })?;
    // GNU timeout 管理脚本的整个进程组，输出直接流向文件或服务日志。
    let mut child = Command::new("/usr/bin/timeout")
        .args(["--signal=TERM", "--kill-after=10s", "15m"])
        .arg(&state.script)
        .env_remove("DEPLOYHOOK_SECRET")
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            error!(%error, "Could not start update script");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "could not start update script",
            )
        })?;
    gate.record(fingerprint, instant);
    info!(pid = ?child.id(), "Update started");
    tokio::spawn(
        async move {
            let started = Instant::now();
            match child.wait().await {
                Ok(status) if status.success() => {
                    info!(
                        elapsed_secs = started.elapsed().as_secs(),
                        "Update succeeded"
                    );
                }
                Ok(status) => {
                    error!(
                        exit_code = ?status.code(),
                        elapsed_secs = started.elapsed().as_secs(),
                        "Update failed (124/137 may indicate timeout)"
                    );
                }
                Err(error) => error!(%error, "Could not wait for update script"),
            }
            drop(gate);
        }
        .instrument(tracing::Span::current()),
    );
    Ok((
        StatusCode::ACCEPTED,
        Json("update started; check service logs for result"),
    )
        .into_response())
}

fn script_output(path: Option<&Path>) -> io::Result<(Stdio, Stdio)> {
    match path {
        Some(path) => {
            // 每次开始执行时覆盖旧内容；克隆句柄共享偏移，合并 stdout 与 stderr。
            let output = fs::File::create(path)?;
            let errors = output.try_clone()?;
            Ok((Stdio::from(output), Stdio::from(errors)))
        }
        None => Ok((Stdio::inherit(), Stdio::inherit())),
    }
}

fn signature_headers(headers: &HeaderMap) -> Result<(&str, &str), ApiError> {
    let read = |name: &'static str| {
        if headers.get_all(name).iter().count() != 1 {
            return Err((
                StatusCode::UNAUTHORIZED,
                "missing or duplicate authentication header",
            ));
        }
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .ok_or((StatusCode::UNAUTHORIZED, "invalid authentication header"))
    };
    Ok((read("x-deploy-timestamp")?, read("x-deploy-signature")?))
}

fn authenticate(
    secret: &[u8],
    timestamp: &str,
    body: &[u8],
    signature: &str,
    now: u64,
) -> Result<[u8; 32], ()> {
    if timestamp.is_empty()
        || timestamp.len() > 20
        || !timestamp.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(());
    }
    let seconds: u64 = timestamp.parse().map_err(|_| ())?;
    if now.abs_diff(seconds) > SIGNATURE_WINDOW {
        return Err(());
    }
    let signature = signature.strip_prefix("sha256=").ok_or(())?;
    if signature.len() != 64 {
        return Err(());
    }
    let mut decoded = [0; 32];
    hex::decode_to_slice(signature, &mut decoded).map_err(|_| ())?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|_| ())?;
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    // 使用密码学库的恒定时间校验，原始请求体不得重新序列化。
    mac.verify_slice(&decoded).map_err(|_| ())?;
    Ok(decoded)
}

impl TriggerGate {
    fn check(&mut self, fingerprint: [u8; 32], now: Instant) -> Result<(), ApiError> {
        // 时间戳允许正负偏差，去重保留两个窗口以覆盖未来时间戳。
        self.seen.retain(|_, seen| {
            now.duration_since(*seen) <= Duration::from_secs(SIGNATURE_WINDOW * 2)
        });
        if self.seen.contains_key(&fingerprint) {
            return Err((StatusCode::CONFLICT, "duplicate webhook"));
        }
        if self
            .last_attempt
            .is_some_and(|last| now.duration_since(last) < COOLDOWN)
        {
            return Err((StatusCode::TOO_MANY_REQUESTS, "trigger rate limit exceeded"));
        }
        Ok(())
    }

    fn record(&mut self, fingerprint: [u8; 32], now: Instant) {
        self.last_attempt = Some(now);
        self.seen.insert(fingerprint, now);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                error!(%error, "Could not register SIGTERM handler");
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
    info!("Webhook service stopping");
}

#[cfg(test)]
mod tests;
