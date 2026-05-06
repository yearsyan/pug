use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

use crate::config::Config;

#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    access_token: Option<String>,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UploadInitResponse {
    pub upload_id: String,
    pub engine_tag: String,
    pub s3_key: String,
    pub upload_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CompleteResponse {
    pub engine_tag: String,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccessTokenValidateResponse {
    pub ok: bool,
    pub access_token: AccessTokenInfo,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccessTokenInfo {
    pub id: u64,
    pub user_id: u64,
    pub name: String,
    pub used_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct EngineTagsResponse {
    pub tags: Vec<EngineTag>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct EngineTag {
    pub tag: String,
    pub repo_commit: String,
    pub engine_commit: String,
    pub godot_version: String,
    pub godot_version_short: String,
}

#[derive(Debug, Deserialize)]
pub struct EngineDownloadResponse {
    pub tag: String,
    pub artifacts: Vec<EngineArtifact>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct EngineArtifact {
    pub platform: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub arch: String,
    #[serde(default)]
    pub archs: Vec<String>,
    pub package_sha256: String,
    pub package_size: i64,
    pub download_url: String,
}

#[derive(Debug, Deserialize)]
pub struct ExtensionsResponse {
    pub extensions: Vec<ExtensionSummary>,
}

#[derive(Debug, Deserialize)]
pub struct ExtensionSummary {
    pub name: String,
    pub versions: Vec<String>,
    pub latest: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ExtensionResolveResponse {
    pub name: String,
    pub version: String,
    pub platform: String,
    pub arch: String,
    pub lib_sha256: String,
    pub lib_size: i64,
    pub package_sha256: String,
    pub package_size: i64,
    pub download_url: String,
    pub engine_commit: String,
}

#[derive(Debug, Serialize)]
pub struct EngineUploadInit<'a> {
    pub repo_commit: &'a str,
    pub engine_commit: &'a str,
    pub godot_version: &'a str,
    pub godot_version_short: &'a str,
    pub platform: &'a str,
    #[serde(rename = "type")]
    pub kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archs: Option<&'a [String]>,
    pub package_sha256: &'a str,
    pub package_size: i64,
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct ExtensionUploadInit<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub repo_commit: &'a str,
    pub engine_commit: &'a str,
    pub godot_version: &'a str,
    pub godot_version_short: &'a str,
    pub platform: &'a str,
    pub arch: &'a str,
    pub lib_sha256: &'a str,
    pub lib_size: i64,
    pub package_sha256: &'a str,
    pub package_size: i64,
    pub force: bool,
}

#[derive(Debug, Serialize)]
struct CompleteUpload<'a> {
    upload_id: &'a str,
}

impl ApiClient {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        Ok(Self {
            base_url: cfg.base_url().trim_end_matches('/').to_string(),
            access_token: cfg.access_token(),
            client: Client::builder().build()?,
        })
    }

    pub fn from_config_with_access_token(cfg: &Config, access_token: String) -> Result<Self> {
        Ok(Self {
            base_url: cfg.base_url().trim_end_matches('/').to_string(),
            access_token: Some(access_token),
            client: Client::builder().build()?,
        })
    }

    pub fn validate_access_token(&self) -> Result<AccessTokenValidateResponse> {
        self.authenticated_get_json("/api/v1/access-token/validate")
    }

    pub fn engine_upload_init(&self, req: &EngineUploadInit<'_>) -> Result<UploadInitResponse> {
        self.authenticated_post_json("/api/v1/engines/upload/init", req)
    }

    pub fn engine_upload_complete(&self, upload_id: &str) -> Result<CompleteResponse> {
        self.authenticated_post_json(
            "/api/v1/engines/upload/complete",
            &CompleteUpload { upload_id },
        )
    }

    pub fn extension_upload_init(
        &self,
        req: &ExtensionUploadInit<'_>,
    ) -> Result<UploadInitResponse> {
        self.authenticated_post_json("/api/v1/extensions/upload/init", req)
    }

    pub fn extension_upload_complete(&self, upload_id: &str) -> Result<CompleteResponse> {
        self.authenticated_post_json(
            "/api/v1/extensions/upload/complete",
            &CompleteUpload { upload_id },
        )
    }

    pub fn engine_tags(&self) -> Result<EngineTagsResponse> {
        self.get_json("/api/v1/engines/tags")
    }

    pub fn engine_download(&self, tag: &str) -> Result<EngineDownloadResponse> {
        self.get_json(&format!("/api/v1/engines/download/{tag}"))
    }

    pub fn extensions(&self) -> Result<ExtensionsResponse> {
        self.get_json("/api/v1/extensions")
    }

    pub fn resolve_extension(
        &self,
        name: &str,
        version: Option<&str>,
        platform: &str,
        arch: &str,
    ) -> Result<ExtensionResolveResponse> {
        let version = version.unwrap_or("latest");
        self.get_json(&format!(
            "/api/v1/extensions/resolve?name={}&version={}&platform={}&arch={}",
            url_escape(name),
            url_escape(version),
            url_escape(platform),
            url_escape(arch)
        ))
    }

    pub fn put_file(&self, target: &UploadInitResponse, path: &Path) -> Result<()> {
        let body = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut req = self.client.put(&target.upload_url).body(body);
        for (key, value) in &target.headers {
            req = req.header(key, value);
        }
        let response = req.send().context("upload object to S3")?;
        if !response.status().is_success() {
            bail!("S3 PUT failed with {}", response.status());
        }
        Ok(())
    }

    pub fn download_to(&self, url: &str, path: &Path) -> Result<()> {
        let mut response = self.client.get(url).send().context("download object")?;
        if !response.status().is_success() {
            bail!("download failed with {}", response.status());
        }
        let mut file =
            std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
        std::io::copy(&mut response, &mut file).context("write download")?;
        Ok(())
    }

    fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let response = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .send()?;
        parse_response(response)
    }

    fn authenticated_get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let token = self.access_token.as_deref().context(
            "access token is not configured; run `pug setup-token <token>` before uploading",
        )?;
        let response = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .bearer_auth(token)
            .send()?;
        parse_response(response)
    }

    fn authenticated_post_json<B: Serialize, T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let token = self.access_token.as_deref().context(
            "access token is not configured; run `pug setup-token <token>` before uploading",
        )?;
        let response = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .bearer_auth(token)
            .json(body)
            .send()?;
        parse_response(response)
    }
}

fn parse_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::blocking::Response,
) -> Result<T> {
    let status = response.status();
    let bytes = response.bytes()?;
    if !status.is_success() {
        if let Ok(envelope) = serde_json::from_slice::<ErrorEnvelope>(&bytes) {
            bail!("{}: {}", envelope.error.code, envelope.error.message);
        }
        bail!("HTTP {status}: {}", String::from_utf8_lossy(&bytes));
    }
    serde_json::from_slice(&bytes)
        .with_context(|| format!("decode response: {}", String::from_utf8_lossy(&bytes)))
}

fn url_escape(value: &str) -> String {
    value
        .replace('@', "%40")
        .replace('/', "%2F")
        .replace(':', "%3A")
        .replace(' ', "%20")
}
