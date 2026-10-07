//! Explicit, fail-closed configuration and TLS provisioning.
use ipnet::IpNet;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use serde::Deserialize;
use std::{fs, io::BufReader, net::SocketAddr, path::Path, sync::Arc, time::Duration};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub public_bind: SocketAddr,
    pub management_bind: SocketAddr,
    pub legacy_url: String,
    pub public_cert_file: String,
    pub public_key_file: String,
    pub upstream_ca_file: String,
    #[serde(default)]
    pub trusted_proxy_cidrs: Vec<IpNet>,
    pub connect_timeout_ms: u64,
    pub header_timeout_ms: u64,
    pub body_idle_timeout_ms: u64,
    pub response_timeout_ms: u64,
    pub shutdown_drain_ms: u64,
    pub log_level: String,
    #[serde(default)]
    pub active_families: Vec<String>,
}

pub struct ValidatedConfig {
    pub settings: GatewayConfig,
    pub server_tls: Arc<ServerConfig>,
    pub client_tls: Arc<ClientConfig>,
    pub upstream_host: String,
    pub upstream_port: u16,
}

fn certificates(
    path: &str,
) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>, &'static str> {
    let data = fs::read(path).map_err(|_| "TLS certificate file unreadable")?;
    let certs = rustls_pemfile::certs(&mut BufReader::new(data.as_slice()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "invalid certificate PEM")?;
    if certs.is_empty() {
        return Err("empty certificate chain");
    }
    Ok(certs)
}

impl GatewayConfig {
    /// Load and validate all settings and TLS material before binding any socket.
    /// Errors deliberately omit file contents, URLs, and parser diagnostics.
    pub fn load_and_validate(path: &Path) -> Result<ValidatedConfig, &'static str> {
        let text = fs::read_to_string(path).map_err(|_| "configuration unreadable")?;
        let settings: Self = toml::from_str(&text).map_err(|_| "invalid configuration")?;
        settings.validate()?;
        let uri: hyper::Uri = settings
            .legacy_url
            .parse()
            .map_err(|_| "invalid upstream origin")?;
        let host = uri
            .host()
            .ok_or("missing upstream host")?
            .trim_matches(['[', ']'])
            .to_owned();
        rustls::pki_types::ServerName::try_from(host.clone())
            .map_err(|_| "invalid upstream TLS identity")?;
        let key_data =
            fs::read(&settings.public_key_file).map_err(|_| "TLS key file unreadable")?;
        let key = rustls_pemfile::private_key(&mut BufReader::new(key_data.as_slice()))
            .map_err(|_| "invalid TLS key")?
            .ok_or("missing TLS key")?;
        let server_tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates(&settings.public_cert_file)?, key)
            .map_err(|_| "invalid or mismatched public TLS material")?;
        let mut roots = RootCertStore::empty();
        for cert in certificates(&settings.upstream_ca_file)? {
            roots.add(cert).map_err(|_| "invalid upstream trust root")?;
        }
        let client_tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(ValidatedConfig {
            upstream_port: uri.port_u16().unwrap_or(443),
            settings,
            server_tls: Arc::new(server_tls),
            client_tls: Arc::new(client_tls),
            upstream_host: host,
        })
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.public_bind.port() != 8002
            || self.management_bind.port() == 0
            || self.management_bind.port() == 8002
        {
            return Err("invalid listener ports");
        }
        let ip = self.management_bind.ip();
        let private = match ip {
            std::net::IpAddr::V4(addr) => addr.is_private() || addr.is_loopback(),
            std::net::IpAddr::V6(addr) => addr.is_loopback() || addr.is_unique_local(),
        };
        if !private {
            return Err("management listener must be loopback or private");
        }
        let uri: hyper::Uri = self
            .legacy_url
            .parse()
            .map_err(|_| "invalid upstream origin")?;
        if uri.scheme_str() != Some("https")
            || uri.host().is_none()
            || uri
                .authority()
                .is_none_or(|authority| authority.as_str().contains('@'))
            || !matches!(
                uri.path_and_query().map(|path| path.as_str()),
                None | Some("/")
            )
            || self.legacy_url.contains('#')
            || uri.port_u16() == Some(0)
        {
            return Err("upstream must be an HTTPS origin");
        }
        for value in [
            self.connect_timeout_ms,
            self.header_timeout_ms,
            self.body_idle_timeout_ms,
            self.response_timeout_ms,
            self.shutdown_drain_ms,
        ] {
            if !(1..=300_000).contains(&value) {
                return Err("timeouts must be 1..300000 milliseconds");
            }
        }
        if !matches!(
            self.log_level.as_str(),
            "error" | "warn" | "info" | "debug" | "trace"
        ) {
            return Err("invalid log level");
        }
        Ok(())
    }
}

/// Convert validated millisecond settings to runtime deadlines.
pub fn milliseconds(value: u64) -> Duration {
    Duration::from_millis(value)
}
