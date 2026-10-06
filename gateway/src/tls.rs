use std::{
    fs::File,
    io::{self, BufReader},
    path::Path,
};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    ClientConfig, RootCertStore, ServerConfig,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("TLS file error: {0}")]
    Io(#[from] io::Error),
    #[error("TLS PEM contains no usable {0}")]
    Missing(&'static str),
    #[error("invalid TLS material: {0}")]
    Rustls(#[from] rustls::Error),
}

pub fn server_config(certificate_path: &Path, key_path: &Path) -> Result<ServerConfig, TlsError> {
    let certificates = certificates(certificate_path)?;
    let key = private_key(key_path)?;
    Ok(ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)?)
}
pub fn client_config(ca_path: &Path) -> Result<ClientConfig, TlsError> {
    let mut roots = RootCertStore::empty();
    for certificate in certificates(ca_path)? {
        roots.add(certificate)?;
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}
fn certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut reader = BufReader::new(File::open(path)?);
    let certificates = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
    if certificates.is_empty() {
        Err(TlsError::Missing("certificate"))
    } else {
        Ok(certificates)
    }
}
fn private_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut reader = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?.ok_or(TlsError::Missing("private key"))
}
