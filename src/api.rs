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
struct ApiEnvelope<T> {
    data: T,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    data: ApiErrorData,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ApiErrorData {
    error_code: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct UploadInitResponse {
    pub upload_id: String,
    pub engine_tag: Option<String>,
    pub s3_key: String,
    pub upload_url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CompleteResponse {
    pub engine_tag: Option<String>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccessTokenValidateResponse {
    pub ok: bool,
    pub access_token: Option<AccessTokenInfo>,
    pub cli_session: Option<CLISessionInfo>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AccessTokenInfo {
    pub id: u64,
    pub project_id: Option<u64>,
    pub created_by_user_id: Option<u64>,
    pub name: String,
    pub used_at: Option<i64>,
    pub created_at: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct CLISessionInfo {
    pub id: u64,
    pub user_id: u64,
    pub name: String,
    pub used_at: Option<i64>,
    pub created_at: i64,
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
    pub project_name: &'a str,
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
    pub project_name: &'a str,
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
pub struct ExportUploadInit<'a> {
    pub project_name: &'a str,
    pub version: &'a str,
    pub platform: &'a str,
    pub mode: &'a str,
    pub package_type: &'a str,
    pub package_sha256: &'a str,
    pub package_size: i64,
    pub engine_tag: &'a str,
    pub repo_commit: &'a str,
    pub export_path: &'a str,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct DownloadablePackageUploadInit<'a> {
    pub project_name: &'a str,
    pub name: &'a str,
    pub version: &'a str,
    pub platform: &'a str,
    pub mode: &'a str,
    pub pack_path: &'a str,
    pub package_sha256: &'a str,
    pub package_size: i64,
    pub engine_tag: &'a str,
    pub repo_commit: &'a str,
    pub encrypt_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encryption_key: Option<&'a str>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct EditorTokenResponse {
    pub editor_token: String,
    pub expires_at: i64,
    pub scope: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ManifestPublicKeyResponse {
    pub manifest_public_key_pem: String,
    pub manifest_public_key_sha256: String,
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
        self.authenticated_get_json("/cli-api/v1/access-token/validate")
    }

    pub fn engine_upload_init(&self, req: &EngineUploadInit<'_>) -> Result<UploadInitResponse> {
        self.authenticated_post_json("/cli-api/v1/engines/upload/init", req)
    }

    pub fn engine_upload_complete(&self, upload_id: &str) -> Result<CompleteResponse> {
        self.authenticated_post_json(
            "/cli-api/v1/engines/upload/complete",
            &CompleteUpload { upload_id },
        )
    }

    pub fn extension_upload_init(
        &self,
        req: &ExtensionUploadInit<'_>,
    ) -> Result<UploadInitResponse> {
        self.authenticated_post_json("/cli-api/v1/extensions/upload/init", req)
    }

    pub fn extension_upload_complete(&self, upload_id: &str) -> Result<CompleteResponse> {
        self.authenticated_post_json(
            "/cli-api/v1/extensions/upload/complete",
            &CompleteUpload { upload_id },
        )
    }

    pub fn editor_token(&self, project: &str) -> Result<EditorTokenResponse> {
        self.authenticated_post_json(
            &format!("/cli-api/v1/projects/{}/editor-token", url_escape(project)),
            &serde_json::json!({}),
        )
    }

    pub fn manifest_public_key(&self, project: &str) -> Result<ManifestPublicKeyResponse> {
        self.authenticated_get_json(&format!(
            "/cli-api/v1/projects/{}/manifest-public-key",
            url_escape(project)
        ))
    }

    pub fn export_upload_init(&self, req: &ExportUploadInit<'_>) -> Result<UploadInitResponse> {
        self.authenticated_post_json("/cli-api/v1/exports/upload/init", req)
    }

    pub fn export_upload_complete(&self, upload_id: &str) -> Result<CompleteResponse> {
        self.authenticated_post_json(
            "/cli-api/v1/exports/upload/complete",
            &CompleteUpload { upload_id },
        )
    }

    pub fn downloadable_package_upload_init(
        &self,
        req: &DownloadablePackageUploadInit<'_>,
    ) -> Result<UploadInitResponse> {
        self.authenticated_post_json("/cli-api/v1/downloadable-packages/upload/init", req)
    }

    pub fn downloadable_package_upload_complete(
        &self,
        upload_id: &str,
    ) -> Result<CompleteResponse> {
        self.authenticated_post_json(
            "/cli-api/v1/downloadable-packages/upload/complete",
            &CompleteUpload { upload_id },
        )
    }

    pub fn engine_tags_for_project(&self, project: &str) -> Result<EngineTagsResponse> {
        self.authenticated_get_json(&format!(
            "/cli-api/v1/projects/{}/engines/tags",
            url_escape(project)
        ))
    }

    pub fn engine_download(&self, project: &str, tag: &str) -> Result<EngineDownloadResponse> {
        self.authenticated_get_json(&format!(
            "/cli-api/v1/projects/{}/engines/download/{}",
            url_escape(project),
            url_escape(tag)
        ))
    }

    pub fn extensions(&self, project: &str) -> Result<ExtensionsResponse> {
        self.authenticated_get_json(&format!(
            "/cli-api/v1/projects/{}/extensions",
            url_escape(project)
        ))
    }

    pub fn resolve_extension(
        &self,
        project: &str,
        name: &str,
        version: Option<&str>,
        platform: &str,
        arch: &str,
    ) -> Result<ExtensionResolveResponse> {
        let version = version.unwrap_or("latest");
        self.authenticated_get_json(&format!(
            "/cli-api/v1/projects/{}/extensions/resolve?name={}&version={}&platform={}&arch={}",
            url_escape(project),
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

    fn authenticated_get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let token = self.access_token.as_deref().context(
            "authentication is not configured; run `pug login` or `pug setup-token <token>` before uploading",
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
            "authentication is not configured; run `pug login` or `pug setup-token <token>` before uploading",
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
            bail!("{}: {}", envelope.data.error_code, envelope.message);
        }
        bail!("HTTP {status}: {}", String::from_utf8_lossy(&bytes));
    }
    let envelope: ApiEnvelope<T> = serde_json::from_slice(&bytes)
        .with_context(|| format!("decode response: {}", String::from_utf8_lossy(&bytes)))?;
    Ok(envelope.data)
}

fn url_escape(value: &str) -> String {
    value
        .replace('@', "%40")
        .replace('/', "%2F")
        .replace(':', "%3A")
        .replace(' ', "%20")
}
