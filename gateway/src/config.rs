use std::{
    collections::HashSet, env, fs::File, io::BufReader, net::SocketAddr, sync::Arc, time::Duration,
};

use hyper::Uri;
use rustls::{ClientConfig, RootCertStore, ServerConfig};

pub struct Config {
    pub public_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub upstream: Uri,
    pub enabled: HashSet<String>,
    pub connect: Duration,
    pub client_read: Duration,
    pub upstream_timeout: Duration,
    pub shutdown: Duration,
    pub log_level: tracing::Level,
    pub public_tls: Arc<ServerConfig>,
    pub upstream_tls: Arc<ClientConfig>,
}

fn setting(name: &str, default: Option<&str>) -> Result<String, String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(env::VarError::NotPresent) => default
            .map(str::to_owned)
            .ok_or(format!("{name}: required")),
        Err(_) => Err(format!("{name}: invalid text")),
    }
}

fn duration(name: &str, default: &str) -> Result<Duration, String> {
    let seconds = setting(name, Some(default))?.parse::<u64>().ok();
    match seconds {
        Some(seconds @ 1..=300) => Ok(Duration::from_secs(seconds)),
        _ => Err(format!("{name}: expected seconds in 1..=300")),
    }
}

fn reader(name: &str) -> Result<BufReader<File>, String> {
    File::open(setting(name, None)?)
        .map(BufReader::new)
        .map_err(|_| format!("{name}: unreadable file"))
}

pub fn origin(value: &str) -> Result<Uri, String> {
    let uri: Uri = value.parse().map_err(|_| "LEGACY_URL: invalid origin")?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
        || uri.path() != "/"
        || uri.query().is_some()
        || value.contains('#')
        || uri.port_u16() == Some(0)
        || uri.authority().is_some_and(|authority| {
            let authority = authority.as_str();
            let port_suffix = if authority.starts_with('[') {
                authority.split_once(']').map_or("", |(_, suffix)| suffix)
            } else {
                authority.split_once(':').map_or("", |(_, port)| port)
            };
            !port_suffix.is_empty() && uri.port_u16().is_none()
        })
    {
        return Err(
            "LEGACY_URL: expected HTTPS origin without credentials, query or base path".into(),
        );
    }
    let host = uri
        .host()
        .unwrap()
        .trim_start_matches('[')
        .trim_end_matches(']');
    rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| "LEGACY_URL: invalid TLS server name")?;
    Ok(uri)
}

impl Config {
    pub fn load() -> Result<Self, String> {
        let public_bind = setting("PUBLIC_BIND", Some("0.0.0.0:8002"))?
            .parse()
            .map_err(|_| "PUBLIC_BIND: invalid socket address")?;
        let admin_bind = setting("ADMIN_BIND", Some("0.0.0.0:8003"))?
            .parse()
            .map_err(|_| "ADMIN_BIND: invalid socket address")?;
        if public_bind == admin_bind {
            return Err("ADMIN_BIND: must differ from PUBLIC_BIND".into());
        }
        let enabled_text = setting("ENABLED_OWNERS", Some(""))?;
        let mut enabled = HashSet::new();
        if !enabled_text.is_empty() {
            for owner in enabled_text.split(',') {
                if owner.is_empty() || owner.trim() != owner || !enabled.insert(owner.to_owned()) {
                    return Err("ENABLED_OWNERS: invalid or duplicate ID".into());
                }
            }
        }
        let certs = rustls_pemfile::certs(&mut reader("PUBLIC_TLS_CERT")?)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "PUBLIC_TLS_CERT: invalid PEM")?;
        let key = rustls_pemfile::private_key(&mut reader("PUBLIC_TLS_KEY")?)
            .map_err(|_| "PUBLIC_TLS_KEY: invalid PEM")?
            .ok_or("PUBLIC_TLS_KEY: missing key")?;
        let public_tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| "PUBLIC_TLS_CERT/PUBLIC_TLS_KEY: invalid or mismatched pair")?;
        let mut roots = RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut reader("LEGACY_CA_FILE")?) {
            roots
                .add(cert.map_err(|_| "LEGACY_CA_FILE: invalid PEM")?)
                .map_err(|_| "LEGACY_CA_FILE: invalid certificate")?;
        }
        if roots.is_empty() {
            return Err("LEGACY_CA_FILE: empty trust store".into());
        }
        Ok(Self {
            public_bind,
            admin_bind,
            enabled,
            upstream: origin(&setting("LEGACY_URL", None)?)?,
            connect: duration("CONNECT_TIMEOUT_SECONDS", "10")?,
            client_read: duration("CLIENT_READ_TIMEOUT_SECONDS", "120")?,
            upstream_timeout: duration("UPSTREAM_TIMEOUT_SECONDS", "150")?,
            shutdown: duration("SHUTDOWN_TIMEOUT_SECONDS", "30")?,
            log_level: setting("LOG_LEVEL", Some("info"))?
                .parse()
                .map_err(|_| "LOG_LEVEL: invalid level")?,
            public_tls: Arc::new(public_tls),
            upstream_tls: Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_https_origin_only() {
        assert!(origin("https://legacy:8002").is_ok());
        for invalid in [
            "http://legacy",
            "https://user:pass@legacy",
            "https://legacy/app",
            "https://legacy/?secret",
            "https://legacy/#fragment",
            "https://legacy:65536",
            "https://legacy:0",
            "//legacy",
        ] {
            assert!(origin(invalid).is_err());
        }
    }
}
