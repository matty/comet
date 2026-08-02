mod permissions;
pub use permissions::AtomicWriteError;

use std::path::Path;
use std::sync::Arc;

use comet_proto::ServerId;
use data_encoding::{BASE64, HEXLOWER};
use rcgen::{CertificateParams, KeyPair, PKCS_ED25519, PublicKeyData};
use rustls_pki_types::PrivatePkcs8KeyDer;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

const IDENTITY_FILE: &str = "device-identity.pem";

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("identity certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("identity PEM is missing the {0} block")]
    MissingPemBlock(&'static str),
    #[error("identity PEM contains invalid base64: {0}")]
    InvalidPem(#[from] data_encoding::DecodeError),
    #[error("identity certificate is invalid: {0}")]
    InvalidCertificate(String),
    #[error("identity certificate does not match its private key")]
    CertificateKeyMismatch,
    #[error("identity atomic write: {0}")]
    AtomicWrite(#[from] AtomicWriteError),
}

pub struct DeviceIdentity {
    server_id: ServerId,
    certificate_der: Vec<u8>,
    private_key_der: Zeroizing<Vec<u8>>,
}

impl DeviceIdentity {
    pub fn load_or_create(data_dir: &Path) -> Result<Arc<Self>, IdentityError> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join(IDENTITY_FILE);
        match std::fs::read_to_string(&path) {
            Ok(pem) => Self::from_pem(&pem).map(Arc::new),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let key_pair = KeyPair::generate_for(&PKCS_ED25519)?;
                let certificate = CertificateParams::new(vec!["comet.local".to_string()])?
                    .self_signed(&key_pair)?;
                let pem = format!("{}{}", certificate.pem(), key_pair.serialize_pem());
                let created = Self::from_parts(
                    certificate.der().to_vec(),
                    key_pair.serialize_der(),
                    key_pair.subject_public_key_info(),
                )?;
                if permissions::write_private_atomic_new(&path, pem.as_bytes())? {
                    Ok(Arc::new(created))
                } else {
                    Self::from_pem(&std::fs::read_to_string(path)?).map(Arc::new)
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn from_pem(pem: &str) -> Result<Self, IdentityError> {
        let certificate_der = pem_block(pem, "CERTIFICATE")?;
        let private_key_der = pem_block(pem, "PRIVATE KEY")?;
        let key_der = PrivatePkcs8KeyDer::from(private_key_der.as_slice());
        let key_pair = KeyPair::from_pkcs8_der_and_sign_algo(&key_der, &PKCS_ED25519)?;
        let (_, certificate) = x509_parser::parse_x509_certificate(&certificate_der)
            .map_err(|error| IdentityError::InvalidCertificate(error.to_string()))?;
        let certificate_public_key = certificate.public_key().raw.to_vec();
        let private_public_key = key_pair.subject_public_key_info();
        if certificate_public_key != private_public_key {
            return Err(IdentityError::CertificateKeyMismatch);
        }
        Self::from_parts(certificate_der, private_key_der, certificate_public_key)
    }

    fn from_parts(
        certificate_der: Vec<u8>,
        private_key_der: Vec<u8>,
        public_key_der: Vec<u8>,
    ) -> Result<Self, IdentityError> {
        let fingerprint = Sha256::digest(public_key_der);
        Ok(Self {
            server_id: ServerId::new(format!("sha256:{}", HEXLOWER.encode(&fingerprint))),
            certificate_der,
            private_key_der: Zeroizing::new(private_key_der),
        })
    }

    pub fn server_id(&self) -> &ServerId {
        &self.server_id
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    pub fn private_key_der(&self) -> &[u8] {
        &self.private_key_der
    }

    #[cfg(test)]
    fn private_key_base64_for_test(&self) -> String {
        BASE64.encode(&self.private_key_der)
    }
}

pub fn write_private_file_atomic(path: &Path, contents: &[u8]) -> Result<(), AtomicWriteError> {
    permissions::write_private_atomic(path, contents)
}

fn pem_block(pem: &str, label: &'static str) -> Result<Vec<u8>, IdentityError> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let encoded = pem
        .split_once(&begin)
        .and_then(|(_, rest)| rest.split_once(&end).map(|(body, _)| body))
        .ok_or(IdentityError::MissingPemBlock(label))?;
    let compact: String = encoded
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    Ok(BASE64.decode(compact.as_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_private_key_is_not_serialized_in_config() {
        let dir = tempfile::tempdir().unwrap();
        let first = DeviceIdentity::load_or_create(dir.path()).unwrap();
        let second = DeviceIdentity::load_or_create(dir.path()).unwrap();
        assert_eq!(first.server_id(), second.server_id());
        let config =
            std::fs::read_to_string(dir.path().join("remote-access.json")).unwrap_or_default();
        assert!(!config.contains(&first.private_key_base64_for_test()));
        assert_eq!(first.certificate_der(), second.certificate_der());
        assert_eq!(first.private_key_der(), second.private_key_der());
    }

    #[test]
    fn identity_rejects_a_certificate_that_does_not_match_its_private_key() {
        let first_dir = tempfile::tempdir().unwrap();
        let second_dir = tempfile::tempdir().unwrap();
        DeviceIdentity::load_or_create(first_dir.path()).unwrap();
        DeviceIdentity::load_or_create(second_dir.path()).unwrap();
        let first_path = first_dir.path().join("device-identity.pem");
        let first = std::fs::read_to_string(&first_path).unwrap();
        let second =
            std::fs::read_to_string(second_dir.path().join("device-identity.pem")).unwrap();
        let first_certificate = pem_text_block(&first, "CERTIFICATE");
        let second_certificate = pem_text_block(&second, "CERTIFICATE");
        std::fs::write(
            &first_path,
            first.replace(first_certificate, second_certificate),
        )
        .unwrap();

        assert!(DeviceIdentity::load_or_create(first_dir.path()).is_err());
    }

    #[test]
    fn concurrent_first_loads_converge_on_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let path = dir.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    DeviceIdentity::load_or_create(&path)
                })
            })
            .collect();
        let identities: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();

        assert!(
            identities
                .iter()
                .all(|identity| identity.server_id() == identities[0].server_id())
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_file_is_owner_read_write_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        DeviceIdentity::load_or_create(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path().join("device-identity.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn identity_file_has_a_protected_private_acl() {
        let dir = tempfile::tempdir().unwrap();
        DeviceIdentity::load_or_create(dir.path()).unwrap();
        assert!(
            permissions::has_protected_dacl_for_test(&dir.path().join("device-identity.pem"))
                .unwrap()
        );
    }

    fn pem_text_block<'a>(pem: &'a str, label: &str) -> &'a str {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let start = pem.find(&begin).unwrap();
        let end = pem[start..].find(&end).unwrap() + start + end.len();
        &pem[start..end]
    }
}
