//! Native TLS termination + leaf-certificate fingerprint (Phase A2).
//!
//! The Chordia security model lets a self-hosted library serve its own (often self-signed) cert:
//! the library advertises the SHA-256 of its leaf certificate to the Hub directory, and peers /
//! native clients **pin** that fingerprint, so a self-signed cert is safe against MITM. (Browser
//! clients can't pin self-signed certs, so for the web UI you front the library with edge TLS via
//! a real-CA cert - Cloudflare Tunnel, Caddy, nginx - and leave the advertised fingerprint empty.)
//!
//! When `[tls]` is configured the server terminates HTTPS in-process and advertises the real
//! fingerprint; otherwise it serves plain HTTP and advertises an empty string ("validate normally"
//! - i.e. edge-terminated or local dev), never a placeholder.

use std::path::Path;

use anyhow::Context;
use axum_server::tls_rustls::RustlsConfig;
use sha2::{Digest, Sha256};

/// Install a process-wide rustls crypto provider (ring) once. Idempotent: a second call (or a
/// provider already installed by another crate) is a no-op. Must run before building any TLS
/// config.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// SHA-256 (lowercase hex) of the **leaf** certificate in a PEM bundle - the value advertised to
/// the Hub and pinned by peers. The leaf is the first certificate in the file (your server cert,
/// before any intermediates).
pub fn leaf_fingerprint_from_pem(cert_path: &Path) -> anyhow::Result<String> {
    let pem = std::fs::read(cert_path)
        .with_context(|| format!("reading TLS cert '{}'", cert_path.display()))?;
    let mut reader = std::io::BufReader::new(&pem[..]);
    let leaf = rustls_pemfile::certs(&mut reader)
        .next()
        .context("no certificate found in TLS cert file")?
        .context("parsing TLS certificate")?;
    Ok(hex::encode(Sha256::digest(&leaf)))
}

/// Load an axum-server rustls config from PEM cert + key files.
pub async fn rustls_config(cert_path: &Path, key_path: &Path) -> anyhow::Result<RustlsConfig> {
    RustlsConfig::from_pem_file(cert_path, key_path)
        .await
        .with_context(|| {
            format!(
                "loading TLS cert '{}' / key '{}'",
                cert_path.display(),
                key_path.display()
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generated self-signed cert yields a stable 64-char hex fingerprint that matches a direct
    /// SHA-256 of its DER, and the cert+key load into a rustls config.
    #[tokio::test]
    async fn fingerprint_and_config_from_generated_cert() {
        install_crypto_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let dir = std::env::temp_dir().join(format!("chordia-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

        let fp = leaf_fingerprint_from_pem(&cert_path).unwrap();
        assert_eq!(fp.len(), 64, "SHA-256 hex is 64 chars");
        assert_eq!(fp, hex::encode(Sha256::digest(cert.cert.der())));

        // The same cert+key must load into a usable rustls server config.
        rustls_config(&cert_path, &key_path).await.unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }
}
