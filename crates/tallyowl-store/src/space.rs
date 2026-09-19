//! What is left on the device, and who may use it.
//!
//! `docs/FAILURE_MODES.md` section 10 gives six points of exhaustion and one
//! behaviour for each. It also gives the rule the whole module turns on:
//!
//! > A reserve, configurable and non-zero by default, is held back so that the
//! > system can still write the metadata needed to recover.
//!
//! That sentence splits every write into two kinds.
//!
//! **Bulk writes** are the ones that fill a device: the append log, a published
//! segment, a compaction's output, an export. They may use the free space
//! *above* the reserve and never the reserve itself.
//!
//! **Recovery writes** are the small ones that make the rest readable again:
//! the catalog transaction that publishes a segment, a receipt, an erasure
//! record. They may use the reserve, because a device with no room to write a
//! manifest is a device nobody can recover in place.
//!
//! The order matters. A bulk write is refused while there is still a reserve
//! left, so the refusal arrives while the system can still record why.
//!
//! # Why the reserve is not simply "stop at 95 percent"
//!
//! `docs/STORAGE.md` section 13 has a 95 percent watermark for tier movement,
//! which is a policy about *where* data lives. This is a different question:
//! whether a write can be made durable at all. A 4 TB device at 95 percent has
//! 200 GB free and a 100 GB device at 95 percent has 5 GB, and the metadata a
//! recovery needs is the same size in both. A byte count answers the question a
//! percentage cannot.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::store::StoreError;

/// The default reserve, matching `storage.reserveBytes` in the configuration
/// schema. The store takes the configured value; this is what it falls back to
/// when nothing configured one.
pub const DEFAULT_RESERVE_BYTES: u64 = 1024 * 1024 * 1024;

/// The six points of exhaustion in FAILURE_MODES.md section 10.
///
/// A refusal names its point, because "the disk is full" does not tell an
/// operator whether ingest stopped or a compaction gave up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    AppendLog,
    SegmentPublish,
    Compaction,
    Catalog,
    ColdCache,
    Export,
}

impl Point {
    /// The label a metric carries, and the word a message uses.
    pub fn as_str(&self) -> &'static str {
        match self {
            Point::AppendLog => "append-log",
            Point::SegmentPublish => "segment-publish",
            Point::Compaction => "compaction",
            Point::Catalog => "catalog",
            Point::ColdCache => "cold-cache",
            Point::Export => "export",
        }
    }

    /// What section 10 says happens here, in words an operator reads in a log
    /// line rather than words that send them to a document.
    pub fn behaviour(&self) -> &'static str {
        match self {
            Point::AppendLog => {
                "New data is refused until space is free. Nothing already accepted is lost."
            }
            Point::SegmentPublish => {
                "The data stays in the append log, which is the durable record until a \
                 segment replaces it. Nothing is lost."
            }
            Point::Compaction => {
                "The attempt stopped and the original files are untouched. Compaction is \
                 never needed for correctness."
            }
            Point::Catalog => "Settings cannot change. Queries still answer.",
            Point::ColdCache => {
                "Cached copies of remote data were removed. No only copy was removed."
            }
            Point::Export => "The export stopped. It never takes space from live data.",
        }
    }
}

/// What the device holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub free_bytes: u64,
    pub total_bytes: u64,
    /// Which device this is, so two installations sharing one are visible.
    ///
    /// **The reserve is a property of the device and TallyOwl enforces it in
    /// one process.** Two installations on one device each hold back the same
    /// bytes and each believes those bytes are its own, so both may spend the
    /// reserve at once and neither gets what it was promised. L037 asked for an
    /// answer to that and this is it.
    ///
    /// The *hard* guarantee is unaffected: a write is refused when the device
    /// cannot take it, and that check reads real free space, so no installation
    /// ever writes past a full device however many are sharing it. What cannot
    /// be enforced across processes is the *soft* guarantee that recovery will
    /// find the reserve unspent.
    ///
    /// So TallyOwl does not pretend. It reports which device it is on, an
    /// operator can see two installations reporting the same one, and
    /// `docs/FAILURE_MODES.md` section 10 states the constraint.
    pub device_id: u64,
}

