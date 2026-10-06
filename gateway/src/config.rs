use std::{collections::HashSet, env, net::SocketAddr, path::PathBuf, time::Duration};

use thiserror::Error;
use url::Url;

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub bind_address: SocketAddr,
    pub public_certificate: PathBuf,
    pub public_key: PathBuf,
    pub legacy_url: Url,
    pub upstream_ca: PathBuf,
    pub upstream_server_name: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub response_idle_timeout: Duration,
    pub log_level: String,
    pub enabled_slices: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{name} is invalid: {reason}")]
    Invalid { name: &'static str, reason: String },
    #[error("{0} must name a readable file")]
    File(&'static str),
    #[error("GATEWAY_ENABLED_SLICES contains duplicate slice {0}")]
    DuplicateSlice(String),
}

impl GatewayConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let bind_address = required("GATEWAY_BIND_ADDRESS")?
            .parse::<SocketAddr>()
            .map_err(|error| ConfigError::Invalid {
                name: "GATEWAY_BIND_ADDRESS",
                reason: error.to_string(),
            })?;
        let public_certificate = required_file("GATEWAY_PUBLIC_CERT_PEM")?;
        let public_key = required_file("GATEWAY_PUBLIC_KEY_PEM")?;
        let legacy_url =
            Url::parse(&required("GATEWAY_LEGACY_URL")?).map_err(|error| ConfigError::Invalid {
                name: "GATEWAY_LEGACY_URL",
                reason: error.to_string(),
            })?;
        if legacy_url.scheme() != "https"
            || legacy_url.host_str().is_none()
            || legacy_url.path() != "/"
            || legacy_url.query().is_some()
        {
            return Err(ConfigError::Invalid {
                name: "GATEWAY_LEGACY_URL",
                reason: "must be an HTTPS origin without path or query".into(),
            });
        }
        let upstream_ca = required_file("GATEWAY_UPSTREAM_CA_PEM")?;
        let upstream_server_name = required("GATEWAY_UPSTREAM_TLS_SERVER_NAME")?;
        if upstream_server_name.trim().is_empty() {
            return Err(ConfigError::Invalid {
                name: "GATEWAY_UPSTREAM_TLS_SERVER_NAME",
                reason: "cannot be empty".into(),
            });
        }
        if legacy_url.host_str() != Some(upstream_server_name.as_str()) {
            return Err(ConfigError::Invalid {
                name: "GATEWAY_UPSTREAM_TLS_SERVER_NAME",
                reason: "must match the hostname in GATEWAY_LEGACY_URL".into(),
            });
        }
        let enabled_slices = env::var("GATEWAY_ENABLED_SLICES")
            .unwrap_or_default()
            .split(',')
            .filter(|slice| !slice.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        let mut seen = HashSet::new();
        for slice in &enabled_slices {
            if !seen.insert(slice) {
                return Err(ConfigError::DuplicateSlice(slice.clone()));
            }
        }
        let log_level = required("GATEWAY_LOG_LEVEL")?;
        if !matches!(
            log_level.as_str(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            return Err(ConfigError::Invalid {
                name: "GATEWAY_LOG_LEVEL",
                reason: "must be trace, debug, info, warn, or error".into(),
            });
        }
        Ok(Self {
            bind_address,
            public_certificate,
            public_key,
            legacy_url,
            upstream_ca,
            upstream_server_name,
            connect_timeout: seconds("GATEWAY_CONNECT_TIMEOUT_SECS")?,
            request_timeout: seconds("GATEWAY_REQUEST_TIMEOUT_SECS")?,
            response_idle_timeout: seconds("GATEWAY_RESPONSE_IDLE_TIMEOUT_SECS")?,
            log_level,
            enabled_slices,
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    env::var(name).map_err(|_| ConfigError::Missing(name))
}
fn required_file(name: &'static str) -> Result<PathBuf, ConfigError> {
    let path = PathBuf::from(required(name)?);
    if path.is_file() {
        Ok(path)
    } else {
        Err(ConfigError::File(name))
    }
}
fn seconds(name: &'static str) -> Result<Duration, ConfigError> {
    let value = required(name)?
        .parse::<u64>()
        .map_err(|error| ConfigError::Invalid {
            name,
            reason: error.to_string(),
        })?;
    if value == 0 {
        return Err(ConfigError::Invalid {
            name,
            reason: "must be greater than zero".into(),
        });
    }
    Ok(Duration::from_secs(value))
}
