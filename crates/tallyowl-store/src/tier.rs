//! Hot, warm, and cold, and the rule that keeps a tier move from losing data.
//!
//! `docs/STORAGE.md` section 13 and D24. All tiers use the same immutable
//! segment format:
//!
//! - **hot:** the append log, the open buffer, and the most recent window;
//! - **warm:** sealed local segments;
//! - **cold:** older sealed segments in object storage, with local manifest and
//!   routing metadata.
//!
//! # The rule that matters
//!
//! **TallyOwl evicts a local segment only after durable cold storage contains
//! its object and TallyOwl has verified it.** An interrupted upload must never
//! evict the only valid copy, which is a Phase 3 exit criterion and the reason
//! this module verifies by content address rather than by trusting a write to
//! have returned.
//!
//! # Object storage is optional
//!
//! D24: the home profile does not use it, and `home-limits` in DEPLOYMENT.md
//! section 3 says a home installation has no cold object store at all. Nothing
//! here runs unless an operator turns it on.
//!
//! # What a cold store is
//!
//! [`ColdStore`] is the boundary. It is deliberately small — put, get, delete,
//! exists — because everything above it is TallyOwl's and everything below it is
//! somebody's bucket. [`FilesystemColdStore`] is a real implementation over a
//! second directory, which is what an operator with a network volume has;
//! bucket backends sit behind the same trait.

use std::path::{Path, PathBuf};

use crate::catalog::Manifest;
use crate::segment::format::content_address;
use crate::store::StoreError;

/// D24's first policy, and every value is configurable for each data class.
#[derive(Debug, Clone, Copy)]
pub struct TierPolicy {
    /// Keep this much recent data on local storage.
    pub hot_ms: i64,
    /// Keep the next span of data on local storage as well.
    pub warm_ms: i64,
    /// Start normal tier movement at this fraction of disk use.
    pub start_at: f64,
    /// Increase tier movement at this fraction.
    pub increase_at: f64,
    /// Stop ingest before unsafe exhaustion at this fraction.
    pub stop_ingest_at: f64,
}

impl Default for TierPolicy {
    fn default() -> TierPolicy {
        TierPolicy {
            hot_ms: 24 * 3_600_000,
            warm_ms: 7 * 24 * 3_600_000,
            start_at: 0.70,
            increase_at: 0.85,
            stop_ingest_at: 0.95,
        }
    }
}

/// Which tier a segment belongs in, by age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Hot,
    Warm,
    Cold,
}

impl TierPolicy {
    /// Where a segment belongs, given the newest event it holds.
    pub fn tier_for(&self, newest_event: i64, now: i64) -> Tier {
        let age = now - newest_event;
        if age < self.hot_ms {
            Tier::Hot
        } else if age < self.hot_ms + self.warm_ms {
            Tier::Warm
        } else {
            Tier::Cold
        }
    }

    /// What disk pressure asks for.
    pub fn pressure(&self, used: f64) -> Pressure {
        if used >= self.stop_ingest_at {
            Pressure::StopIngest
        } else if used >= self.increase_at {
            Pressure::MoveFaster
        } else if used >= self.start_at {
            Pressure::Move
        } else {
            Pressure::Rest
        }
    }
}

/// What the disk is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Rest,
    Move,
    MoveFaster,
    /// Stop accepting durable intake. A collector that cannot retain data must
    /// not acknowledge it, which is DELIVERY.md section 8.
    StopIngest,
}

/// The object-storage boundary.
///
/// Small on purpose. Everything above it is TallyOwl's; everything below it is
/// somebody's bucket, and the abstraction stays replaceable.
pub trait ColdStore: Send + Sync {
    /// Put an object. The implementation must not report success before the
    /// bytes are durable in its own terms.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError>;
    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError>;
    fn exists(&self, key: &str) -> Result<bool, StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    /// A name for a person reading a health report.
    fn describe(&self) -> String;
}

/// A cold store over a second directory.
///
/// This is what an operator with a network volume has, and it is a real
/// implementation rather than a stand-in: it writes to a temporary name, calls
/// fsync, and renames, so a crash never leaves a partial object under a real
/// key.
pub struct FilesystemColdStore {
    root: PathBuf,
}

impl FilesystemColdStore {
    pub fn new(root: impl AsRef<Path>) -> Result<FilesystemColdStore, StoreError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| {
            StoreError::Unavailable(format!(
                "The cold storage directory {} could not be created: {e}",
                root.display()
            ))
        })?;
        Ok(FilesystemColdStore { root })
    }

    fn path(&self, key: &str) -> PathBuf {
        // A key never escapes the root, whatever it holds.
        self.root.join(key.replace(['/', '\\'], "_"))
    }
}

impl ColdStore for FilesystemColdStore {
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        use std::io::Write;

        let path = self.path(key);
        let temporary = path.with_extension("uploading");
        let mut file = std::fs::File::create(&temporary).map_err(|e| {
            StoreError::Unavailable(format!("Cold storage could not be written: {e}"))
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| {
                StoreError::Unavailable(format!("Cold storage could not be written: {e}"))
            })?;
        drop(file);
        std::fs::rename(&temporary, &path)
            .map_err(|e| StoreError::Unavailable(format!("Cold storage could not be written: {e}")))
    }