impl Device {
    pub fn used_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.free_bytes)
    }
}

/// The free space on the device that holds `path`.
///
/// The path has to exist. A caller asks about a data directory it has already
/// created, so this is not a limitation in practice and it is a clear failure
/// when it happens.
//
// The conversions below look useless on 64-bit Linux, where these fields are
// already `u64`, and they are not useless on a platform where they are 32 bits.
// `from` widens where it must and does nothing where it need not, and a cast in
// its place would silently truncate if a field were ever wider.
#[cfg(unix)]
#[allow(clippy::useless_conversion)]
pub fn device(path: &Path) -> Result<Device, StoreError> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let name = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        StoreError::InvalidArgument(format!(
            "The path {} cannot be asked about because it holds a zero byte.",
            path.display()
        ))
    })?;

    // SAFETY: `name` is a valid NUL-terminated C string that outlives the call,
    // and `stat` is written only by the call and read only after it returns 0.
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let ok = unsafe { libc::statvfs(name.as_ptr(), stat.as_mut_ptr()) } == 0;
    if !ok {
        return Err(StoreError::Unavailable(format!(
            "The free space on the device holding {} could not be read: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }
    let stat = unsafe { stat.assume_init() };

    // `f_bavail` is what an unprivileged process may use, which is the honest
    // number. `f_bfree` counts blocks only root can reach, and a store that
    // believed it had those would refuse nothing and then fail to write.
    // SAFETY: the same valid C string, and `info` is written only by the call.
    let mut info = std::mem::MaybeUninit::<libc::stat>::uninit();
    let device_id = if unsafe { libc::stat(name.as_ptr(), info.as_mut_ptr()) } == 0 {
        // `st_dev` is already a `u64` on this target and is not on every one,
        // so this stays a widening conversion rather than a cast.
        u64::from(unsafe { info.assume_init() }.st_dev)
    } else {
        // Not knowing which device this is costs a report line, never a write.
        0
    };

    let block = u64::from(stat.f_frsize);
    Ok(Device {
        free_bytes: block.saturating_mul(u64::from(stat.f_bavail)),
        total_bytes: block.saturating_mul(u64::from(stat.f_blocks)),
        device_id,
    })
}

#[cfg(not(unix))]
pub fn device(path: &Path) -> Result<Device, StoreError> {
    Err(StoreError::Unavailable(format!(
        "The free space on the device holding {} cannot be read on this platform. \
         TallyOwl runs on Linux and macOS.",
        path.display()
    )))
}

/// The space rule for one data directory.
///
/// A [`Space`] answers one question: may this write happen? It holds no lock
/// and keeps no cache, because the answer changes for reasons outside this
/// process and a cached answer would be a stale one at the moment it matters.
#[derive(Debug)]
pub struct Space {
    directory: PathBuf,
    reserve_bytes: u64,
    /// A test needs a full device without a full device. Nothing else sets it,
    /// and it is `u64::MAX` when unset, which no real device reports.
    pretend_free_bytes: AtomicU64,
    refusals: [AtomicU64; 6],
}

const UNSET: u64 = u64::MAX;

impl Space {
    pub fn new(directory: impl AsRef<Path>, reserve_bytes: u64) -> Space {
        Space {
            directory: directory.as_ref().to_path_buf(),
            reserve_bytes,
            pretend_free_bytes: AtomicU64::new(UNSET),
            refusals: Default::default(),
        }
    }

    pub fn reserve_bytes(&self) -> u64 {
        self.reserve_bytes
    }

    /// Report the device, honouring a test's pretended free space.
    pub fn device(&self) -> Result<Device, StoreError> {
        let mut found = device(&self.directory)?;
        let pretend = self.pretend_free_bytes.load(Ordering::Relaxed);
        if pretend != UNSET {
            found.free_bytes = pretend;
        }
        Ok(found)
    }

    /// Behave as though the device had this much free space.
    ///
    /// Filling a real device to test the behaviour would need a device to fill,
    /// would be slow, and would leave a workstation in a state the test cannot
    /// undo if it fails part-way. The write paths are the thing under test, not
    /// `statvfs`, and `statvfs` has its own test.
    pub fn pretend_free_bytes(&self, bytes: u64) {
        self.pretend_free_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Go back to reporting the real device.
    pub fn stop_pretending(&self) {
        self.pretend_free_bytes.store(UNSET, Ordering::Relaxed);
    }

    /// May a bulk write of `need_bytes` happen?
    ///
    /// A bulk write may use the free space above the reserve and never the
    /// reserve itself.
    pub fn check_bulk(&self, point: Point, need_bytes: u64) -> Result<(), StoreError> {
        let device = self.device()?;
        let usable = device.free_bytes.saturating_sub(self.reserve_bytes);
        if usable >= need_bytes {
            return Ok(());
        }
        self.count(point);
        Err(StoreError::Exhausted(format!(
            "There is not enough space on the device holding {}. {} {} free, and {} is held \
             back so that recovery can still write. {}",
            self.directory.display(),
            self::bytes(device.free_bytes),
            if device.free_bytes == 1 {
                "byte is"
            } else {
                "bytes are"
            },
            self::bytes(self.reserve_bytes),
            point.behaviour()
        )))
    }

    /// May a recovery write of `need_bytes` happen?
    ///
    /// A recovery write may use the reserve. It is refused only when the device
    /// has nothing left at all, which is the state section 10 says cannot
    /// always be recovered in place.
    pub fn check_recovery(&self, point: Point, need_bytes: u64) -> Result<(), StoreError> {
        let device = self.device()?;
        if device.free_bytes >= need_bytes {
            return Ok(());
        }
        self.count(point);
        Err(StoreError::Exhausted(format!(
            "The device holding {} is full. {} free, and even the reserve is gone. {}",
            self.directory.display(),
            self::bytes(device.free_bytes),
            point.behaviour()
        )))
    }

    /// True when the device has eaten into the reserve.
    ///
    /// This is what stops control writes: the reserve exists for recovery
    /// metadata, and a routine settings change is not that.
    pub fn is_inside_reserve(&self) -> bool {
        self.device()
            .map(|device| device.free_bytes < self.reserve_bytes)
            .unwrap_or(false)
    }

    /// True when a bulk write of a typical size would be refused.
    ///
    /// Readiness uses this. Section 10 says to fail readiness **before** the
    /// device is full, so a node stops being sent work while it can still
    /// finish what it holds.
    pub fn is_low(&self) -> bool {
        self.device()
            .map(|device| device.free_bytes.saturating_sub(self.reserve_bytes) < LOW_WATER_BYTES)
            .unwrap_or(false)
    }

    /// Refusals at each point, in the order of [`Point`].
    pub fn refusals(&self) -> [u64; 6] {
        std::array::from_fn(|index| self.refusals[index].load(Ordering::Relaxed))
    }

    /// Refusals since the last call, and reset.
    ///
    /// A counter metric rises by a delta. Reporting the running total would
    /// make the series jump backwards after a restart, and a rate over it would
    /// be meaningless.
    pub fn take_refusals(&self) -> [u64; 6] {
        std::array::from_fn(|index| self.refusals[index].swap(0, Ordering::Relaxed))
    }

    fn count(&self, point: Point) {
        self.refusals[point as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// How much room above the reserve readiness wants to see.
///
/// One segment's worth, rounded up. A node with less than this is going to
/// refuse a write soon, and saying so early is the whole point.
pub const LOW_WATER_BYTES: u64 = 64 * 1024 * 1024;

/// A byte count a person reads without counting digits.
pub fn bytes(count: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1024 * 1024 * 1024),
        ("MiB", 1024 * 1024),
        ("KiB", 1024),
        ("bytes", 1),
    ];
    for (unit, size) in UNITS {
        if count >= size {
            if size == 1 {
                return format!("{count} {unit}");
            }
            return format!("{:.1} {unit}", count as f64 / size as f64);
        }
    }
    format!("{count} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(name: &str) -> PathBuf {
        let path = std::path::PathBuf::from("target")
            .join("space-tests")
            .join(format!("{name}-{}", tallyowl_obs::time::now_nanos()));
        std::fs::create_dir_all(&path).expect("a place to work");
        path
    }

    #[test]
    fn the_real_device_reports_something_believable() {
        // This is the test that covers `statvfs`, so that every other test can
        // pretend without the pretence hiding a broken reading.
        let found = device(&place("real")).expect("the device answers");
        assert!(found.total_bytes > 0, "a device with no size");
        assert!(
            found.free_bytes <= found.total_bytes,
            "more free than exists: {found:?}"
        );
    }

    #[test]
    fn a_path_that_does_not_exist_is_a_clear_failure() {
        let failure = device(Path::new("/nowhere/at/all/really")).unwrap_err();
        assert!(failure.to_string().contains("could not be read"));
    }

    #[test]
    fn a_bulk_write_never_touches_the_reserve() {
        let space = Space::new(place("bulk"), 1_000);
        space.pretend_free_bytes(1_500);

        space.check_bulk(Point::AppendLog, 500).expect("500 fits");
        let refused = space.check_bulk(Point::AppendLog, 501).unwrap_err();
        assert!(matches!(refused, StoreError::Exhausted(_)));
        // The message says what happened to the data, not only that a disk is
        // full, because those are different facts to an operator.
        assert!(refused
            .to_string()
            .contains("Nothing already accepted is lost"));
    }

    #[test]
    fn a_recovery_write_may_use_the_reserve() {
        let space = Space::new(place("recovery"), 1_000);
        space.pretend_free_bytes(900);

        // Below the reserve, so no bulk write is possible at all.
        assert!(space.check_bulk(Point::SegmentPublish, 1).is_err());
        // And the metadata that makes the rest recoverable still gets through.
        space
            .check_recovery(Point::Catalog, 900)
            .expect("the reserve is for this");
        assert!(space.check_recovery(Point::Catalog, 901).is_err());
    }

    #[test]
    fn a_refusal_is_counted_at_the_point_it_happened() {
        let space = Space::new(place("counted"), 1_000);
        space.pretend_free_bytes(0);

        let _ = space.check_bulk(Point::AppendLog, 1);
        let _ = space.check_bulk(Point::AppendLog, 1);
        let _ = space.check_bulk(Point::Compaction, 1);

        let counted = space.refusals();
        assert_eq!(counted[Point::AppendLog as usize], 2);
        assert_eq!(counted[Point::Compaction as usize], 1);
        assert_eq!(counted[Point::Export as usize], 0);
    }

    #[test]
    fn readiness_fails_before_the_device_is_full() {
        let space = Space::new(place("readiness"), 1_000);

        space.pretend_free_bytes(LOW_WATER_BYTES + 1_000);
        assert!(!space.is_low());
        assert!(!space.is_inside_reserve());

        // Still far from full, and already saying so.
        space.pretend_free_bytes(LOW_WATER_BYTES);
        assert!(
            space.is_low(),
            "readiness waited until the reserve was gone"
        );
        assert!(!space.is_inside_reserve());

        space.pretend_free_bytes(999);
        assert!(space.is_inside_reserve());
    }

    #[test]
    fn a_byte_count_reads_as_a_person_would_say_it() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(1024), "1.0 KiB");
        assert_eq!(bytes(1024 * 1024 * 1024), "1.0 GiB");
    }
}
