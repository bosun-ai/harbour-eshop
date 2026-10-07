use std::{env, fs::File, io::BufReader, net::SocketAddr, sync::Arc, time::Duration};

use hyper::Uri;
use rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::CertificateDer};

pub type ConfigError = Box<dyn std::error::Error + Send + Sync>;

pub struct Config {
    pub public_bind: SocketAddr,
    pub ops_bind: SocketAddr,
    pub upstream: Uri,
    pub server_tls: Arc<ServerConfig>,
    pub client_tls: Arc<ClientConfig>,
    pub connect: Duration,
    pub idle: Duration,
    pub total: Duration,
    pub drain: Duration,
    pub log_level: tracing::Level,
}

fn required(name: &str) -> Result<String, ConfigError> {
    env::var(name).map_err(|_| format!("missing {name}").into())
}

pub fn origin(value: &str) -> Result<Uri, ConfigError> {
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
        return Err("legacy URL must be a fixed HTTPS origin".into());
    }
    Ok(uri)
}

fn duration(name: &str, default: u64) -> Result<Duration, ConfigError> {
    let seconds = env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse::<u64>()?;
    if !(1..=3600).contains(&seconds) {
        return Err(format!("{name} must be between 1 and 3600 seconds").into());
    }
    Ok(Duration::from_secs(seconds))
}

fn certificates(path: &str) -> Result<Vec<CertificateDer<'static>>, ConfigError> {
    let certs = rustls_pemfile::certs(&mut BufReader::new(File::open(path)?))
        .collect::<Result<Vec<_>, _>>()?;
    if certs.is_empty() {
        return Err("empty certificate bundle".into());
    }
    Ok(certs)
}

impl Config {
    /// Validate all configuration and TLS material before opening either listener.
    pub fn load() -> Result<Self, ConfigError> {
        crate::dispatch::validate_activation(&env::var("GW_ACTIVE_FAMILIES").unwrap_or_default())?;
        let key =
            rustls_pemfile::private_key(&mut BufReader::new(File::open(required("GW_TLS_KEY")?)?))?
                .ok_or("missing private key")?;
        let mut server = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates(&required("GW_TLS_CERT")?)?, key)?;
        server.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        for cert in certificates(&required("GW_LEGACY_CA")?)? {
            roots.add(cert)?;
        }
        let mut client = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        let public_bind = required("GW_PUBLIC_BIND")?.parse()?;
        let ops_bind: SocketAddr = env::var("GW_OPS_BIND")
            .unwrap_or_else(|_| "127.0.0.1:9000".into())
            .parse()?;
        if !ops_bind.ip().is_loopback() || public_bind == ops_bind {
            return Err("operations listener must be distinct and loopback-only".into());
        }
        let config = Self {
            public_bind,
            ops_bind,
            upstream: origin(&required("GW_LEGACY_URL")?)?,
            server_tls: Arc::new(server),
            client_tls: Arc::new(client),
            connect: duration("GW_CONNECT_SECONDS", 5)?,
            idle: duration("GW_IDLE_SECONDS", 30)?,
            total: duration("GW_TOTAL_SECONDS", 120)?,
            drain: duration("GW_DRAIN_SECONDS", 30)?,
            log_level: env::var("GW_LOG_LEVEL")
                .unwrap_or_else(|_| "info".into())
                .parse()?,
        };
        if config.connect > config.total || config.idle > config.total {
            return Err("connect and idle limits must not exceed total limit".into());
        }
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_origin_only() {
        assert!(origin("https://legacy:8002").is_ok());
        for value in [
            "http://legacy",
            "https://user@legacy",
            "https://legacy/app",
            "https://legacy/?x",
            "https://legacy/#x",
        ] {
            assert!(origin(value).is_err());
        }
    }
}
