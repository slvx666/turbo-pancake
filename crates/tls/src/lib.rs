//! TLS of the app-server and its clients (spec §2, §8, §12, spike 5; ADR 0014).
//!
//! * Server: certificate chain + key (PEM) → rustls `ServerConfig`.
//! * Clients: certificates are **always** verified. Trust is either the public web PKI
//!   (Mozilla roots, for a server with a public certificate) or exactly one private CA
//!   given as a PEM file (a home server reached by IP address). There is no switch that
//!   disables verification.
//! * `generate`: a private CA + a server certificate for the given IP addresses / host
//!   names (rcgen); `issue_server` re-issues the server certificate from the same CA
//!   (rotation without touching the clients).

use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::crypto::aws_lc_rs;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

pub use rustls;

#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("no certificate in the PEM data")]
    NoCertificate,
    #[error("bad PEM data: {0}")]
    Pem(String),
    #[error("TLS configuration: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
}

pub type Result<T> = std::result::Result<T, TlsError>;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(aws_lc_rs::default_provider())
}

pub fn certificates(pem: &[u8]) -> Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| TlsError::Pem(e.to_string()))?;
    if certs.is_empty() {
        return Err(TlsError::NoCertificate);
    }
    Ok(certs)
}

/// Client trust: `ca_pem` = the server's private CA (only it is trusted), `None` = the
/// public web PKI.
pub fn client_config(ca_pem: Option<&[u8]>) -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    match ca_pem {
        Some(pem) => {
            for c in certificates(pem)? {
                roots.add(c)?;
            }
        }
        None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
    }
    let cfg = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Server: certificate chain (server first) and its private key, both PEM.
pub fn server_config(chain_pem: &[u8], key_pem: &[u8]) -> Result<Arc<ServerConfig>> {
    let chain = certificates(chain_pem)?;
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| TlsError::Pem(e.to_string()))?;
    let mut cfg = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

/// A private CA and a server certificate signed by it.
#[derive(Clone, Debug)]
pub struct Pki {
    /// Give this to clients (`--server-ca`). Not secret.
    pub ca_cert_pem: String,
    /// Keep offline: it can issue certificates every client trusts.
    pub ca_key_pem: String,
    /// Server certificate followed by the CA certificate (the chain the server sends).
    pub server_chain_pem: String,
    pub server_key_pem: String,
}

fn validity(params: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(days);
}

fn ca_params(days: i64) -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.distinguished_name
        .push(DnType::CommonName, "turbo-pancake private CA");
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    validity(&mut p, days);
    Ok(p)
}

/// New CA (10 years) and a server certificate (`server_days`) for `names` — IP addresses
/// (`203.0.113.7`) and/or DNS names; IP strings become IP-address SANs.
pub fn generate(names: &[String], server_days: i64) -> Result<Pki> {
    let ca_key = KeyPair::generate()?;
    let ca_cert = ca_params(3650)?.self_signed(&ca_key)?;
    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_key.serialize_pem();
    let (server_chain_pem, server_key_pem) =
        issue_server(&ca_cert_pem, &ca_key_pem, names, server_days)?;
    Ok(Pki {
        ca_cert_pem,
        ca_key_pem,
        server_chain_pem,
        server_key_pem,
    })
}

/// A new server certificate from an existing CA: `(chain_pem, key_pem)`.
pub fn issue_server(
    ca_cert_pem: &str,
    ca_key_pem: &str,
    names: &[String],
    days: i64,
) -> Result<(String, String)> {
    let ca_key = KeyPair::from_pem(ca_key_pem)?;
    let issuer = Issuer::from_ca_cert_pem(ca_cert_pem, ca_key)?;
    let mut p = CertificateParams::new(names.to_vec())?;
    p.distinguished_name.push(
        DnType::CommonName,
        names
            .first()
            .cloned()
            .unwrap_or_else(|| "turbo-pancake".into()),
    );
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    validity(&mut p, days);
    let key = KeyPair::generate()?;
    let cert = p.signed_by(&key, &issuer)?;
    Ok((format!("{}{ca_cert_pem}", cert.pem()), key.serialize_pem()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pki_builds_configs() {
        let pki = generate(&["127.0.0.1".into(), "localhost".into()], 30).unwrap();
        assert_eq!(
            certificates(pki.server_chain_pem.as_bytes()).unwrap().len(),
            2
        );
        server_config(
            pki.server_chain_pem.as_bytes(),
            pki.server_key_pem.as_bytes(),
        )
        .unwrap();
        client_config(Some(pki.ca_cert_pem.as_bytes())).unwrap();
        client_config(None).unwrap();
        // Rotation from the same CA.
        let (chain, key) =
            issue_server(&pki.ca_cert_pem, &pki.ca_key_pem, &["127.0.0.1".into()], 30).unwrap();
        server_config(chain.as_bytes(), key.as_bytes()).unwrap();
        assert!(matches!(
            client_config(Some(b"not a pem")),
            Err(TlsError::NoCertificate | TlsError::Pem(_))
        ));
    }
}
