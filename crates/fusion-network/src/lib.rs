use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose,
};
use sha2::{Digest, Sha256};
use tonic::transport::{Certificate as TonicCertificate, Identity};
use x509_parser::prelude::{FromDer, X509CertificationRequest};

pub struct MasterIdentity {
    pub ca_pem: String,
    pub server_cert_pem: String,
    pub server_key_pem: String,
}

pub fn default_data_dir() -> Result<PathBuf> {
    let base = if let Some(path) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(path)
    } else {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        PathBuf::from(home).join(".local/share")
    };
    Ok(base.join("fusion"))
}

pub fn master_identity(data_dir: &Path, advertise_host: &str) -> Result<MasterIdentity> {
    fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;
    let ca_path = data_dir.join("ca.pem");
    let ca_key_path = data_dir.join("ca-key.pem");

    let (ca, ca_key) = match (ca_path.exists(), ca_key_path.exists()) {
        (true, true) => {
            let ca_pem = fs::read_to_string(&ca_path)?;
            let key_pem = fs::read_to_string(&ca_key_path)?;
            let key = KeyPair::from_pem(&key_pem).context("loading the CA private key")?;
            validate_ca_key(&ca_pem, &key)?;
            let cert = CertificateParams::from_ca_cert_pem(&ca_pem)
                .context("loading the Fusion certificate authority")?
                .self_signed(&key)
                .context("reconstructing the Fusion certificate authority")?;
            (cert, key)
        }
        (false, false) => {
            let mut params = CertificateParams::default();
            set_validity(&mut params, 365 * 10);
            params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            let key = KeyPair::generate().context("generating the CA private key")?;
            let cert = params
                .self_signed(&key)
                .context("creating the local certificate authority")?;
            write_private_file(&ca_key_path, key.serialize_pem().as_bytes())?;
            fs::write(&ca_path, cert.pem())?;
            (cert, key)
        }
        _ => bail!(
            "incomplete certificate authority files in {}; restore both ca.pem and ca-key.pem",
            data_dir.display()
        ),
    };

    let server_key = KeyPair::generate().context("generating the Master TLS key")?;
    let mut server_params = CertificateParams::new(vec![advertise_host.to_owned()])
        .context("invalid advertised host for the TLS certificate")?;
    set_validity(&mut server_params, 365);
    server_params.is_ca = IsCa::NoCa;
    server_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server_params
        .signed_by(&server_key, &ca, &ca_key)
        .context("signing the Master TLS certificate")?;

    Ok(MasterIdentity {
        ca_pem: fs::read_to_string(ca_path)?,
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
    })
}

pub fn create_worker_csr(node_id: &str, node_name: &str) -> Result<(String, String)> {
    let key = KeyPair::generate().context("generating the Worker private key")?;
    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, node_id);
    subject.push(DnType::OrganizationName, node_name);
    let mut params = CertificateParams::default();
    set_validity(&mut params, 365);
    params.distinguished_name = subject;
    let csr = params
        .serialize_request(&key)
        .context("creating the Worker certificate request")?
        .pem()
        .context("encoding the Worker certificate request")?;
    Ok((csr, key.serialize_pem()))
}

pub fn sign_worker_csr(data_dir: &Path, node_id: &str, csr_pem: &str) -> Result<String> {
    let ca_pem =
        fs::read_to_string(data_dir.join("ca.pem")).context("reading the Fusion CA certificate")?;
    let ca_key_pem = fs::read_to_string(data_dir.join("ca-key.pem"))
        .context("reading the Fusion CA private key")?;
    let ca_params = CertificateParams::from_ca_cert_pem(&ca_pem)
        .context("parsing the Fusion CA certificate")?;
    let ca_key = KeyPair::from_pem(&ca_key_pem).context("parsing the Fusion CA private key")?;
    validate_ca_key(&ca_pem, &ca_key)?;
    let ca = ca_params
        .self_signed(&ca_key)
        .context("loading the Fusion CA")?;
    let mut request = CertificateSigningRequestParams::from_pem(csr_pem)
        .context("parsing the Worker certificate request")?;
    validate_worker_csr(csr_pem)?;

    let mut subject = DistinguishedName::new();
    subject.push(DnType::CommonName, node_id);
    request.params.distinguished_name = subject;
    set_validity(&mut request.params, 365);
    request.params.is_ca = IsCa::NoCa;
    request.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    request.params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];

    Ok(request
        .signed_by(&ca, &ca_key)
        .context("signing the Worker certificate")?
        .pem())
}

