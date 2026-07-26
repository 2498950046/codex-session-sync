use std::env;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

pub const DEFAULT_MAX_OBJECT_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub const DEFAULT_MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
const MIN_TOKEN_BYTES: usize = 16;

#[derive(Clone)]
pub struct ServerConfig {
    pub bind: String,
    pub data_dir: PathBuf,
    pub token: String,
    pub max_object_bytes: u64,
    pub max_manifest_bytes: u64,
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServerConfig")
            .field("bind", &self.bind)
            .field("data_dir", &self.data_dir)
            .field("token", &"[redacted]")
            .field("max_object_bytes", &self.max_object_bytes)
            .field("max_manifest_bytes", &self.max_manifest_bytes)
            .finish()
    }
}

impl ServerConfig {
    pub fn from_env() -> Result<Self> {
        let token = validate_token(
            env::var("SYNC_SERVER_TOKEN")
                .context("SYNC_SERVER_TOKEN must be set before the sync server can start")?,
        )?;

        Ok(Self {
            bind: env::var("SYNC_SERVER_BIND").unwrap_or_else(|_| "127.0.0.1:8787".into()),
            data_dir: env::var_os("SYNC_SERVER_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("./data")),
            token,
            max_object_bytes: parse_limit(
                "SYNC_SERVER_MAX_OBJECT_BYTES",
                DEFAULT_MAX_OBJECT_BYTES,
            )?,
            max_manifest_bytes: parse_limit(
                "SYNC_SERVER_MAX_MANIFEST_BYTES",
                DEFAULT_MAX_MANIFEST_BYTES,
            )?,
        })
    }

    #[cfg(test)]
    pub fn for_test(data_dir: PathBuf) -> Self {
        Self {
            bind: "127.0.0.1:0".to_string(),
            data_dir,
            token: "test-token-that-is-long-enough-for-auth".to_string(),
            max_object_bytes: 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
        }
    }
}

fn parse_limit(name: &str, default: u64) -> Result<u64> {
    let Some(value) = env::var_os(name) else {
        return Ok(default);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("{name} must contain valid UTF-8"))?;
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{name} must be greater than zero");
    }
    Ok(parsed)
}

fn validate_token(token: String) -> Result<String> {
    if token.len() < MIN_TOKEN_BYTES {
        bail!("SYNC_SERVER_TOKEN must contain at least {MIN_TOKEN_BYTES} characters");
    }
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        bail!("SYNC_SERVER_TOKEN must contain only visible ASCII characters without spaces");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_token() {
        let config = ServerConfig::for_test(PathBuf::from("data"));
        let rendered = format!("{config:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains(&config.token));
    }

    #[test]
    fn token_must_be_a_nonempty_visible_ascii_value() {
        assert!(validate_token("valid-token_123+/=".to_string()).is_ok());
        assert!(validate_token(String::new()).is_err());
        assert!(validate_token("too-short".to_string()).is_err());
        assert!(validate_token("contains space".to_string()).is_err());
        assert!(validate_token("contains\nnewline".to_string()).is_err());
        assert!(validate_token("令牌".to_string()).is_err());
    }
}
