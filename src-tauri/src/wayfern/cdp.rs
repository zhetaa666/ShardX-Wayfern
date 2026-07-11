//! Wayfern CDP client — spawns headless chrome.exe and calls
//! `Wayfern.getFingerprint` at page level. No auth needed for that method
//! (Runtime.evaluate IS paid-gated, but we never touch it).
//!
//! Prototype: `wayfern-probe/probe3.mjs` and `wayfern-fresh/extract-verify.mjs`.
//! Verified: 3/3 unique canvasNoiseSeed + webglRenderer per launch (fresh UDD).

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::sleep;

/// Total time budget from spawn → CDP result → cleanup. Prototype takes ~2s;
/// give plenty of headroom for cold caches and slow disks.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const PORT_WAIT: Duration = Duration::from_secs(15);
const TARGET_WAIT: Duration = Duration::from_secs(8);
const WS_REPLY_WAIT: Duration = Duration::from_secs(10);

pub async fn grab_fingerprint(binary: &Path) -> Result<Value> {
    tokio::time::timeout(TOTAL_TIMEOUT, grab_inner(binary))
        .await
        .context("Wayfern fingerprint grab timed out")?
}

async fn grab_inner(binary: &Path) -> Result<Value> {
    let port = pick_free_port().await?;
    let udd = tempdir_prefix("wf-grab-")?;

    let mut cmd = tokio::process::Command::new(binary);
    cmd.args([
        &format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1",
        &format!("--user-data-dir={}", udd.display()),
        "--no-first-run",
        "--no-default-browser-check",
        "--headless=new",
        "--disable-gpu",
        "about:blank",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    // Wayfern loads dataset/font-spoof from CWD-relative subdirs — cwd MUST be
    // the binary dir or Worker-scope timezone spoof breaks.
    // (See memory: project_wayfern_cwd.md.)
    if let Some(parent) = binary.parent() {
        cmd.current_dir(parent);
    }

    #[cfg(target_os = "windows")]
    {
        // CREATE_NO_WINDOW so no console flashes for the user.
        // `creation_flags` is inherent on tokio::process::Command on Windows,
        // no CommandExt import needed.
        cmd.creation_flags(0x0800_0000);
    }

    let mut child = cmd.spawn().context("failed to spawn Wayfern chrome.exe")?;

    // Guard: whatever we return / bail with, make sure the child dies AND the
    // UDD is scrubbed. Spawning +30 chrome.exe's per session isn't cute.
    let result = run_cdp(port).await;

    let _ = child.start_kill();
    // Give the process a beat to release its lock on UDD files.
    sleep(Duration::from_millis(500)).await;
    let _ = tokio::fs::remove_dir_all(&udd).await;

    result
}

async fn run_cdp(port: u16) -> Result<Value> {
    wait_for_port(port).await?;
    let target = find_page_target(port).await?;
    call_get_fingerprint(&target).await
}

async fn wait_for_port(port: u16) -> Result<()> {
    let deadline = tokio::time::Instant::now() + PORT_WAIT;
    while tokio::time::Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(());
        }
        sleep(Duration::from_millis(300)).await;
    }
    anyhow::bail!("CDP port {port} never opened");
}

async fn find_page_target(port: u16) -> Result<String> {
    let url = format!("http://127.0.0.1:{port}/json/list");
    let deadline = tokio::time::Instant::now() + TARGET_WAIT;
    let client = reqwest::Client::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send().await {
            if let Ok(list) = resp.json::<Value>().await {
                if let Some(arr) = list.as_array() {
                    for t in arr {
                        if t.get("type").and_then(|s| s.as_str()) == Some("page") {
                            if let Some(ws) =
                                t.get("webSocketDebuggerUrl").and_then(|s| s.as_str())
                            {
                                return Ok(ws.to_string());
                            }
                        }
                    }
                }
            }
        }
        sleep(Duration::from_millis(300)).await;
    }
    anyhow::bail!("no CDP page target appeared");
}

async fn call_get_fingerprint(ws_url: &str) -> Result<Value> {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("CDP WS connect failed")?;

    use tokio_tungstenite::tungstenite::Message;
    let req = serde_json::json!({
        "id": 1,
        "method": "Wayfern.getFingerprint",
        "params": {}
    });
    ws.send(Message::Text(req.to_string())).await?;

    let deadline = tokio::time::Instant::now() + WS_REPLY_WAIT;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = tokio::time::timeout(remaining, ws.next()).await;
        let msg = match msg {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("WS error: {e}")),
            Ok(None) => anyhow::bail!("WS closed before reply"),
            Err(_) => anyhow::bail!("WS reply timed out"),
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string(),
            _ => continue,
        };
        let v: Value = serde_json::from_str(&text).context("CDP reply not JSON")?;
        // CDP events (no `id` field) are noise between the send and the reply.
        if v.get("id").and_then(|n| n.as_i64()) != Some(1) {
            continue;
        }
        let _ = ws.send(Message::Close(None)).await;
        if let Some(err) = v.get("error") {
            anyhow::bail!("Wayfern.getFingerprint failed: {err}");
        }
        // Result shape: { result: { fingerprint: { ...75 fields... } } }
        let fp = v
            .pointer("/result/fingerprint")
            .cloned()
            .context("no fingerprint in CDP result")?;
        return Ok(fp);
    }
    anyhow::bail!("no CDP reply within {}s", WS_REPLY_WAIT.as_secs());
}

async fn pick_free_port() -> Result<u16> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let p = l.local_addr()?.port();
    drop(l);
    Ok(p)
}

fn tempdir_prefix(prefix: &str) -> Result<std::path::PathBuf> {
    let base = std::env::temp_dir();
    let unique = uuid::Uuid::new_v4().simple().to_string();
    let dir = base.join(format!("{prefix}{unique}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
