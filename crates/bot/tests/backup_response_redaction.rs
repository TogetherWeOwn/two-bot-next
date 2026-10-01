//! Exercise the real successful uploader against a credential-echoing fake S3.
use axum::{
    http::{header, HeaderMap},
    routing::put,
    Router,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn successful_upload_never_prints_etag_echoed_authorization() {
    let root = std::env::var_os("PAPERCLIP_RUN_SCRATCH_DIR")
        .or_else(|| std::env::var_os("PAPERCLIP_SCRATCH_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let scratch = Scratch(root.join(format!("s3-response-redaction-{}", std::process::id())));
    std::fs::create_dir(&scratch.0).unwrap();
    let dump = scratch.0.join("fixture.dump");
    std::fs::write(&dump, b"synthetic backup bytes").unwrap();

    let echoed = Arc::new(Mutex::new(String::new()));
    let received = echoed.clone();
    let app = Router::new().route(
        "/{*key}",
        put(move |headers: HeaderMap| {
            let received = received.clone();
            async move {
                let auth = headers[header::AUTHORIZATION].clone();
                *received.lock().unwrap() = auth.to_str().unwrap().to_owned();
                ([(header::ETAG, auth)], "fixture-remote-body-secret")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_two-bot"))
        .env_clear()
        .env("TWO_BACKUP_S3_ENDPOINT", endpoint)
        .env("TWO_BACKUP_S3_BUCKET", "fixture-backups")
        .env("TWO_BACKUP_S3_ACCESS_KEY_ID", "fixture-access-id")
        .env("TWO_BACKUP_S3_SECRET_ACCESS_KEY", "fixture-signing-key")
        .env("RUST_LOG", "debug")
        .arg("backup-upload")
        .arg(&dump)
        .output()
        .await
        .unwrap();
    server.abort();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "synthetic uploader did not succeed"
    );
    assert!(text.contains("stored fixture-backups/fixture.dump"));
    let auth = echoed.lock().unwrap().clone();
    assert!(auth.contains("Credential=fixture-access-id/"));
    let signature = auth.split("Signature=").nth(1).unwrap();
    for secret in [
        &auth,
        signature,
        "fixture-access-id",
        "fixture-signing-key",
        "fixture-remote-body-secret",
    ] {
        assert!(
            !text.contains(secret),
            "remote response leaked a synthetic credential"
        );
    }
}
