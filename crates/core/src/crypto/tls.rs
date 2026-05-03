#[cfg(feature = "server")]
use {
    rustls::pki_types::CertificateDer, rustls::pki_types::PrivateKeyDer,
    rustls::pki_types::PrivatePkcs8KeyDer, std::path::Path, std::sync::Arc,
    tokio_rustls::TlsAcceptor,
};

/// Ensure the rustls crypto provider is installed (ring backend)
#[cfg(feature = "server")]
fn ensure_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ── HTTPS certificate (long-lived, 10 years) — server only ──

#[cfg(feature = "server")]
/// Generate a self-signed TLS certificate for HTTPS.
/// 10-year validity, persisted to disk so browser trust persists across restarts.
pub fn generate_self_signed_cert()
-> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, SanType};
    let mut params = CertificateParams::default();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "ShellAnyWhere");
    params
        .distinguished_name
        .push(DnType::OrganizationName, "ShellAnyWhere");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);

    // Subject Alternative Names — required by modern browsers (Chrome 58+)
    params
        .subject_alt_names
        .push(SanType::DnsName("localhost".try_into()?));
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V4(
            std::net::Ipv4Addr::new(127, 0, 0, 1),
        )));
    params
        .subject_alt_names
        .push(SanType::IpAddress(std::net::IpAddr::V6(
            std::net::Ipv6Addr::LOCALHOST,
        )));
    if let Ok(h) = hostname::get().and_then(|h| {
        h.into_string()
            .map_err(|_| std::io::Error::other("invalid"))
    }) && !h.is_empty()
        && let Ok(ia5) = h.as_str().try_into()
    {
        params.subject_alt_names.push(SanType::DnsName(ia5));
    }
    for ip in local_ip_addresses() {
        params.subject_alt_names.push(SanType::IpAddress(ip));
    }
    params.subject_alt_names.push(SanType::URI(
        "https://github.com/ejfkdev/ShellAnyWhere".try_into()?,
    ));
    // Valid for 10 years
    params.not_before = rcgen::date_time_ymd(2024, 1, 1);
    params.not_after = rcgen::date_time_ymd(2034, 12, 31);

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));

    Ok((vec![cert_der], key_der))
}

#[cfg(feature = "server")]
/// Load or generate a long-lived HTTPS certificate.
pub fn load_or_generate_cert(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    if cert_path.exists() && key_path.exists() {
        match load_cert_from_files(cert_path, key_path) {
            Ok(pair) => {
                log::info!("Loaded TLS certificate from {:?}", cert_path);
                return Ok(pair);
            }
            Err(e) => {
                log::warn!(
                    "Failed to load TLS certificate from {:?}: {}. Regenerating.",
                    cert_path,
                    e
                );
            }
        }
    }

    let (cert, key) = generate_self_signed_cert()?;

    if let Some(parent) = cert_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        log::warn!("Cannot create cert directory {:?}: {}", parent, e);
    }
    let cert_pem = pem_encode_cert_chain(&cert);
    if let Err(e) = std::fs::write(cert_path, &cert_pem) {
        log::warn!("Cannot save certificate to {:?}: {}", cert_path, e);
    } else {
        log::info!("Saved TLS certificate to {:?}", cert_path);
    }
    let key_pem = pem_encode_key(&key);
    if let Err(e) = std::fs::write(key_path, &key_pem) {
        log::warn!("Cannot save private key to {:?}: {}", key_path, e);
    } else {
        log::info!("Saved TLS private key to {:?}", key_path);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600));
        }
    }

    Ok((cert, key))
}

#[cfg(feature = "server")]
/// Enumerate local IP addresses from all network interfaces.
fn local_ip_addresses() -> Vec<std::net::IpAddr> {
    let mut ips = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0")
        && socket.connect("8.8.8.8:80").is_ok()
        && let Ok(local_addr) = socket.local_addr()
    {
        ips.push(local_addr.ip());
    }
    if let Ok(socket) = std::net::UdpSocket::bind("[::]:0")
        && socket.connect("[2001:4860:4860::8888]:80").is_ok()
        && let Ok(local_addr) = socket.local_addr()
    {
        ips.push(local_addr.ip());
    }
    ips
}

