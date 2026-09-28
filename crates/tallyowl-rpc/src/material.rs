//! The certificates a process presents, and how they change while it runs.
//!
//! D62 has two kinds:
//!
//! - **Operator files** for a listener that applications reach: collector
//!   intake and the OpenTelemetry receiver. [`CertificateSet`] holds them.
//! - **An enrolled node identity** for mutual TLS between services. An
//!   [`IdentitySource`] gives the current one, and enrollment replaces it on
//!   renewal.
//!
//! Neither needs a restart to change. A TLS configuration here asks for the
//! current certificate on every handshake, so a rotated file or a renewed
//! identity is used by the next connection.
//!
//! # Several pairs at once
//!
//! An operator rotates a certificate before it expires by adding the new pair
//! beside the old one. The set presents the pair that is valid now and that has
//! the latest start time, so applications that trust the issuer see no gap.
//! The gauge reports the time until the **last** loaded certificate expires,
//! because that is when the listener has nothing left to show.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tallyowl_obs::error::{ErrorCode, TallyOwlError};

/// The certificate chain file in each directory: PEM, leaf first.
pub const CERTIFICATE_FILE: &str = "tls.crt";
/// The private key file in each directory: PEM.
pub const KEY_FILE: &str = "tls.key";

/// What one reload did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reload {
    /// Nothing changed.
    Unchanged,
    /// The files changed, or a different pair became the one to present, and
    /// the new set is in use.
    Replaced,
    /// The files changed and could not be used. The set in use is unchanged,
    /// and the text says why.
    Kept(String),
}

/// Something that reads its files again.
pub trait Reloadable: Send + Sync {
    fn reload(&self, now_ms: i64) -> Reload;
}

fn refused(message: String) -> TallyOwlError {
    TallyOwlError::new(ErrorCode::FailedPrecondition, message)
}

/// Read every certificate in a PEM file.
pub(crate) fn read_certificates(
    bytes: &[u8],
    source: &Path,
) -> Result<Vec<CertificateDer<'static>>, String> {
    let certificates: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &bytes[..])
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{} could not be read as PEM: {e}", source.display()))?;
    if certificates.is_empty() {
        return Err(format!("{} holds no certificate.", source.display()));
    }
    Ok(certificates)
}

/// The start and end of a certificate's validity, in milliseconds.
pub(crate) fn validity_ms(certificate: &[u8]) -> Option<(i64, i64)> {
    use x509_parser::prelude::*;
    let (_, parsed) = X509Certificate::from_der(certificate).ok()?;
    let validity = parsed.validity();
    Some((
        validity.not_before.timestamp().saturating_mul(1000),
        validity.not_after.timestamp().saturating_mul(1000),
    ))
}

/// One certificate and key pair, ready to present.
struct Pair {
    directory: PathBuf,
    not_before_ms: i64,
    not_after_ms: i64,
    key: Arc<CertifiedKey>,
}

fn read_pair(directory: &Path, hasher: &mut DefaultHasher) -> Result<Pair, String> {
    let certificate_path = directory.join(CERTIFICATE_FILE);
    let key_path = directory.join(KEY_FILE);
    let certificate_bytes = std::fs::read(&certificate_path)
        .map_err(|e| format!("{} could not be read: {e}", certificate_path.display()))?;
    let key_bytes = std::fs::read(&key_path)
        .map_err(|e| format!("{} could not be read: {e}", key_path.display()))?;
    directory.hash(hasher);
    certificate_bytes.hash(hasher);
    key_bytes.hash(hasher);

    let chain = read_certificates(&certificate_bytes, &certificate_path)?;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut &key_bytes[..])
        .map_err(|e| format!("{} could not be read as PEM: {e}", key_path.display()))?
        .ok_or_else(|| format!("{} holds no private key.", key_path.display()))?;
    let (not_before_ms, not_after_ms) = validity_ms(&chain[0]).ok_or_else(|| {
        format!(
            "The first certificate in {} could not be read.",
            certificate_path.display()
        )
    })?;
    let signing = rustls::crypto::ring::sign::any_supported_type(&key).map_err(|e| {
        format!(
            "{} is not a key TallyOwl can sign with: {e}",
            key_path.display()
        )
    })?;
    let certified = CertifiedKey::new(chain, signing);
    certified.keys_match().map_err(|_| {
        format!(
            "{} is not the key for the certificate in {}. Put the matching pair in the directory.",
            key_path.display(),
            certificate_path.display()
        )
    })?;
    Ok(Pair {
        directory: directory.to_path_buf(),
        not_before_ms,
        not_after_ms,
        key: Arc::new(certified),
    })
}

