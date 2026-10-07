use crate::{
    config::ConfigError,
    proxy::{Body, LegacyUpstream, ProxyError},
};
use hyper::{Request, Response, body::Incoming};

// No application ownership units are registered in this hosting bootstrap.
const REGISTERED_FAMILIES: &[&str] = &[];

pub fn validate_activation(value: &str) -> Result<(), ConfigError> {
    for family in value.split(',').filter(|family| !family.is_empty()) {
        if !REGISTERED_FAMILIES.contains(&family) {
            return Err("unknown active ownership family".into());
        }
    }
    if !value.is_empty() {
        return Err("bootstrap has no activatable handlers".into());
    }
    Ok(())
}

/// All paths and methods, including unknown application routes, belong to Harbour.
pub async fn dispatch(
    request: Request<Incoming>,
    legacy: &LegacyUpstream,
) -> Result<Response<Body>, ProxyError> {
    legacy.forward(request).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn merging_is_not_activation() {
        assert!(super::validate_activation("").is_ok());
        for value in ["cart", ",", " "] {
            assert!(super::validate_activation(value).is_err());
        }
    }
}
