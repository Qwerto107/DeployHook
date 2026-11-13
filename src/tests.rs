use super::*;
use axum::{body::Body, http::Request as HttpRequest};
use tower::ServiceExt;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const NOW: u64 = 1_800_000_000;

fn sign(timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(timestamp.as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

#[test]
fn signature_covers_timestamp_and_exact_body() {
    let timestamp = NOW.to_string();
    let body = br#"{"event":"deploy"}"#;
    let signature = sign(&timestamp, body);
    // 独立测试向量由 Python 标准库生成，用于核对签名协议。
    assert_eq!(
        signature,
        "sha256=a746085595aeb39ebce14a404e4df2c800ee4f7c2d349f6cf5d5eb589ef0c514"
    );
    assert!(authenticate(SECRET, &timestamp, body, &signature, NOW).is_ok());
    assert!(authenticate(SECRET, &timestamp, b"changed", &signature, NOW).is_err());
    assert!(authenticate(SECRET, &(NOW + 1).to_string(), body, &signature, NOW).is_err());
    assert!(authenticate(b"another secret", &timestamp, body, &signature, NOW).is_err());
}

#[test]
fn rejects_old_and_future_signed_requests() {
    for seconds in [NOW - 301, NOW + 301] {
        let timestamp = seconds.to_string();
        assert!(authenticate(SECRET, &timestamp, b"", &sign(&timestamp, b""), NOW).is_err());
    }
    for seconds in [NOW - 300, NOW + 300] {
        let timestamp = seconds.to_string();
        assert!(authenticate(SECRET, &timestamp, b"", &sign(&timestamp, b""), NOW).is_ok());
    }
}

#[test]
fn rejects_malformed_authentication() {
    let timestamp = NOW.to_string();
    for signature in [
        "",
        "sha256=00",
        "sha1=1234",
        &format!("sha256={}", "z".repeat(64)),
    ] {
        assert!(authenticate(SECRET, &timestamp, b"", signature, NOW).is_err());
    }
    for timestamp in ["", "+1800000000", "1800000000 ", "18446744073709551616"] {
        assert!(authenticate(SECRET, timestamp, b"", &sign(timestamp, b""), NOW).is_err());
    }
}

#[test]
fn rejects_duplicate_headers() {
    let mut headers = HeaderMap::new();
    headers.insert("x-deploy-timestamp", NOW.to_string().parse().unwrap());
    headers.insert("x-deploy-signature", "sha256=00".parse().unwrap());
    assert!(signature_headers(&headers).is_ok());
    headers.append("x-deploy-timestamp", NOW.to_string().parse().unwrap());
    assert!(signature_headers(&headers).is_err());
}

#[test]
fn duplicate_cooldown_and_expiration_are_independent() {
    let now = Instant::now();
    let mut gate = TriggerGate::default();
    assert!(gate.check([1; 32], now).is_ok());
    gate.record([1; 32], now);
    assert_eq!(
        gate.check([1; 32], now).unwrap_err().0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        gate.check([2; 32], now).unwrap_err().0,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert!(gate.check([2; 32], now + COOLDOWN).is_ok());
    assert_eq!(
        gate.check([1; 32], now + Duration::from_secs(599))
            .unwrap_err()
            .0,
        StatusCode::CONFLICT
    );
    assert!(gate.check([1; 32], now + Duration::from_secs(601)).is_ok());
}

fn app() -> Router {
    router(test_state())
}

fn test_state() -> Arc<AppState> {
    Arc::new(AppState {
        secret: SECRET.to_vec(),
        script: PathBuf::from("/must-not-execute-in-tests.sh"),
        script_log: None,
        gate: Arc::new(Mutex::new(TriggerGate::default())),
    })
}

#[tokio::test]
async fn health_and_invalid_requests_do_not_execute_commands() {
    let response = app()
        .oneshot(HttpRequest::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response = app()
        .oneshot(HttpRequest::post("/webhook").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app()
        .oneshot(HttpRequest::get("/webhook").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn rejects_oversized_body_without_content_length() {
    let response = app()
        .oneshot(
            HttpRequest::post("/webhook")
                .header("x-deploy-timestamp", NOW.to_string())
                .header("x-deploy-signature", sign(&NOW.to_string(), b""))
                .body(Body::from(vec![b'x'; MAX_BODY + 1]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn rejects_forged_signature_before_starting_script() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let response = app()
        .oneshot(
            HttpRequest::post("/webhook")
                .header("x-deploy-timestamp", now)
                .header("x-deploy-signature", format!("sha256={}", "00".repeat(32)))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rejects_authenticated_request_while_update_is_running() {
    let state = test_state();
    let _running = state.gate.clone().try_lock_owned().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let response = router(state)
        .oneshot(
            HttpRequest::post("/webhook")
                .header("x-deploy-timestamp", now.as_str())
                .header("x-deploy-signature", sign(&now, b"{}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[cfg(unix)]
#[tokio::test]
async fn script_log_combines_streams_and_overwrites_previous_run() {
    let path = env::temp_dir().join(format!("deployhook-log-test-{}.log", std::process::id()));
    fs::write(&path, "previous output that must be removed").unwrap();
    let (stdout, stderr) = script_output(Some(&path)).unwrap();
    let status = Command::new("/bin/sh")
        .args(["-c", "printf 'stdout\\n'; printf 'stderr\\n' >&2"])
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), "stdout\nstderr\n");
    let (stdout, stderr) = script_output(Some(&path)).unwrap();
    let status = Command::new("/bin/sh")
        .args(["-c", "printf 'next run\\n'"])
        .stdout(stdout)
        .stderr(stderr)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(&path).unwrap(), "next run\n");
    fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn rejected_requests_preserve_previous_script_log() {
    let path = env::temp_dir().join(format!(
        "deployhook-rejected-test-{}.log",
        std::process::id()
    ));
    fs::write(&path, "previous output").unwrap();
    let mut state = test_state();
    Arc::get_mut(&mut state).unwrap().script_log = Some(path.clone());
    let response = router(state.clone())
        .oneshot(HttpRequest::post("/webhook").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(fs::read_to_string(&path).unwrap(), "previous output");

    let _running = state.gate.clone().try_lock_owned().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .to_string();
    let response = router(state)
        .oneshot(
            HttpRequest::post("/webhook")
                .header("x-deploy-timestamp", now.as_str())
                .header("x-deploy-signature", sign(&now, b"{}"))
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(fs::read_to_string(&path).unwrap(), "previous output");
    fs::remove_file(path).unwrap();
}