/// Everything read from the directories at one time.
struct Loaded {
    hash: u64,
    pairs: Vec<Pair>,
    /// The index of the pair presented now.
    current: usize,
}

fn read_all(directories: &[PathBuf]) -> Result<(u64, Vec<Pair>), String> {
    if directories.is_empty() {
        return Err(
            "No certificate directory is configured. Set `tls.certificateDirectories` to one or more directories that each hold tls.crt and tls.key."
                .to_string(),
        );
    }
    let mut hasher = DefaultHasher::new();
    let mut pairs = Vec::with_capacity(directories.len());
    for directory in directories {
        pairs.push(read_pair(directory, &mut hasher)?);
    }
    Ok((hasher.finish(), pairs))
}

/// The pair valid at `now_ms` with the latest start, if any.
fn choose(pairs: &[Pair], now_ms: i64) -> Option<usize> {
    pairs
        .iter()
        .enumerate()
        .filter(|(_, pair)| pair.not_before_ms <= now_ms && now_ms < pair.not_after_ms)
        .max_by_key(|(_, pair)| pair.not_before_ms)
        .map(|(index, _)| index)
}

fn none_valid(pairs: &[Pair], now_ms: i64) -> String {
    let each: Vec<String> = pairs
        .iter()
        .map(|pair| {
            let state = if now_ms < pair.not_before_ms {
                "is not valid yet"
            } else {
                "has expired"
            };
            format!("{} {state}", pair.directory.display())
        })
        .collect();
    format!(
        "No certificate is valid now: {}. Put a valid certificate and key in one of the directories.",
        each.join("; ")
    )
}

/// The server certificates of a listener that applications reach.
pub struct CertificateSet {
    directories: Vec<PathBuf>,
    loaded: RwLock<Loaded>,
}

impl CertificateSet {
    /// Read every directory. Each holds `tls.crt` (PEM chain, leaf first) and
    /// `tls.key` (PEM). Refused when a directory cannot be read or no pair is
    /// valid at `now_ms`.
    pub fn load(directories: &[PathBuf], now_ms: i64) -> Result<CertificateSet, TallyOwlError> {
        let (hash, pairs) = read_all(directories).map_err(refused)?;
        let current = choose(&pairs, now_ms).ok_or_else(|| refused(none_valid(&pairs, now_ms)))?;
        Ok(CertificateSet {
            directories: directories.to_vec(),
            loaded: RwLock::new(Loaded {
                hash,
                pairs,
                current,
            }),
        })
    }

    /// Read the directories again. The set changes only when the new files
    /// parse and hold a pair valid now. A pair that became valid, or stopped
    /// being valid, since the last reload is chosen again even when no file
    /// changed.
    pub fn reload(&self, now_ms: i64) -> Reload {
        let fresh = read_all(&self.directories);
        let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
        match fresh {
            Err(reason) => {
                if Some(loaded.current) == choose(&loaded.pairs, now_ms) {
                    Reload::Kept(reason)
                } else {
                    // The files are broken and the held pair expired: keep
                    // presenting what there is, and say both things.
                    Reload::Kept(format!(
                        "{reason} The certificate in use is no longer the right one to present."
                    ))
                }
            }
            Ok((hash, pairs)) if hash == loaded.hash => {
                drop(pairs);
                match choose(&loaded.pairs, now_ms) {
                    Some(index) if index != loaded.current => {
                        loaded.current = index;
                        Reload::Replaced
                    }
                    Some(_) => Reload::Unchanged,
                    None => Reload::Kept(none_valid(&loaded.pairs, now_ms)),
                }
            }
            Ok((hash, pairs)) => match choose(&pairs, now_ms) {
                Some(current) => {
                    *loaded = Loaded {
                        hash,
                        pairs,
                        current,
                    };
                    Reload::Replaced
                }
                None => Reload::Kept(none_valid(&pairs, now_ms)),
            },
        }
    }

    /// Seconds until the last loaded certificate expires. Negative once all of
    /// them have.
    pub fn seconds_left(&self, now_ms: i64) -> i64 {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        let last = loaded
            .pairs
            .iter()
            .map(|pair| pair.not_after_ms)
            .max()
            .unwrap_or(now_ms);
        (last - now_ms).div_euclid(1000)
    }

