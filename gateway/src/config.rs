//! File-only configuration and TLS material validation, with redacted errors.
use hyper::Uri;
use rustls::{ClientConfig, RootCertStore, ServerConfig, pki_types::CertificateDer};
use serde::Deserialize;
use std::{
    fs::File,
    io::BufReader,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// The single activation/configuration source. Unknown settings are errors.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub public_bind: SocketAddr,
    pub management_bind: SocketAddr,
    pub legacy_upstream: String,
    pub public_certificate: PathBuf,
    pub public_key: PathBuf,
    pub upstream_trust: PathBuf,
    pub trusted_proxies: Vec<IpAddr>,
    pub enabled_families: Vec<String>,
    pub connect_ms: u64,
    pub header_ms: u64,
    pub body_idle_ms: u64,
    pub deadline_ms: u64,
    pub probe_ms: u64,
    pub drain_ms: u64,
    pub max_headers: usize,
    pub max_header_bytes: usize,
    pub max_connections: usize,
    pub max_body_bytes: u64,
}

impl Config {
    /// Read one explicit file. Never include its contents in an error.
    pub fn load(path: &Path) -> Result<Self, &'static str> {
        let text = std::fs::read_to_string(path).map_err(|_| "config_read")?;
        let config: Self = toml::from_str(&text).map_err(|_| "config_parse")?;
        config.validate()?;
        Ok(config)
    }

    /// Enforce private management, fixed HTTPS destination and bounded resources.
    pub fn validate(&self) -> Result<(), &'static str> {
        let uri: Uri = self.legacy_upstream.parse().map_err(|_| "upstream_uri")?;
        let authority = uri.authority().ok_or("upstream_uri")?.as_str();
        if uri.scheme_str() != Some("https")
            || authority.contains('@')
            || uri.path_and_query().map(|part| part.as_str()) != Some("/")
            || self.legacy_upstream.contains('#')
            || uri.port_u16() != Some(8002)
        {
            return Err("upstream_uri");
        }
        let private = match self.management_bind.ip() {
            IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
            IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
        };
        if !private
            || self.public_bind == self.management_bind
            || self.public_bind.port() == 0
            || self.management_bind.port() == 0
        {
            return Err("listener_config");
        }
        for value in [
            self.connect_ms,
            self.header_ms,
            self.body_idle_ms,
            self.deadline_ms,
            self.probe_ms,
            self.drain_ms,
        ] {
            if !(1..=300_000).contains(&value) {
                return Err("timeout_config");
            }
        }
        if !(1..=256).contains(&self.max_headers)
            || !(8192..=65536).contains(&self.max_header_bytes)
            || !(1..=1024).contains(&self.max_connections)
            || !(1..=64 * 1024 * 1024).contains(&self.max_body_bytes)
        {
            return Err("resource_config");
        }
        Ok(())
    }

    /// Public TLS has no fallback certificate or plaintext listener.
    pub fn server_tls(&self) -> Result<Arc<ServerConfig>, &'static str> {
        let certs = certificates(&self.public_certificate)?;
        let mut reader = BufReader::new(File::open(&self.public_key).map_err(|_| "key_read")?);
        let key = rustls_pemfile::private_key(&mut reader)
            .map_err(|_| "key_parse")?
            .ok_or("key_missing")?;
        let mut tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|_| "certificate_key_pair")?;
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(tls))
    }

    /// Only explicitly mounted trust is used; hostname verification is mandatory.
    pub fn client_tls(&self) -> Result<Arc<ClientConfig>, &'static str> {
        let mut roots = RootCertStore::empty();
        for certificate in certificates(&self.upstream_trust)? {
            roots.add(certificate).map_err(|_| "trust_parse")?;
        }
        let mut tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(tls))
    }
}

fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, &'static str> {
    let mut reader = BufReader::new(File::open(path).map_err(|_| "certificate_read")?);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|_| "certificate_parse")?;
    if certs.is_empty() {
        return Err("certificate_missing");
    }
    Ok(certs)
}

/// Milliseconds are bounded during validation.
pub fn duration(ms: u64) -> Duration {
    Duration::from_millis(ms)
}
