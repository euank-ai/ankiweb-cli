//! AnkiWeb sync protocol client.
//!
//! Handles authentication, collection download, and collection upload.

use std::io::{Cursor, Read, Write};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

const SYNC_VERSION: u8 = 11;
const DEFAULT_ENDPOINT: &str = "https://sync.ankiweb.net/";
const CLIENT_VERSION: &str = "anki,25.02 (dev),linux";

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub username: String,
    pub password: String,
    pub endpoint: Option<String>,
}

#[derive(Serialize)]
struct SyncHeader {
    #[serde(rename = "v")]
    sync_version: u8,
    #[serde(rename = "k")]
    sync_key: String,
    #[serde(rename = "c")]
    client_ver: String,
    #[serde(rename = "s")]
    session_key: String,
}

#[derive(Serialize)]
struct HostKeyRequest {
    #[serde(rename = "u")]
    username: String,
    #[serde(rename = "p")]
    password: String,
}

#[derive(Deserialize, Debug)]
struct HostKeyResponse {
    key: String,
    #[serde(default)]
    #[allow(dead_code)]
    endpoint: Option<String>,
}

#[derive(Serialize)]
struct MetaRequest {
    #[serde(rename = "v")]
    sync_version: u8,
    #[serde(rename = "cv")]
    client_version: String,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
struct MetaResponse {
    #[serde(rename = "cont", default)]
    should_continue: bool,
    #[serde(rename = "msg", default)]
    server_message: String,
    #[serde(default)]
    empty: bool,
}

fn session_id() -> String {
    use rand::Rng;
    let table = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::thread_rng();
    (0..8)
        .map(|_| table[rng.gen_range(0..table.len())] as char)
        .collect()
}

fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = zstd::Encoder::new(Vec::new(), 3)?;
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = zstd::Decoder::new(Cursor::new(data))?;
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

struct SyncRequestResult {
    data: Vec<u8>,
    new_endpoint: Option<String>,
}

async fn sync_request(
    client: &reqwest::Client,
    endpoint: &str,
    method: &str,
    hkey: &str,
    session_key: &str,
    body: &[u8],
) -> Result<SyncRequestResult> {
    let url = format!("{}/sync/{}", endpoint.trim_end_matches('/'), method);
    tracing::debug!(%url, %method, "sync_request");

    let header = SyncHeader {
        sync_version: SYNC_VERSION,
        sync_key: hkey.to_string(),
        client_ver: CLIENT_VERSION.to_string(),
        session_key: session_key.to_string(),
    };

    let compressed_body = zstd_compress(body)?;
    let header_json = serde_json::to_string(&header)?;

    let resp = client
        .post(&url)
        .header("anki-sync", &header_json)
        .header("content-type", "application/octet-stream")
        .body(compressed_body.clone())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    tracing::debug!(status = %resp.status(), "response");
    let (resp, new_endpoint) = if resp.status().is_redirection() {
        if let Some(location) = resp.headers().get("location").and_then(|v| v.to_str().ok()) {
            let new_base = location.trim_end_matches('/').to_string();
            let redirect_url = format!("{}/sync/{}", new_base, method);
            tracing::debug!(%redirect_url, "following redirect");
            let resp = client
                .post(&redirect_url)
                .header("anki-sync", &serde_json::to_string(&header)?)
                .header("content-type", "application/octet-stream")
                .body(compressed_body)
                .send()
                .await
                .with_context(|| format!("POST {redirect_url} (redirect)"))?;
            (resp, Some(new_base))
        } else {
            (resp, None)
        }
    } else {
        (resp, None)
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let headers = format!("{:?}", resp.headers());
        let body_bytes = resp.bytes().await.unwrap_or_default();
        // Try to decompress if zstd
        let body_text = if body_bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
            zstd_decompress(&body_bytes)
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_else(|_| format!("{:?}", body_bytes))
        } else {
            String::from_utf8_lossy(&body_bytes).to_string()
        };
        tracing::error!(%status, %headers, %body_text, "sync request failed");
        return Err(anyhow!("sync {method} failed ({status}): {body_text}"));
    }