    /// The pair presented now.
    pub fn current(&self) -> Arc<CertifiedKey> {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&loaded.pairs[loaded.current].key)
    }

    /// The directory of the pair presented now, for a log line.
    pub fn current_directory(&self) -> PathBuf {
        let loaded = self.loaded.read().unwrap_or_else(|e| e.into_inner());
        loaded.pairs[loaded.current].directory.clone()
    }
}

impl Reloadable for CertificateSet {
    fn reload(&self, now_ms: i64) -> Reload {
        CertificateSet::reload(self, now_ms)
    }
}

impl std::fmt::Debug for CertificateSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertificateSet")
            .field("directories", &self.directories)
            .finish()
    }
}

/// Presents the current pair of a [`CertificateSet`] on every handshake.
#[derive(Debug)]
pub(crate) struct SetResolver(pub(crate) Arc<CertificateSet>);

impl ResolvesServerCert for SetResolver {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.current())
    }
}

/// The node's own enrolled identity. Enrollment replaces it in place on
/// renewal, and the next handshake presents the new one.
pub trait IdentitySource: Send + Sync {
    /// `None` before the node has enrolled. A connection that needs an
    /// identity is refused until then.
    fn current(&self) -> Option<crate::tls::Identity>;
}

/// An identity that never changes. For a test, and for the old entry points
/// that took one identity.
pub struct StaticIdentity(pub crate::tls::Identity);

impl IdentitySource for StaticIdentity {
    fn current(&self) -> Option<crate::tls::Identity> {
        Some(self.0.clone())
    }
}

/// An identity that a holder can replace. Enrollment can use this directly.
#[derive(Default)]
pub struct SwappableIdentity(RwLock<Option<crate::tls::Identity>>);

impl SwappableIdentity {
    pub fn new(identity: Option<crate::tls::Identity>) -> SwappableIdentity {
        SwappableIdentity(RwLock::new(identity))
    }

    /// Replace the identity. The next handshake presents the new one.
    pub fn replace(&self, identity: crate::tls::Identity) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(identity);
    }
}

impl IdentitySource for SwappableIdentity {
    fn current(&self) -> Option<crate::tls::Identity> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// What a reload told the host: which item, and what happened.
pub type ReloadReport = Arc<dyn Fn(usize, &Reload) + Send + Sync>;

/// Reloads several items on an interval.
///
/// [`ReloadDriver::tick`] is the whole of the logic and takes the time, so a
/// test moves the clock rather than waiting. [`ReloadDriver::spawn`] runs it on
/// a thread with the real clock.
pub struct ReloadDriver {
    items: Vec<Arc<dyn Reloadable>>,
    interval_ms: i64,
    next_due_ms: Mutex<Option<i64>>,
}

impl ReloadDriver {
    pub fn new(items: Vec<Arc<dyn Reloadable>>, interval: Duration) -> ReloadDriver {
        ReloadDriver {
            items,
            interval_ms: i64::try_from(interval.as_millis())
                .unwrap_or(i64::MAX)
                .max(1),
            next_due_ms: Mutex::new(None),
        }
    }

    /// Reload every item when the interval has passed. The first call only
    /// starts the interval, because the items were read when they were built.
    /// Returns what each item did, or nothing when it was not due.
    pub fn tick(&self, now_ms: i64) -> Option<Vec<Reload>> {
        let mut next = self.next_due_ms.lock().unwrap_or_else(|e| e.into_inner());
        match *next {
            None => {
                *next = Some(now_ms.saturating_add(self.interval_ms));
                None
            }
            Some(due) if now_ms < due => None,
            Some(_) => {
                *next = Some(now_ms.saturating_add(self.interval_ms));
                drop(next);
                Some(self.items.iter().map(|item| item.reload(now_ms)).collect())
            }
        }
    }

    /// Run [`ReloadDriver::tick`] on a thread until `stop` is set. `report`
    /// hears every result that is not `Unchanged`.
    pub fn spawn(
        self,
        stop: Arc<AtomicBool>,
        report: ReloadReport,
    ) -> std::io::Result<std::thread::JoinHandle<()>> {
        let pause = Duration::from_millis(
            u64::try_from(self.interval_ms / 4)
                .unwrap_or(1)
                .clamp(10, 1000),
        );
        std::thread::Builder::new()
            .name("tallyowl-tls-reload".into())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Some(results) = self.tick(now_ms()) {
                        for (index, result) in results.iter().enumerate() {
                            if *result != Reload::Unchanged {
                                report(index, result);
                            }
                        }
                    }
                    std::thread::sleep(pause);
                }
            })
    }
}

/// The wall clock in milliseconds since 1970.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
