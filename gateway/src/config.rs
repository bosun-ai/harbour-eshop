use crate::Error;
use hyper::Uri;
use std::{collections::HashSet, env, net::SocketAddr, time::Duration};

pub(crate) struct Config {
    pub public_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub cert: String,
    pub key: String,
    pub origin: Uri,
    pub ca: String,
    pub enabled: HashSet<String>,
    pub connect: Duration,
    pub upload: Duration,
    pub response: Duration,
    pub drain: Duration,
    pub log_level: tracing::Level,
}

pub(crate) fn setting(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

pub(crate) fn origin(value: &str) -> Result<Uri, Error> {
    let uri: Uri = value.parse()?;
    if uri.scheme_str() != Some("https")
        || uri.host().is_none()
        || uri
            .authority()
            .is_some_and(|value| value.as_str().contains('@'))
        || uri.path() != "/"
        || uri.query().is_some()
        || value.contains('#')
    {
        return Err(
            "LEGACY_ORIGIN must be an HTTPS origin without credentials or base path".into(),
        );
    }
    Ok(uri)
}

impl Config {
    pub(crate) fn load() -> Result<Self, Error> {
        let required = |name: &str| -> Result<String, Error> {
            let value = env::var(name).map_err(|_| format!("{name} is required"))?;
            if value.is_empty() {
                return Err(format!("{name} is empty").into());
            }
            Ok(value)
        };
        let seconds = |name: &str, default: &str| -> Result<Duration, Error> {
            let value: u64 = setting(name, default).parse()?;
            if !(1..=300).contains(&value) {
                return Err(format!("{name} must be 1..300 seconds").into());
            }
            Ok(Duration::from_secs(value))
        };
        let config = Self {
            public_bind: setting("PUBLIC_BIND", "0.0.0.0:8002").parse()?,
            admin_bind: setting("ADMIN_BIND", "127.0.0.1:8003").parse()?,
            cert: required("PUBLIC_CERT_FILE")?,
            key: required("PUBLIC_KEY_FILE")?,
            origin: origin(&required("LEGACY_ORIGIN")?)?,
            ca: required("LEGACY_CA_FILE")?,
            enabled: setting("ENABLED_ROUTE_FAMILIES", "")
                .split(',')
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .collect(),
            connect: seconds("CONNECT_TIMEOUT_SECS", "5")?,
            upload: seconds("UPLOAD_TIMEOUT_SECS", "30")?,
            response: seconds("RESPONSE_TIMEOUT_SECS", "60")?,
            drain: seconds("DRAIN_TIMEOUT_SECS", "10")?,
            log_level: setting("LOG_LEVEL", "info").parse()?,
        };
        if config.connect > config.upload
            || config.upload > config.response
            || config.public_bind == config.admin_bind
        {
            return Err("require connect <= upload <= response and distinct listeners".into());
        }
        Ok(config)
    }
}