    let resp_bytes = resp.bytes().await?;
    let data = if resp_bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd_decompress(&resp_bytes)?
    } else {
        resp_bytes.to_vec()
    };

    Ok(SyncRequestResult { data, new_endpoint })
}

/// Established sync session with auth + endpoint resolved.
pub struct SyncSession {
    client: reqwest::Client,
    hkey: String,
    session_key: String,
    endpoint: String,
    /// Raw meta response data (JSON bytes).
    pub meta_data: Vec<u8>,
}

impl SyncSession {
    /// Make a sync request within this session.
    pub async fn request(&self, method: &str, body: &[u8]) -> Result<Vec<u8>> {
        let result = sync_request(
            &self.client,
            &self.endpoint,
            method,
            &self.hkey,
            &self.session_key,
            body,
        )
        .await?;
        Ok(result.data)
    }
}

pub async fn establish_session(config: &SyncConfig) -> Result<SyncSession> {
    let endpoint = config
        .endpoint
        .as_deref()
        .unwrap_or(DEFAULT_ENDPOINT)
        .to_string();

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // Login
    let session_key = session_id();
    let req = HostKeyRequest {
        username: config.username.clone(),
        password: config.password.clone(),
    };
    let body = serde_json::to_vec(&req)?;
    tracing::debug!("logging in...");
    let result = sync_request(&client, &endpoint, "hostKey", "", &session_key, &body).await?;
    let resp: HostKeyResponse = serde_json::from_slice(&result.data)?;
    let hkey = resp.key;
    tracing::debug!(%hkey, "login successful");

    // Meta
    let meta_req = MetaRequest {
        sync_version: SYNC_VERSION,
        client_version: CLIENT_VERSION.to_string(),
    };
    let meta_body = serde_json::to_vec(&meta_req)?;
    let meta_result =
        sync_request(&client, &endpoint, "meta", &hkey, &session_key, &meta_body).await?;
    let resolved_endpoint = meta_result
        .new_endpoint
        .unwrap_or_else(|| endpoint.clone());

    // Parse meta response to check server message and continue flag
    #[derive(Deserialize)]
    struct Meta {
        #[serde(rename = "cont", default)]
        should_continue: bool,
        #[serde(rename = "msg", default)]
        server_message: String,
        #[serde(default)]
        empty: bool,
    }
    let meta: Meta = serde_json::from_slice(&meta_result.data)
        .context("parsing meta response")?;
    
    if !meta.server_message.is_empty() {
        tracing::warn!(msg = %meta.server_message, "AnkiWeb server message");
    }
    if !meta.should_continue {
        let msg = if meta.server_message.is_empty() {
            "server refused to continue sync".to_string()
        } else {
            meta.server_message.clone()
        };
        return Err(anyhow!("{msg}"));
    }

    Ok(SyncSession {
        client,
        hkey,
        session_key,
        endpoint: resolved_endpoint,
        meta_data: meta_result.data,
    })
}

/// Download the full collection from AnkiWeb.
pub async fn download_collection(config: &SyncConfig) -> Result<Vec<u8>> {
    let session = establish_session(config).await?;

    let result = sync_request(
        &session.client,
        &session.endpoint,
        "download",
        &session.hkey,
        &session.session_key,
        b"{}",
    )
    .await?;

    tracing::info!(bytes = result.data.len(), "downloaded collection");
    Ok(result.data)
}

/// Upload a modified collection back to AnkiWeb.
pub async fn upload_collection(config: &SyncConfig, collection: &[u8]) -> Result<()> {
    let session = establish_session(config).await?;

    let result = sync_request(
        &session.client,
        &session.endpoint,
        "upload",
        &session.hkey,
        &session.session_key,
        collection,
    )
    .await?;

    let resp_str = String::from_utf8_lossy(&result.data);
    tracing::info!(%resp_str, "upload response");

    // Anki returns "OK" on success
    if !resp_str.contains("OK") && !resp_str.is_empty() {
        return Err(anyhow!("upload may have failed: {resp_str}"));
    }

    Ok(())
}
