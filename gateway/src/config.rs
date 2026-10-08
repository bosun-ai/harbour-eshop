//! Environment configuration validated before either listener binds.
use crate::Error;
use hyper::Uri;
use std::{env, fs::File, io::BufReader, net::SocketAddr, sync::Arc, time::Duration};
use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig};

/// Validated listener, TLS, selection and deadline settings.
#[derive(Clone)]
pub struct Config {
    pub public_bind: SocketAddr,
    pub management_bind: SocketAddr,
    pub upstream: Uri,
    pub server_tls: Arc<ServerConfig>,
    pub client_tls: Arc<ClientConfig>,
    pub enabled_slices: Vec<String>,
    pub connect: Duration,
    pub header: Duration,
    pub request_body: Duration,
    pub upstream_response: Duration,
    pub body_idle: Duration,
    pub shutdown: Duration,
    pub log_level: String,
}

/// Reject anything other than a plain HTTPS origin, without normalizing paths.
pub fn origin(value: &str) -> Result<Uri, Error> {
    let uri: Uri = value.parse()?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri
            .authority()
            .is_none_or(|authority| authority.as_str().contains('@'))
        || uri.path() != "/"
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err("LEGACY_UPSTREAM must be an HTTPS origin".into());
    }
    Ok(uri)
}

fn required(name: &str) -> Result<String, Error> {
    env::var(name).map_err(|_| format!("missing {name}").into())
}

fn deadline(name: &str, default: u64) -> Result<Duration, Error> {
    let seconds = env::var(name)
        .unwrap_or(default.to_string())
        .parse::<u64>()?;
    if !(1..=300).contains(&seconds) {
        return Err(format!("{name} must be 1..300 seconds").into());
    }
    Ok(Duration::from_secs(seconds))
}

impl Config {
    /// Load required PEM material and verify the certificate/key pairing.
    pub fn from_env() -> Result<Self, Error> {
        let certs =
            rustls_pemfile::certs(&mut BufReader::new(File::open(required("PUBLIC_CERT")?)?))
                .collect::<Result<Vec<_>, _>>()?;
        let key =
            rustls_pemfile::private_key(&mut BufReader::new(File::open(required("PUBLIC_KEY")?)?))?
                .ok_or("PUBLIC_KEY has no private key")?;
        let mut server_tls = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)?;
        server_tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let mut roots = RootCertStore::empty();
        for cert in
            rustls_pemfile::certs(&mut BufReader::new(File::open(required("LEGACY_TRUST")?)?))
        {
            roots.add(cert?)?;
        }
        if roots.is_empty() {
            return Err("LEGACY_TRUST has no certificates".into());
        }
        let mut client_tls = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"http/1.1".to_vec()];
        let log_level = env::var("LOG_LEVEL").unwrap_or("info".into());
        if !["off", "error", "warn", "info", "debug", "trace"].contains(&log_level.as_str()) {
            return Err("invalid LOG_LEVEL".into());
        }
        Ok(Self {
            public_bind: env::var("PUBLIC_BIND")
                .unwrap_or("0.0.0.0:8002".into())
                .parse()?,
            management_bind: env::var("MANAGEMENT_BIND")
                .unwrap_or("127.0.0.1:9000".into())
                .parse()?,
            upstream: origin(&required("LEGACY_UPSTREAM")?)?,
            server_tls: Arc::new(server_tls),
            client_tls: Arc::new(client_tls),
            enabled_slices: env::var("ENABLED_SLICES")
                .unwrap_or_default()
                .split(',')
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .collect(),
            connect: deadline("CONNECT_SECONDS", 5)?,
            header: deadline("HEADER_SECONDS", 30)?,
            request_body: deadline("REQUEST_BODY_SECONDS", 120)?,
            upstream_response: deadline("UPSTREAM_RESPONSE_SECONDS", 150)?,
            body_idle: deadline("BODY_IDLE_SECONDS", 30)?,
            shutdown: deadline("SHUTDOWN_SECONDS", 10)?,
            log_level,
        })
    }
}