// ── PEM helpers — server only ──

#[cfg(feature = "server")]
fn load_cert_from_files(
    cert_path: &Path,
    key_path: &Path,
) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_pem = std::fs::read_to_string(cert_path)?;
    let key_pem = std::fs::read_to_string(key_path)?;
    let cert_chain = pem_parse_certificates(&cert_pem)?;
    let key_der = pem_parse_private_key(&key_pem)?;
    Ok((cert_chain, key_der))
}

#[cfg(feature = "server")]
fn pem_encode_cert(cert: &CertificateDer<'_>) -> String {
    let b64 = base64_encode(cert.as_ref());
    format!(
        "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
        format_pem_lines(&b64)
    )
}

#[cfg(feature = "server")]
fn pem_encode_cert_chain(chain: &[CertificateDer<'_>]) -> String {
    chain.iter().map(pem_encode_cert).collect()
}

#[cfg(feature = "server")]
fn pem_encode_key(key: &PrivateKeyDer<'_>) -> String {
    let der = match key {
        PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der(),
        PrivateKeyDer::Pkcs1(k) => k.secret_pkcs1_der(),
        PrivateKeyDer::Sec1(k) => k.secret_sec1_der(),
        _ => &[],
    };
    let b64 = base64_encode(der);
    format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        format_pem_lines(&b64)
    )
}

#[cfg(feature = "server")]
fn pem_parse_certificates(pem_str: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let certs: Vec<CertificateDer<'static>> = pem::parse_many(pem_str)?
        .into_iter()
        .filter(|p| p.tag() == "CERTIFICATE")
        .map(|p| CertificateDer::from(p.into_contents()))
        .collect();
    if certs.is_empty() {
        anyhow::bail!("No CERTIFICATE found in PEM")
    }
    Ok(certs)
}

#[cfg(feature = "server")]
fn pem_parse_private_key(pem_str: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    for pem_block in pem::parse_many(pem_str)? {
        match pem_block.tag() {
            "PRIVATE KEY" => {
                return Ok(PrivateKeyDer::from(PrivatePkcs8KeyDer::from(
                    pem_block.into_contents(),
                )));
            }
            "RSA PRIVATE KEY" => {
                return Ok(PrivateKeyDer::from(
                    rustls::pki_types::PrivatePkcs1KeyDer::from(pem_block.into_contents()),
                ));
            }
            "EC PRIVATE KEY" => {
                return Ok(PrivateKeyDer::from(
                    rustls::pki_types::PrivateSec1KeyDer::from(pem_block.into_contents()),
                ));
            }
            _ => continue,
        }
    }
    anyhow::bail!("No private key found in PEM")
}

#[cfg(feature = "server")]
fn base64_encode(data: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD};
    STANDARD.encode(data)
}

#[cfg(feature = "server")]
fn format_pem_lines(b64: &str) -> String {
    b64.as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── TLS acceptor — server only ──

#[cfg(feature = "server")]
/// Build a TLS acceptor (server-side) with the given cert and key.
/// Advertises HTTP/2 and HTTP/1.1 via ALPN for browser compatibility.
pub fn build_tls_acceptor(
    cert: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<TlsAcceptor> {
    ensure_crypto_provider();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert, key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "server")]
    #[test]
    fn test_generate_self_signed_cert() {
        let (cert, key) = generate_self_signed_cert().expect("cert generation failed");
        assert!(!cert.is_empty());
        for c in &cert {
            assert!(!c.as_ref().is_empty());
        }
        assert!(!key.secret_der().is_empty());
    }

    #[cfg(feature = "server")]
    #[test]
    fn test_build_tls_acceptor() {
        let (cert, key) = generate_self_signed_cert().unwrap();
        let acceptor = build_tls_acceptor(cert, key);
        assert!(acceptor.is_ok());
    }
}
