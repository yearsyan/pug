use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use crate::api::ApiClient;

const DEFAULT_BASE_URL: &str = "https://example.com";

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
            .or_else(|| {
                (!self.backend.access_token.trim().is_empty())
                    .then(|| self.backend.access_token.clone())
            })
            .map(|token| token.trim().to_string())
            .filter(|token| !token.is_empty())
    }

    pub fn verify_access_token(&self) -> Result<()> {
        if self.access_token().is_none() {
            bail!("access token is not configured; run `pug setup-token <token>` before uploading");
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
    let dir = dirs::config_dir().context("cannot resolve config directory")?;
    Ok(dir.join("pug").join("config.toml"))
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
