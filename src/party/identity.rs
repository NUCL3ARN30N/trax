//! This device's party identity: a DTLS certificate kept across restarts, so the host
//! recognises a guest that reconnects and its permissions survive.

use super::protocol::PeerId;
use crate::paths;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use webrtc::peer_connection::RTCCertificate;
use webrtc::peer_connection::crypto::{self, SignatureScheme};

pub struct Identity {
    pub certificate: RTCCertificate,
    pub peer_id: PeerId,
}

fn file() -> std::path::PathBuf {
    let dir = paths::data_dir().join("party");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("identity.pem")
}

/// Peer id for a DER certificate: base32 of its SHA-256.
pub fn peer_id_for(der: &[u8]) -> PeerId {
    data_encoding::BASE32_NOPAD.encode(&Sha256::digest(der)).to_lowercase()
}

/// Load the identity, creating it on first use.
pub fn load() -> Result<Identity> {
    let provider = crypto::default_provider().map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = file();
    let certificate = match std::fs::read_to_string(&path)
        .ok()
        .and_then(|pem| RTCCertificate::from_pem(&pem, provider.crypto()).ok())
        .filter(|c| c.expires > std::time::SystemTime::now() + std::time::Duration::from_secs(86_400))
    {
        Some(c) => c,
        None => {
            let mut params = rcgen::CertificateParams::new(vec!["trax".to_string()])?;
            // A long life, so the identity (and the host's grants) stay valid.
            params.not_after = rcgen::date_time_ymd(2045, 1, 1);
            let c = RTCCertificate::generate(provider.crypto(), SignatureScheme::EcdsaP256Sha256, params)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let pem = c.serialize_pem().map_err(|e| anyhow::anyhow!("{e}"))?;
            std::fs::write(&path, pem).context("can't save the party identity")?;
            c
        }
    };
    let der = certificate.dtls_certificate.certificate.first().context("certificate without data")?;
    let peer_id = peer_id_for(der.as_ref());
    Ok(Identity { certificate, peer_id })
}

/// A throw-away identity (tests run host and guest in one process).
#[cfg(test)]
pub fn ephemeral() -> Identity {
    let provider = crypto::default_provider().unwrap();
    let params = rcgen::CertificateParams::new(vec!["trax".to_string()]).unwrap();
    let certificate = RTCCertificate::generate(provider.crypto(), SignatureScheme::EcdsaP256Sha256, params).unwrap();
    let peer_id = peer_id_for(certificate.dtls_certificate.certificate[0].as_ref());
    Identity { certificate, peer_id }
}
