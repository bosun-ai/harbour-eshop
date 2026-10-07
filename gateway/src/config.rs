//! Environment parsing and TLS material validation (no insecure mode).
use crate::Error;
use hyper::Uri;
use std::{env, fs::File, io::BufReader, net::SocketAddr, sync::Arc, time::Duration};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};

/// Validated startup configuration, shared by serving and CLI probes.
pub struct Config {
    pub bind: SocketAddr,
    pub upstream: Uri,
    pub active: Vec<String>,
    pub connect: Duration,
    pub exchange: Duration,
    pub drain: Duration,
    pub level: tracing::Level,
    pub public_tls: Arc<ServerConfig>,
    pub private_tls: Arc<ClientConfig>,
}

fn required(name: &str) -> Result<String, Error> {
    env::var(name).map_err(|_| format!("{name}: required").into())
}

fn seconds(name: &str, default: u64) -> Result<Duration, Error> {
    let value = env::var(name).unwrap_or_else(|_| default.to_string());
    parse_seconds(&value).map_err(|_| format!("{name}: expected seconds in 1..=3600").into())
}

fn parse_seconds(value: &str) -> Result<Duration, Error> {
    let value: u64 = value.parse()?;
    if !(1..=3600).contains(&value) {
        return Err("timeout out of bounds".into());
    }
    Ok(Duration::from_secs(value))
}

/// Parse only an HTTPS origin, never a credential-bearing URL or base path.
pub fn origin(value: &str) -> Result<Uri, Error> {
    let uri: Uri = value
        .parse()
        .map_err(|_| "LEGACY_UPSTREAM_URL: invalid origin")?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri.authority().is_none_or(|a| a.as_str().contains('@'))
        || uri.path() != "/"
        || uri.query().is_some()
        || value.contains('#')
        || uri.port_u16() == Some(0)
        || uri.authority().is_some_and(|authority| {
            authority
                .as_str()
                .rsplit_once(':')
                .is_some_and(|(_, port)| !port.contains(']') && port.parse::<u16>().is_err())
        })
    {
        return Err("LEGACY_UPSTREAM_URL: expected HTTPS origin".into());
    }
    Ok(uri)
}

fn certificates(
    name: &str,
) -> Result<Vec<tokio_rustls::rustls::pki_types::CertificateDer<'static>>, Error> {
    let result = (|| {
        let mut reader = BufReader::new(File::open(required(name)?)?);
        let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
        if certs.is_empty() {
            return Err("empty certificate bundle".into());
        }
        Ok(certs)
    })();
    result.map_err(|_: Error| format!("{name}: unreadable or invalid PEM").into())
}

impl Config {
    /// Read and validate environment without disclosing values on failure.
    pub fn load() -> Result<Self, Error> {
        let bind = env::var("GATEWAY_BIND")
            .unwrap_or_else(|_| "0.0.0.0:8002".into())
            .parse::<SocketAddr>()
            .map_err(|_| "GATEWAY_BIND: invalid socket address")?;
        if bind.port() == 0 {
            return Err("GATEWAY_BIND: port must be nonzero".into());
        }
        let upstream = origin(&required("LEGACY_UPSTREAM_URL")?)?;
        let certs = certificates("GATEWAY_TLS_CERT_FILE")?;
        let key = (|| -> Result<_, Error> {
            let mut reader = BufReader::new(File::open(required("GATEWAY_TLS_KEY_FILE")?)?);
            rustls_pemfile::private_key(&mut reader)?.ok_or_else(|| "missing key".into())
        })()
        .map_err(|_| "GATEWAY_TLS_KEY_FILE: unreadable or invalid PEM")?;
        let public_tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| "public TLS: invalid certificate/key pairing")?;
        let mut roots = RootCertStore::empty();
        for cert in certificates("LEGACY_TLS_CA_FILE")? {
            roots
                .add(cert)
                .map_err(|_| "LEGACY_TLS_CA_FILE: invalid certificate")?;
        }
        let private_tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let active = env::var("GATEWAY_ACTIVE_FAMILIES")
            .unwrap_or_default()
            .split(',')
            .filter(|id| !id.trim().is_empty())
            .map(|id| id.trim().to_owned())
            .collect();
        let level = env::var("GATEWAY_LOG_LEVEL")
            .unwrap_or_else(|_| "INFO".into())
            .parse()
            .map_err(|_| "GATEWAY_LOG_LEVEL: invalid level")?;
        Ok(Self {
            bind,
            upstream,
            active,
            connect: seconds("GATEWAY_CONNECT_SECONDS", 5)?,
            exchange: seconds("GATEWAY_EXCHANGE_SECONDS", 180)?,
            drain: seconds("GATEWAY_DRAIN_SECONDS", 30)?,
            level,
            public_tls: Arc::new(public_tls),
            private_tls: Arc::new(private_tls),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origins_and_bounds() {
        assert!(origin("https://legacy:8002").is_ok());
        for bad in [
            "http://legacy",
            "https://u:p@legacy",
            "https://legacy/a",
            "https://legacy/?x",
            "https://legacy/#x",
            "https://legacy:0",
            "https://legacy:99999",
        ] {
            assert!(origin(bad).is_err(), "{bad}");
        }
        for bad in ["0", "3601", "-1", "no"] {
            assert!(parse_seconds(bad).is_err());
        }
        assert!(parse_seconds("180").is_ok());
    }
}