    fn get(&self, key: &str) -> Result<Vec<u8>, StoreError> {
        std::fs::read(self.path(key))
            .map_err(|e| StoreError::Unavailable(format!("Cold storage could not be read: {e}")))
    }

    fn exists(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self.path(key).is_file())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        match std::fs::remove_file(self.path(key)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Unavailable(format!(
                "Cold storage could not be changed: {e}"
            ))),
        }
    }

    fn describe(&self) -> String {
        format!("the directory {}", self.root.display())
    }
}

/// The object key one segment lives under.
///
/// A segment is identified by its content address, which is what makes an
/// obsolete upload unable to overwrite a live object. See FAILURE_MODES.md
/// section 8.3 rule 1.
pub fn object_key(manifest: &Manifest) -> String {
    format!(
        "{}/{}.tos",
        crate::row::hex(&manifest.project_id),
        crate::row::hex(&manifest.content_address)
    )
}

/// What one tier move did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TieringOutcome {
    pub uploaded: usize,
    pub verified: usize,
    /// Local files removed after the copy was verified.
    pub evicted: usize,
    /// Uploads that did not verify, so nothing local was removed.
    pub unverified: usize,
}

/// Copy one segment to cold storage and verify it.
///
/// **Verification reads the object back and checks its content address.** A put
/// that returned is not proof: an interrupted upload, a truncated write, or a
/// bucket that acknowledged early would all report success. The content address
/// is the only thing that says the bytes that arrived are the bytes that left.
///
/// This never removes the local file. Eviction is a separate step on purpose,
/// so that "the copy is there" and "the original can go" cannot be confused.
pub fn upload(
    cold: &dyn ColdStore,
    manifest: &Manifest,
    local_bytes: &[u8],
) -> Result<bool, StoreError> {
    let key = object_key(manifest);
    cold.put(&key, local_bytes)?;

    let round_trip = cold.get(&key)?;
    // The prologue holds the content address, so the comparison is over the
    // same region the writer addressed.
    if round_trip.len() < crate::segment::format::PROLOGUE_BYTES {
        return Ok(false);
    }
    let address = content_address(&round_trip[crate::segment::format::PROLOGUE_BYTES..]);
    Ok(address == manifest.content_address)
}

/// Remove the local copy, once and only once the cold copy is verified.
///
/// The verification runs again here rather than trusting a flag from earlier.
/// The interval between an upload and an eviction is exactly where a bucket can
/// lose an object, and this is the last moment anything can notice.
pub fn evict_local(
    cold: &dyn ColdStore,
    manifest: &Manifest,
    local_path: &Path,
) -> Result<bool, StoreError> {
    let key = object_key(manifest);

    if !cold.exists(&key)? {
        return Ok(false);
    }
    let round_trip = cold.get(&key)?;
    if round_trip.len() < crate::segment::format::PROLOGUE_BYTES {
        return Ok(false);
    }
    if content_address(&round_trip[crate::segment::format::PROLOGUE_BYTES..])
        != manifest.content_address
    {
        // The cold copy is not the segment. The local file is the only valid
        // copy and it stays.
        return Ok(false);
    }

    std::fs::remove_file(local_path)
        .map_err(|e| StoreError::Unavailable(format!("A local file could not be removed: {e}")))?;
    Ok(true)
}

/// A bounded local cache of cold objects.
///
/// STORAGE.md section 13: a cold query reads required index and column page
/// ranges and fills a bounded cache. The bound is the point. An unbounded cache
/// turns cold storage into a slow copy of local storage and fills the device it
/// was supposed to relieve.
///
/// **A cached copy is never the only copy.** That is what makes
/// [`ColdCache::evict_all`] safe, and it is the behaviour
/// FAILURE_MODES.md section 10 asks for when the device fills: throw the whole
/// cache away, because every byte of it can be fetched again.
pub struct ColdCache {
    directory: PathBuf,
    max_bytes: u64,
}

impl ColdCache {
    pub fn new(directory: impl AsRef<Path>, max_bytes: u64) -> Result<ColdCache, StoreError> {
        let directory = directory.as_ref().to_path_buf();
        std::fs::create_dir_all(&directory).map_err(|e| {
            StoreError::Unavailable(format!(
                "The cache directory {} could not be created: {e}",
                directory.display()
            ))
        })?;
        Ok(ColdCache {
            directory,
            max_bytes,
        })
    }

    /// A cold object, from the cache when it is there and from cold storage
    /// when it is not.
    ///
    /// A cache write that fails is not an error. The bytes are already in hand
    /// and the caller wanted the object, not the cache.
    pub fn read_through(&self, cold: &dyn ColdStore, key: &str) -> Result<Vec<u8>, StoreError> {
        if let Some(found) = self.get(key) {
            return Ok(found);
        }
        let bytes = cold.get(key)?;
        let _ = self.put(key, &bytes);
        Ok(bytes)
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let path = self.path_for(key);
        let bytes = std::fs::read(&path).ok()?;
        // Touch it, so the eviction order is by last use rather than by age.
        // A segment read every minute for a week should outlive one read once.
        let _ = std::fs::File::open(&path).and_then(|file| {
            file.set_times(
                std::fs::FileTimes::new()
                    .set_accessed(std::time::SystemTime::now())
                    .set_modified(std::time::SystemTime::now()),
            )
        });
        Some(bytes)
    }