pub fn validate_worker_csr(csr_pem: &str) -> Result<()> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(csr_pem.as_bytes())
        .map_err(|error| anyhow::anyhow!("parsing Worker CSR PEM: {error}"))?;
    if pem.label != "CERTIFICATE REQUEST" {
        bail!("Worker CSR must use the CERTIFICATE REQUEST PEM label");
    }
    let (_, csr) = X509CertificationRequest::from_der(&pem.contents)
        .map_err(|error| anyhow::anyhow!("parsing Worker CSR: {error}"))?;
    csr.verify_signature()
        .context("verifying Worker CSR proof of private-key possession")
}

fn validate_ca_key(ca_pem: &str, ca_key: &KeyPair) -> Result<()> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes())
        .map_err(|error| anyhow::anyhow!("parsing Fusion CA PEM: {error}"))?;
    let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents)
        .map_err(|error| anyhow::anyhow!("parsing Fusion CA certificate: {error}"))?;
    if !certificate.is_ca() || certificate.public_key().raw != ca_key.public_key_der() {
        bail!("Fusion CA certificate and private key do not match");
    }
    Ok(())
}

fn set_validity(params: &mut CertificateParams, days: i64) {
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::minutes(5);
    params.not_after = now + time::Duration::days(days);
}

pub fn ca_fingerprint(ca_pem: &str) -> Result<String> {
    let (_, parsed) = x509_parser::pem::parse_x509_pem(ca_pem.as_bytes())
        .map_err(|error| anyhow::anyhow!("parsing the Fusion CA PEM: {error}"))?;
    Ok(hex::encode_upper(Sha256::digest(parsed.contents)))
}

pub fn verify_fingerprint(ca_pem: &str, expected: &str) -> Result<()> {
    let actual = ca_fingerprint(ca_pem)?;
    let expected = expected
        .chars()
        .filter(|character| *character != ':')
        .collect::<String>()
        .to_ascii_uppercase();
    if actual != expected {
        bail!("Master CA fingerprint mismatch (received {actual})");
    }
    Ok(())
}

pub fn tonic_ca_certificate(ca_pem: &str) -> TonicCertificate {
    TonicCertificate::from_pem(ca_pem.as_bytes())
}

pub fn tonic_identity(cert_pem: &str, key_pem: &str) -> Identity {
    Identity::from_pem(cert_pem.as_bytes(), key_pem.as_bytes())
}

pub fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating private key file {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approved_worker_certificate_is_limited_to_client_auth() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let identity = master_identity(directory.path(), "127.0.0.1")?;
        let node_id = "1a9e3fd7-39cf-4fc6-93c0-862326fa1a6c";
        let (csr, _) = create_worker_csr(node_id, "test-worker")?;
        let certificate_pem = sign_worker_csr(directory.path(), node_id, &csr)?;
        let (_, pem) = x509_parser::pem::parse_x509_pem(certificate_pem.as_bytes())
            .map_err(|error| anyhow::anyhow!("parsing signed certificate: {error}"))?;
        let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents)
            .map_err(|error| anyhow::anyhow!("parsing signed certificate DER: {error}"))?;

        assert!(!certificate.is_ca());
        assert!(
            certificate
                .extended_key_usage()?
                .expect("client certificate has an EKU")
                .value
                .client_auth
        );
        verify_fingerprint(&identity.ca_pem, &ca_fingerprint(&identity.ca_pem)?)?;
        assert!(verify_fingerprint(&identity.ca_pem, "00").is_err());
        Ok(())
    }

    #[test]
    fn master_reuses_the_local_ca_between_restarts() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let first = master_identity(directory.path(), "localhost")?;
        let second = master_identity(directory.path(), "localhost")?;

        assert_eq!(
            ca_fingerprint(&first.ca_pem)?,
            ca_fingerprint(&second.ca_pem)?
        );
        assert_ne!(first.server_cert_pem, second.server_cert_pem);
        Ok(())
    }
}
