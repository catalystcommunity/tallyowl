//! Which authorities this process trusts.
//!
//! Mutual TLS asks this on every handshake, so a new authority beside the old
//! one is trusted by the next connection with no restart. That is how the
//! installation authority rotates: add the new one to
//! `installation.authorities`, move every node to certificates the new one
//! signed, then remove the old one.
//!
//! Files are the first source. D62 keeps the question behind [`TrustSource`] so
//! that LinkKeys can be a second one, with no change to anything that asks it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::RwLock;

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

use crate::material::{read_certificates, Reload, Reloadable};

/// Which authorities this process trusts.
pub trait TrustSource: Send + Sync {
    /// Every trusted authority certificate, in DER form. Empty means nothing is
    /// trusted, and every mutual TLS handshake is refused.
    fn authorities(&self) -> Vec<Vec<u8>>;
}

/// A fixed list of authorities. For a test, and for the old entry points that
/// took one authority.
pub struct StaticTrust(pub Vec<Vec<u8>>);

impl TrustSource for StaticTrust {
    fn authorities(&self) -> Vec<Vec<u8>> {
        self.0.clone()
    }
}

struct Held {
    hash: u64,
    authorities: Vec<Vec<u8>>,
}

/// The authorities in `installation.authorities`: PEM files, each holding one
/// or more certificates.
pub struct FileTrust {
    files: Vec<PathBuf>,
    held: RwLock<Held>,
}

fn read_files(files: &[PathBuf]) -> Result<Held, String> {
    if files.is_empty() {
        return Err(
            "No trusted authority is configured. Set `installation.authorities` to the PEM files of the authorities this installation trusts."
                .to_string(),
        );
    }
    let mut hasher = DefaultHasher::new();
    let mut authorities = Vec::new();
    for file in files {
        let bytes = std::fs::read(file)
            .map_err(|e| format!("{} could not be read: {e}", file.display()))?;
        file.hash(&mut hasher);
        bytes.hash(&mut hasher);
        for certificate in read_certificates(&bytes, file)? {
            authorities.push(certificate.to_vec());
        }
    }
    Ok(Held {
        hash: hasher.finish(),
        authorities,
    })
}

impl FileTrust {
    /// Read every file. Refused when a file cannot be read or holds no
    /// certificate.
    pub fn load(files: &[PathBuf]) -> Result<FileTrust, TallyOwlError> {
        let held = read_files(files)
            .map_err(|message| TallyOwlError::new(ErrorCode::FailedPrecondition, message))?;
        Ok(FileTrust {
            files: files.to_vec(),
            held: RwLock::new(held),
        })
    }
}

impl TrustSource for FileTrust {
    fn authorities(&self) -> Vec<Vec<u8>> {
        self.held
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .authorities
            .clone()
    }
}

impl Reloadable for FileTrust {
    /// Read the files again. A file that cannot be read keeps the set in use.
    fn reload(&self, _now_ms: i64) -> Reload {
        match read_files(&self.files) {
            Err(reason) => Reload::Kept(reason),
            Ok(fresh) => {
                let mut held = self.held.write().unwrap_or_else(|e| e.into_inner());
                if fresh.hash == held.hash {
                    Reload::Unchanged
                } else {
                    *held = fresh;
                    Reload::Replaced
                }
            }
        }
    }
}