    pub fn put(&self, key: &str, bytes: &[u8]) -> Result<(), StoreError> {
        // An object larger than the whole cache is not cached at all, rather
        // than emptying the cache to hold one thing.
        if bytes.len() as u64 > self.max_bytes {
            return Ok(());
        }
        self.make_room_for(bytes.len() as u64)?;
        std::fs::write(self.path_for(key), bytes).map_err(|e| {
            StoreError::Unavailable(format!("A cached copy could not be written: {e}"))
        })
    }

    /// Bytes the cache currently holds.
    pub fn bytes(&self) -> u64 {
        self.entries().iter().map(|(_, _, size)| *size).sum::<u64>()
    }

    /// Throw the whole cache away.
    ///
    /// Section 10, cold-tier cache: evict cached copies. This is the one point
    /// of exhaustion with no refusal and no lost work, because every byte here
    /// exists in cold storage too. Returns how many objects went.
    pub fn evict_all(&self) -> Result<usize, StoreError> {
        let mut removed = 0;
        for (path, _, _) in self.entries() {
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn make_room_for(&self, wanted: u64) -> Result<(), StoreError> {
        let mut entries = self.entries();
        let mut held: u64 = entries.iter().map(|(_, _, size)| *size).sum();
        if held + wanted <= self.max_bytes {
            return Ok(());
        }
        // Oldest use first.
        entries.sort_by_key(|(_, used_at, _)| *used_at);
        for (path, _, size) in entries {
            if held + wanted <= self.max_bytes {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                held = held.saturating_sub(size);
            }
        }
        Ok(())
    }

    fn entries(&self) -> Vec<(PathBuf, std::time::SystemTime, u64)> {
        let Ok(read) = std::fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        read.flatten()
            .filter_map(|entry| {
                let data = entry.metadata().ok()?;
                if !data.is_file() {
                    return None;
                }
                let used_at = data.modified().ok()?;
                Some((entry.path(), used_at, data.len()))
            })
            .collect()
    }

    /// One file for one object key. A key holds a slash, and a directory tree
    /// per project would leave empty directories behind after an eviction.
    fn path_for(&self, key: &str) -> PathBuf {
        self.directory.join(key.replace('/', "-"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 24 * 3_600_000;
    const NOW: i64 = 1_785_628_800_000;

    #[test]
    fn a_segment_moves_through_the_tiers_with_its_age() {
        // D24's first policy: 24 hours hot, the next 7 days warm, older cold.
        let policy = TierPolicy::default();
        assert_eq!(policy.tier_for(NOW, NOW), Tier::Hot);
        assert_eq!(policy.tier_for(NOW - DAY / 2, NOW), Tier::Hot);
        assert_eq!(policy.tier_for(NOW - 2 * DAY, NOW), Tier::Warm);
        assert_eq!(policy.tier_for(NOW - 7 * DAY, NOW), Tier::Warm);
        assert_eq!(policy.tier_for(NOW - 30 * DAY, NOW), Tier::Cold);
    }

    #[test]
    fn disk_pressure_asks_for_the_thing_d24_says() {
        let policy = TierPolicy::default();
        assert_eq!(policy.pressure(0.10), Pressure::Rest);
        assert_eq!(policy.pressure(0.70), Pressure::Move);
        assert_eq!(policy.pressure(0.85), Pressure::MoveFaster);
        // A collector that cannot retain data must not acknowledge it.
        assert_eq!(policy.pressure(0.95), Pressure::StopIngest);
        assert_eq!(policy.pressure(1.00), Pressure::StopIngest);
    }

    #[test]
    fn an_object_key_is_the_content_address() {
        // A segment is identified by its content address, so an obsolete upload
        // cannot overwrite a live object.
        let mut manifest = manifest();
        let first = object_key(&manifest);
        manifest.content_address = [2; 32];
        assert_ne!(first, object_key(&manifest));
        // And the same segment always lands on the same key.
        manifest.content_address = [1; 32];
        assert_eq!(first, object_key(&manifest));
    }

    fn manifest() -> Manifest {
        Manifest {
            segment_id: [5; 16],
            content_address: [1; 32],
            tablet_id: 0,
            virtual_shard: 0,
            workspace_id: [8; 16],
            project_id: [9; 16],
            kinds: vec!["event".into()],
            occurred_range: (NOW - 30 * DAY, NOW - 29 * DAY),
            received_range: (NOW - 30 * DAY, NOW - 29 * DAY),
            committed_range: (NOW - 30 * DAY, NOW - 29 * DAY),
            log_range: (0, 10),
            row_count: 100,
            byte_count: 4_000,
            generation: 1,
            tier: "local".into(),
            relative_path: "segments/5.tos".into(),
        }
    }
}
