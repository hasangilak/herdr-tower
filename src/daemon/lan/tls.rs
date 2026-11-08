//! TLS for the phone API: a self-signed certificate made once on this Mac
//! (`~/.config/herder-tower/lan/`), which the phone pins by its SHA-256
//! fingerprint (it arrives with the pairing code, in the QR). No CA, no
//! hostname: the fingerprint is the identity, so a changed LAN address
//! doesn't matter and nobody in between can pass for the Mac.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct Identity {
    pub cert_der: Vec<u8>,
    key_der: Vec<u8>,
}

impl Identity {
    pub fn dir() -> PathBuf {
        crate::config::config_dir().join("lan")
    }

    /// The Mac's certificate, made on first use.
    pub fn load_or_create(dir: &Path) -> Result<Identity> {
        let (cert, key) = (dir.join("cert.der"), dir.join("key.der"));
        if let (Ok(cert_der), Ok(key_der)) = (std::fs::read(&cert), std::fs::read(&key)) {
            return Ok(Identity { cert_der, key_der });
        }
        let mut params = rcgen::CertificateParams::new(vec!["herder-tower".to_string()])?;
        params.distinguished_name.push(rcgen::DnType::CommonName, "herder-tower");
        params.not_before = rcgen::date_time_ymd(2024, 1, 1);
        params.not_after = rcgen::date_time_ymd(2099, 1, 1);
        let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let c = params.self_signed(&key_pair)?;
        let id = Identity { cert_der: c.der().to_vec(), key_der: key_pair.serialize_der() };
        write_private(&key, &id.key_der)?;
        write_private(&cert, &id.cert_der)?;
        Ok(id)
    }

    /// SHA-256 of the certificate (DER), hex: what the phone pins.
    pub fn fingerprint(&self) -> String {
        super::auth::sha256_hex(&self.cert_der)
    }

    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
            .with_no_client_auth()
            .with_single_cert(
                vec![CertificateDer::from(self.cert_der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone())),
            )
            .context("the tower's TLS certificate")?;
        Ok(Arc::new(cfg))
    }
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    std::io::Write::write_all(&mut f, data)?;
    Ok(())
}

/// The fingerprint grouped for reading aloud or comparing by eye: `ab12 cd34 …`.
pub fn grouped(fp: &str) -> String {
    fp.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_one_identity_and_keeps_it() {
        let dir = std::env::temp_dir().join(format!("tower-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Identity::load_or_create(&dir).unwrap();
        let b = Identity::load_or_create(&dir).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint(), "made once, then reused");
        assert_eq!(a.fingerprint().len(), 64);
        assert!(a.server_config().is_ok());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.join("key.der")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(grouped("abcdefgh12"), "abcd efgh 12");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
