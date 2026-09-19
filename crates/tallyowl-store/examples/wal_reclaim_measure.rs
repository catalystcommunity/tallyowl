//! How much of the append log survives while the log is busy.
//!
//! `docs/BENCHMARKS.md` section 21 records what this measures and why the
//! number matters: reclamation used to step aside whenever a group commit was
//! in flight, and on a log that never goes quiet it stepped aside for ever. The
//! file then kept every byte the installation had ever written.
//!
//! ```sh
//! cargo run --release -p tallyowl-store --example wal_reclaim_measure -- 25 6 200
//! ```
//!
//! The arguments are the run length in seconds, the number of callers, and the
//! group-commit linger in microseconds. It states the filesystem it ran on,
//! because section 9 of the implementation prompt forbids a storage measurement
//! that does not.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tallyowl_store::wal::{GroupCommit, Wal, FRAME_HEADER_BYTES};

/// The payload each caller appends. A batch frame is larger; this is about the
/// count of reclamations and the bytes left behind, not about throughput.
const PAYLOAD: usize = 192;

fn main() {
    let seconds: u64 = argument(1).unwrap_or(25);
    let writers: usize = argument(2).unwrap_or(6);
    let linger_us: u64 = argument(3).unwrap_or(200);

    let place = std::path::PathBuf::from(
        std::env::var("CARGO_TARGET_DIR").unwrap_or_else(|_| "target".into()),
    )
    .join("wal-reclaim-measure")
    .join(format!("{}", tallyowl_obs::time::now_nanos()));
    std::fs::create_dir_all(&place).expect("a place to work");

    let wal = Wal::open(
        place.join("t.wal"),
        GroupCommit {
            linger: Duration::from_micros(linger_us),
            // Rewrite whatever is covered, whenever it is asked, so this
            // measures the handover and not the amortisation floor.
            min_reclaim_bytes: 0,
            ..GroupCommit::default()
        },
    )
    .expect("the log opens");

    let stop = Arc::new(AtomicU64::new(0));
    let appended = Arc::new(AtomicU64::new(0));
    let rewrites = Arc::new(AtomicU64::new(0));
    let mut threads = Vec::new();

    for _ in 0..writers {
        let wal = Arc::clone(&wal);
        let stop = Arc::clone(&stop);
        let appended = Arc::clone(&appended);
        threads.push(std::thread::spawn(move || {
            while stop.load(Ordering::Relaxed) == 0 {
                wal.append(&[7u8; PAYLOAD]).expect("every append returns");
                appended.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    // One reclaimer, which is what a seal is.
    {
        let wal = Arc::clone(&wal);
        let stop = Arc::clone(&stop);
        let rewrites = Arc::clone(&rewrites);
        threads.push(std::thread::spawn(move || {
            while stop.load(Ordering::Relaxed) == 0 {
                let upto = wal.next_position().saturating_sub(1);
                if wal.reclaim_through(upto).expect("reclaims") > 0 {
                    rewrites.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::yield_now();
            }
        }));
    }

    std::thread::sleep(Duration::from_secs(seconds));
    stop.store(1, Ordering::Relaxed);
    for thread in threads {
        thread.join().expect("no thread wedged");
    }

    let taken = appended.load(Ordering::Relaxed) * (PAYLOAD + FRAME_HEADER_BYTES) as u64;
    let held = wal.durable_bytes();
    println!("filesystem: {}", filesystem(&place));
    println!("callers: {writers}, linger: {linger_us} us, run: {seconds} s");
    println!("appends: {}", appended.load(Ordering::Relaxed));
    println!("reclamations: {}", rewrites.load(Ordering::Relaxed));
    println!(
        "bytes taken: {taken}, bytes still in the log: {held} ({:.1} percent)",
        100.0 * held as f64 / taken.max(1) as f64
    );

    drop(wal);
    let _ = std::fs::remove_dir_all(&place);
}

fn argument<T: std::str::FromStr>(n: usize) -> Option<T> {
    std::env::args().nth(n).and_then(|a| a.parse().ok())
}

/// What the data sits on. A storage measurement on tmpfs is not a storage
/// measurement, and this says which one it was.
fn filesystem(place: &std::path::Path) -> String {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return "unknown".to_string();
    };
    let absolute = place.canonicalize().unwrap_or_else(|_| place.to_path_buf());
    let mut best = ("unknown", 0usize);
    for line in mounts.lines() {
        let mut parts = line.split_whitespace();
        let (Some(_), Some(point), Some(kind)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if absolute.starts_with(point) && point.len() >= best.1 {
            best = (kind, point.len());
        }
    }
    best.0.to_string()
}
