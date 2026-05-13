use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::Command,
};

use crate::api::ApiClient;

const DEFAULT_BASE_URL: &str = "https://example.com";
const AUTH_ACCESS_TOKEN: &str = "access_token";
const AUTH_LOGIN_SESSION: &str = "login_session";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub backend: BackendConfig,
    pub engine: EngineConfig,
    pub extension: ExtensionConfig,
    pub build: BuildConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendConfig {
    pub base_url: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub login_session: String,
    #[serde(default)]
    pub active_auth: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub install_dir: String,
    pub current: String,
    pub editor_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionConfig {
    pub auto_fetch_engine: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    pub repo_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendConfig {
                base_url: DEFAULT_BASE_URL.to_string(),
                access_token: String::new(),
                login_session: String::new(),
                active_auth: String::new(),
            },
            engine: EngineConfig {
                install_dir: "~/.local/share/pug/engines".to_string(),
                current: String::new(),
                editor_path: String::new(),
            },
            extension: ExtensionConfig {
                auto_fetch_engine: true,
            },
            build: BuildConfig {
                repo_path: String::new(),
            },
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.is_file() {
            let cfg = Self::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn base_url(&self) -> String {
        std::env::var("PANNEL_BASE_URL").unwrap_or_else(|_| self.backend.base_url.clone())
    }

    pub fn access_token(&self) -> Option<String> {
        std::env::var("PANNEL_ACCESS_TOKEN")
            .ok()
            .and_then(clean_token)
            .or_else(|| self.configured_access_token())
    }

    pub fn uses_login_session_auth(&self) -> bool {
        if std::env::var("PANNEL_ACCESS_TOKEN")
            .ok()
            .and_then(clean_token)
            .is_some()
        {
            return false;
        }
        self.configured_auth_uses_login_session()
    }

    fn configured_access_token(&self) -> Option<String> {
        match self.backend.active_auth.trim() {
            AUTH_LOGIN_SESSION => clean_token(self.backend.login_session.clone())
                .or_else(|| clean_token(self.backend.access_token.clone())),
            AUTH_ACCESS_TOKEN => clean_token(self.backend.access_token.clone())
                .or_else(|| clean_token(self.backend.login_session.clone())),
            _ => clean_token(self.backend.access_token.clone())
                .or_else(|| clean_token(self.backend.login_session.clone())),
        }
    }

    fn configured_auth_uses_login_session(&self) -> bool {
        let has_access_token = !self.backend.access_token.trim().is_empty();
        let has_login_session = !self.backend.login_session.trim().is_empty();
        match self.backend.active_auth.trim() {
            AUTH_LOGIN_SESSION => has_login_session,
            AUTH_ACCESS_TOKEN => !has_access_token && has_login_session,
            _ => !has_access_token && has_login_session,
        }
    }

    pub fn verify_access_token(&self) -> Result<()> {
        if self.access_token().is_none() {
            bail!(
                "authentication is not configured; run `pug login` or `pug setup-token <token>` before uploading"
            );
        }
        ApiClient::from_config(self)?
            .validate_access_token()
            .with_context(|| format!("validate access token against {}", self.base_url()))?;
        Ok(())
    }

    pub fn install_dir(&self) -> Result<PathBuf> {
        expand_tilde(&self.engine.install_dir)
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dir = config_dir()?;
    Ok(dir.join("pug").join("config.toml"))
}

#[cfg(target_os = "macos")]
fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    Ok(dirs::home_dir()
        .context("cannot resolve home directory")?
        .join(".config"))
}

#[cfg(not(target_os = "macos"))]
fn config_dir() -> Result<PathBuf> {
    dirs::config_dir().context("cannot resolve config directory")
}

pub fn setup_access_token(token: Option<String>) -> Result<()> {
    let token = match token {
        Some(token) => token,
        None => {
            eprint!("access token: ");
            io::stderr().flush()?;
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            input
        }
    };
    let token = token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("access token must not be empty");
    }

    let mut cfg = Config::load()?;
    ApiClient::from_config_with_access_token(&cfg, token.clone())?
        .validate_access_token()
        .with_context(|| format!("validate access token against {}", cfg.base_url()))?;
    cfg.backend.access_token = token;
    cfg.backend.active_auth = AUTH_ACCESS_TOKEN.to_string();
    cfg.save()?;
    println!(
        "verified access token against {}",
        cfg.base_url().trim_end_matches('/')
    );
    println!("saved access token to {}", config_path()?.display());
    Ok(())
}

pub fn expand_tilde(value: &str) -> Result<PathBuf> {
    if value == "~" {
        return dirs::home_dir().context("cannot resolve home directory");
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Ok(dirs::home_dir()
            .context("cannot resolve home directory")?
            .join(rest));
    }
    Ok(PathBuf::from(value))
}

pub fn setup_login_session(token: String) -> Result<()> {
    let token = token.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("login session must not be empty");
    }
    let mut cfg = Config::load()?;
    ApiClient::from_config_with_access_token(&cfg, token.clone())?
        .validate_access_token()
        .with_context(|| format!("validate login session against {}", cfg.base_url()))?;
    cfg.backend.login_session = token;
    cfg.backend.active_auth = AUTH_LOGIN_SESSION.to_string();
    cfg.save()?;
    println!("saved pug login session to {}", config_path()?.display());
    Ok(())
}

