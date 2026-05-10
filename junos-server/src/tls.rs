//! TLS material management.
//!
//! Resolution order:
//! 1. If both `--tls-cert` and `--tls-key` paths are given, load them.
//! 2. Else if `.certs/cert.pem` + `.certs/key.pem` exist, load them.
//! 3. Else generate a self-signed cert covering localhost + the host's
//!    first non-loopback IPv4, write it to `.certs/`, then load.
//!
//! The on-disk cache means that once the iPhone trusts the cert, restarts
//! don't invalidate that trust.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use axum_server::tls_rustls::RustlsConfig;
use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use tracing::{info, warn};

const CERT_DIR:  &str = ".certs";
const CERT_FILE: &str = "cert.pem";
const KEY_FILE:  &str = "key.pem";

pub async fn ensure_cert(cert: Option<&Path>, key: Option<&Path>) -> Result<RustlsConfig> {
    if let (Some(c), Some(k)) = (cert, key) {
        info!("TLS: using user-supplied cert {}", c.display());
        return RustlsConfig::from_pem_file(c, k)
            .await
            .context("loading user-supplied TLS cert/key");
    }
    if cert.is_some() || key.is_some() {
        return Err(anyhow!("--tls-cert and --tls-key must be supplied together"));
    }

    let dir = PathBuf::from(CERT_DIR);
    let cert_path = dir.join(CERT_FILE);
    let key_path  = dir.join(KEY_FILE);

    if cert_path.exists() && key_path.exists() {
        info!("TLS: reusing cached cert {}", cert_path.display());
        return RustlsConfig::from_pem_file(&cert_path, &key_path)
            .await
            .context("loading cached TLS cert/key");
    }

    let sans = collect_sans();
    info!("TLS: generating self-signed cert covering {:?}", sans);

    let mut params = CertificateParams::new(sans).context("rcgen params")?;
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, "junos-dev");
        dn
    };
    let key_pair = KeyPair::generate().context("rcgen keypair")?;
    let cert     = params.self_signed(&key_pair).context("rcgen self-sign")?;
    let cert_pem = cert.pem();
    let key_pem  = key_pair.serialize_pem();

    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    std::fs::write(&cert_path, &cert_pem)
        .with_context(|| format!("write {}", cert_path.display()))?;
    std::fs::write(&key_path, &key_pem)
        .with_context(|| format!("write {}", key_path.display()))?;
    set_key_perms(&key_path);

    info!("TLS: wrote {} and {} (1-year validity)",
          cert_path.display(), key_path.display());

    RustlsConfig::from_pem(cert_pem.into_bytes(), key_pem.into_bytes())
        .await
        .context("installing freshly generated TLS material")
}

fn collect_sans() -> Vec<String> {
    let mut sans = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    match if_addrs::get_if_addrs() {
        Ok(ifs) => {
            for i in ifs {
                if i.is_loopback() { continue; }
                if let std::net::IpAddr::V4(v4) = i.ip() {
                    sans.push(v4.to_string());
                }
            }
        }
        Err(e) => warn!("if_addrs failed, cert will only cover localhost: {e}"),
    }
    sans.sort();
    sans.dedup();
    sans
}

#[cfg(unix)]
fn set_key_perms(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        perms.set_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
}

#[cfg(not(unix))]
fn set_key_perms(_path: &Path) {}
