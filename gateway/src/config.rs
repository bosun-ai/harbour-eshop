//! Startup-only environment validation and TLS loading.
use crate::Error;
use hyper::Uri;
use std::{env, fs::File, io::BufReader, net::SocketAddr, time::Duration};
use tokio_rustls::rustls::{ServerConfig, pki_types::CertificateDer};

pub struct Config {
    pub public: SocketAddr,
    pub admin: SocketAddr,
    pub tls: ServerConfig,
    pub origin: Uri,
    pub ca: Vec<CertificateDer<'static>>,
    pub enabled: Vec<String>,
    pub connect: Duration,
    pub headers: Duration,
    pub idle: Duration,
    pub drain: Duration,
    pub log_level: tracing::level_filters::LevelFilter,
}

fn setting(name: &str) -> Result<String, Error> {
    env::var(name).map_err(|_| format!("{name} is required").into())
}

fn duration(name: &str, default: &str) -> Result<Duration, Error> {
    positive_duration(&env::var(name).unwrap_or_else(|_| default.into()))
        .map_err(|_| format!("{name} must be positive integer seconds").into())
}

fn positive_duration(value: &str) -> Result<Duration, Error> {
    let seconds: u64 = value.parse()?;
    if seconds == 0 || seconds > 86400 {
        return Err("timeout outside 1..86400".into());
    }
    Ok(Duration::from_secs(seconds))
}

pub fn certificates(path: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let mut reader = BufReader::new(File::open(path)?);
    let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err("empty certificate file".into());
    }
    Ok(certs)
}

fn origin(value: &str) -> Result<Uri, Error> {
    let uri: Uri = value.parse()?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        || uri.path() != "/"
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err("invalid HTTPS origin".into());
    }
    Ok(uri)
}

impl Config {
    pub fn from_env() -> Result<Self, Error> {
        let certs = certificates(&setting("GATEWAY_TLS_CERT")?)
            .map_err(|_| "GATEWAY_TLS_CERT is invalid or unreadable")?;
        let mut reader = BufReader::new(
            File::open(setting("GATEWAY_TLS_KEY")?).map_err(|_| "GATEWAY_TLS_KEY is unreadable")?,
        );
        let key = rustls_pemfile::private_key(&mut reader)
            .map_err(|_| "GATEWAY_TLS_KEY is invalid")?
            .ok_or("GATEWAY_TLS_KEY is empty")?;
        let tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| "public TLS certificate/key mismatch")?;
        let public = env::var("GATEWAY_PUBLIC_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8002".into())
            .parse()
            .map_err(|_| "GATEWAY_PUBLIC_BIND is invalid")?;
        let admin = env::var("GATEWAY_ADMIN_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8003".into())
            .parse()
            .map_err(|_| "GATEWAY_ADMIN_BIND is invalid")?;
        if public == admin {
            return Err("public and admin binds must differ".into());
        }
        Ok(Self {
            public,
            admin,
            tls,
            origin: origin(&setting("GATEWAY_LEGACY_URL")?)
                .map_err(|_| "GATEWAY_LEGACY_URL must be an HTTPS origin")?,
            ca: certificates(&setting("GATEWAY_LEGACY_CA")?)
                .map_err(|_| "GATEWAY_LEGACY_CA is invalid or unreadable")?,
            enabled: env::var("GATEWAY_ENABLED_SLICES")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
            connect: duration("GATEWAY_CONNECT_SECONDS", "5")?,
            headers: duration("GATEWAY_HEADER_SECONDS", "30")?,
            idle: duration("GATEWAY_IDLE_SECONDS", "30")?,
            drain: duration("GATEWAY_DRAIN_SECONDS", "10")?,
            log_level: env::var("GATEWAY_LOG_LEVEL")
                .unwrap_or_else(|_| "info".into())
                .parse()
                .map_err(|_| "GATEWAY_LOG_LEVEL is invalid")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_origins_and_bounds() {
        for bad in [
            "http://legacy:8002",
            "https://u:p@legacy",
            "https://legacy/base",
            "https://legacy/?q",
            "https://legacy/#x",
        ] {
            assert!(origin(bad).is_err());
        }
        for good in ["https://legacy:8002", "https://localhost/"] {
            assert!(origin(good).is_ok());
        }
        for bad in ["0", "-1", "no", "86401"] {
            assert!(positive_duration(bad).is_err());
        }
        assert!(positive_duration("1").is_ok());
    }
}