pub fn login() -> Result<()> {
    let cfg = Config::load()?;
    let listener = TcpListener::bind("127.0.0.1:0").context("start local login callback")?;
    let callback = format!("http://{}/callback", listener.local_addr()?);
    let state = local_state();
    let login_path = format!(
        "/cli-login/complete?callback={}&state={}",
        query_escape(&callback),
        query_escape(&state)
    );
    let base = cfg.base_url().trim_end_matches('/').to_string();
    let login_url = format!("{base}{login_path}");
    open_browser(&login_url)?;
    println!("opened browser for pug login");
    println!("waiting for local callback at {callback}");

    let (mut stream, _) = listener.accept().context("wait for login callback")?;
    let mut buf = [0_u8; 4096];
    let n = stream.read(&mut buf).context("read login callback")?;
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    let first_line = request.lines().next().unwrap_or_default();
    let path = first_line.split_whitespace().nth(1).unwrap_or_default();
    let params = parse_query(path.split_once('?').map(|(_, q)| q).unwrap_or_default());
    let got_state = params
        .iter()
        .find(|(key, _)| key == "state")
        .map(|(_, value)| value.as_str())
        .unwrap_or_default();
    let token = params
        .iter()
        .find(|(key, _)| key == "token")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    if got_state != state || token.is_empty() {
        let _ = stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\n\r\npug login failed\n",
        );
        anyhow::bail!("login callback did not include a valid session");
    }
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\npug login complete. You can close this window.\n");
    setup_login_session(token)
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(url);
        cmd
    };
    #[cfg(target_os = "linux")]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(url);
        cmd
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("rundll32");
        cmd.args(["url.dll,FileProtocolHandler", url]);
        cmd
    };
    cmd.spawn().context("open browser")?;
    Ok(())
}

fn local_state() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}-{:x}", nanos, std::process::id())
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![b as char]
            }
            _ => format!("%{b:02X}").chars().collect(),
        })
        .collect()
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (percent_decode(key), percent_decode(value)))
        .collect()
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16)
        {
            out.push(hex);
            index += 3;
            continue;
        }
        out.push(if bytes[index] == b'+' {
            b' '
        } else {
            bytes[index]
        });
        index += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

fn clean_token(token: String) -> Option<String> {
    let token = token.trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_auth(access_token: &str, login_session: &str, active_auth: &str) -> Config {
        Config {
            backend: BackendConfig {
                base_url: DEFAULT_BASE_URL.to_string(),
                access_token: access_token.to_string(),
                login_session: login_session.to_string(),
                active_auth: active_auth.to_string(),
            },
            ..Config::default()
        }
    }

    #[test]
    fn configured_auth_uses_last_login_session() {
        let cfg = config_with_auth("project-token", "login-session", AUTH_LOGIN_SESSION);

        assert_eq!(
            cfg.configured_access_token().as_deref(),
            Some("login-session")
        );
    }

    #[test]
    fn configured_auth_uses_last_access_token() {
        let cfg = config_with_auth("project-token", "login-session", AUTH_ACCESS_TOKEN);

        assert_eq!(
            cfg.configured_access_token().as_deref(),
            Some("project-token")
        );
    }

    #[test]
    fn configured_auth_falls_back_when_active_value_is_empty() {
        let cfg = config_with_auth("", "login-session", AUTH_ACCESS_TOKEN);

        assert_eq!(
            cfg.configured_access_token().as_deref(),
            Some("login-session")
        );
    }

    #[test]
    fn configured_auth_keeps_legacy_token_precedence() {
        let cfg = config_with_auth("project-token", "login-session", "");

        assert_eq!(
            cfg.configured_access_token().as_deref(),
            Some("project-token")
        );
    }

    #[test]
    fn configured_login_auth_tracks_effective_auth_source() {
        assert!(
            config_with_auth("project-token", "login-session", AUTH_LOGIN_SESSION)
                .configured_auth_uses_login_session()
        );
        assert!(
            !config_with_auth("project-token", "login-session", AUTH_ACCESS_TOKEN)
                .configured_auth_uses_login_session()
        );
        assert!(
            config_with_auth("", "login-session", AUTH_ACCESS_TOKEN)
                .configured_auth_uses_login_session()
        );
        assert!(
            !config_with_auth("project-token", "login-session", "")
                .configured_auth_uses_login_session()
        );
    }
}
