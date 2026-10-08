use std::{net::SocketAddr, path::PathBuf, time::Duration};

use hyper::Uri;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    pub public_bind: SocketAddr,
    pub management_bind: SocketAddr,
    pub upstream_url: String,
    pub public_certificate: PathBuf,
    pub public_key: PathBuf,
    pub upstream_ca: PathBuf,
    #[serde(default)]
    pub trusted_proxies: Vec<std::net::IpAddr>,
    #[serde(default)]
    pub enabled_families: Vec<String>,
    pub log_level: LogLevel,
    pub limits: Limits,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn filter(self) -> tracing::level_filters::LevelFilter {
        match self {
            Self::Error => tracing::level_filters::LevelFilter::ERROR,
            Self::Warn => tracing::level_filters::LevelFilter::WARN,
            Self::Info => tracing::level_filters::LevelFilter::INFO,
            Self::Debug => tracing::level_filters::LevelFilter::DEBUG,
            Self::Trace => tracing::level_filters::LevelFilter::TRACE,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct Limits {
    pub connect_ms: u64,
    pub client_header_ms: u64,
    pub body_idle_ms: u64,
    pub upstream_response_ms: u64,
    pub total_request_ms: u64,
    pub readiness_ms: u64,
    pub shutdown_ms: u64,
}

impl Limits {
    pub fn duration(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self, &'static str> {
        let text = std::fs::read_to_string(path).map_err(|_| "config_unreadable")?;
        let config: Self = toml::from_str(&text).map_err(|_| "config_invalid")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<Uri, &'static str> {
        let uri: Uri = self.upstream_url.parse().map_err(|_| "upstream_invalid")?;
        if uri.scheme_str() != Some("https")
            || uri.authority().is_none()
            || uri
                .authority()
                .is_some_and(|value| value.as_str().contains('@'))
            || uri.path() != "/"
            || uri.query().is_some()
            || self.upstream_url.contains('#')
        {
            return Err("upstream_invalid");
        }
        if self.public_bind.port() == 0
            || self.management_bind.port() == 0
            || self.public_bind.port() == self.management_bind.port()
        {
            return Err("bind_invalid");
        }
        let limits = &self.limits;
        for value in [
            limits.connect_ms,
            limits.client_header_ms,
            limits.body_idle_ms,
            limits.upstream_response_ms,
            limits.total_request_ms,
            limits.readiness_ms,
            limits.shutdown_ms,
        ] {
            if value == 0 || value > 3_600_000 {
                return Err("limit_invalid");
            }
        }
        if limits.total_request_ms < limits.upstream_response_ms {
            return Err("limit_invalid");
        }
        Ok(uri)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(include_str!("../config.example.toml")).unwrap()
    }

    #[test]
    fn strict_config_and_limits() {
        let mut config = config();
        assert!(config.validate().is_ok());
        config.limits.body_idle_ms = 0;
        assert!(config.validate().is_err());
        assert!(
            toml::from_str::<Config>(&format!(
                "unexpected = true\n{}",
                include_str!("../config.example.toml")
            ))
            .is_err()
        );
    }

    #[test]
    fn fixed_https_authority_only() {
        for invalid in [
            "http://legacy:8002/",
            "https://user@legacy:8002/",
            "https://legacy:8002/base",
            "https://legacy:8002/?x=1",
            "https://legacy:8002/#fragment",
        ] {
            let mut config = config();
            config.upstream_url = invalid.into();
            assert!(config.validate().is_err(), "{invalid}");
        }
    }
}
