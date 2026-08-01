//! D3 prototype: the embedded transactional catalog.
//!
//! STORAGE.md section 3.3 gives the catalog workload. This benchmark runs that
//! workload against `redb` and reports whether a copy-on-write B+tree meets the
//! ingest hot path.
//!
//! The receipt path is the one that matters. Every batch does one point lookup
//! for deduplication and one durable write. If that path is slow, ingest is
//! slow, and no other catalog property compensates.
//!
//! This is decision-support code. It is not product code.

use redb::{Database, Durability, ReadableDatabase, ReadableTableMetadata, TableDefinition};
use std::time::Instant;

const RECEIPTS: TableDefinition<&str, &[u8]> = TableDefinition::new("receipt");
const SEGMENTS: TableDefinition<&str, &[u8]> = TableDefinition::new("segment");

struct Rng(u64);
impl Rng {
    fn new(s: u64) -> Self {
        Rng(s ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn receipt_key(source: u32, batch: u64) -> String {
    // The ordered prefix layout from STORAGE.md section 3.3.
    format!("receipt/{source:08x}/{batch:016x}")
}

fn segment_key(tablet: u32, generation: u64, segment: u64) -> String {
    format!("segment/{tablet:08x}/{generation:016x}/{segment:016x}")
}

/// A receipt value: batch ID, log position, counts, and a commit time. Small
/// and fixed, which is what the real record looks like.
fn receipt_value(pos: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(&pos.to_le_bytes());
    v.extend_from_slice(&(pos * 7).to_le_bytes());
    v.extend_from_slice(&[0u8; 48]);
    v
}

fn bench_receipts_fresh(dir: &std::path::Path, count: usize, batch_size: usize, durability: Durability) -> f64 {
    let path = dir.join(format!("rc-{batch_size}-{}.redb", matches!(durability, Durability::Immediate)));
    let _ = std::fs::remove_file(&path);
    let db = Database::create(&path).unwrap();
    let r = bench_receipts(&db, count, batch_size, durability);
    drop(db);
    let _ = std::fs::remove_file(&path);
    r
}

fn bench_receipts(db: &Database, count: usize, batch_size: usize, durability: Durability) -> f64 {
    let t = Instant::now();
    let mut written = 0usize;
    while written < count {
        let mut wtx = db.begin_write().unwrap();
        wtx.set_durability(durability).unwrap();
        {
            let mut t = wtx.open_table(RECEIPTS).unwrap();
            for i in 0..batch_size {
                let n = (written + i) as u64;
                t.insert(receipt_key((n % 8) as u32, n).as_str(), receipt_value(n).as_slice())
                    .unwrap();
            }
        }
        wtx.commit().unwrap();
        written += batch_size;
    }
    let s = t.elapsed().as_secs_f64();
    written as f64 / s
}

fn bench_lookup(db: &Database, count: usize, total: usize, seed: u64) -> (f64, usize) {
    let mut r = Rng::new(seed);
    let rtx = db.begin_read().unwrap();
    let table = rtx.open_table(RECEIPTS).unwrap();
    let t = Instant::now();
    let mut hits = 0usize;
    for _ in 0..count {
        let n = r.next() % (total as u64 * 2); // half miss, half hit
        let k = receipt_key((n % 8) as u32, n);
        if table.get(k.as_str()).unwrap().is_some() {
            hits += 1;
        }
    }
    let s = t.elapsed().as_secs_f64();
    (count as f64 / s, hits)
}

fn bench_prefix_scan(db: &Database, tablet: u32) -> (f64, usize) {
    let rtx = db.begin_read().unwrap();
    let table = rtx.open_table(SEGMENTS).unwrap();
    let lo = format!("segment/{tablet:08x}/");
    let hi = format!("segment/{:08x}/", tablet + 1);
    let t = Instant::now();
    let mut n = 0usize;
    for row in table.range(lo.as_str()..hi.as_str()).unwrap() {
        let (_k, _v) = row.unwrap();
        n += 1;
    }
    let s = t.elapsed().as_secs_f64();
    (n as f64 / s, n)
}

fn db_size(path: &std::path::Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn main() {
    // NEVER default to the system temp directory. On this machine /tmp is
    // tmpfs, so a benchmark there measures RAM and reports a durability number
    // that is off by three orders of magnitude.
    let dir = std::env::args()
        .nth(2)
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/tallyowl-bench")
        })
        .join(format!("catalog-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("catalog.redb");

    let receipts: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);

    println!("# D3 embedded catalog benchmark (redb)");
    println!("path={}", path.display());
    println!("NOTE: this path must be on real storage, never tmpfs.");
    println!("receipts={receipts}");
    println!();

    // The receipt write path, at several batch sizes and both durability modes.
    //
    // `Immediate` fsyncs on commit. That is what a durable receipt requires.
    // `Eventual` does not, and it is shown only to size the cost of safety.
    println!("## Receipt commit path");
    println!(
        "{:<12} {:<12} {:>14} {:>16}",
        "durability", "per commit", "receipts/s", "commits/s"
    );
    for (label, dur) in [
        ("immediate", Durability::Immediate),
        ("none (unsafe)", Durability::None),
    ] {
        for batch in [1usize, 16, 256] {
            let rate = bench_receipts_fresh(&dir, receipts / 4, batch, dur);
            println!(
                "{:<12} {:<12} {:>14.0} {:>16.0}",
                label,
                batch,
                rate,
                rate / batch as f64
            );
        }
    }

    // Fill for the read tests.
    let db = Database::create(&path).unwrap();
    println!();
    println!("## Filling for read tests");
    let t = Instant::now();
    let mut n = 0u64;
    while (n as usize) < receipts {
        let mut wtx = db.begin_write().unwrap();
        wtx.set_durability(Durability::None).unwrap();
        {
            let mut tbl = wtx.open_table(RECEIPTS).unwrap();
            let mut seg = wtx.open_table(SEGMENTS).unwrap();
            for _ in 0..1000 {
                tbl.insert(
                    receipt_key((n % 8) as u32, n).as_str(),
                    receipt_value(n).as_slice(),
                )
                .unwrap();
                if n % 100 == 0 {
                    seg.insert(
                        segment_key(((n / 100) % 8) as u32, n / 10_000, n).as_str(),
                        receipt_value(n).as_slice(),
                    )
                    .unwrap();
                }
                n += 1;
            }
        }
        wtx.commit().unwrap();
    }
    println!("inserted {n} receipts in {:.1}s", t.elapsed().as_secs_f64());

    println!();
    println!("## Deduplication lookup (the ingest hot path)");
    let (rate, hits) = bench_lookup(&db, 200_000, receipts, 7);
    println!("point lookups/s {:>14.0}   (hits {hits})", rate);
    println!("mean latency    {:>14.2} us", 1_000_000.0 / rate);

    println!();
    println!("## Manifest prefix scan");
    let (rate, count) = bench_prefix_scan(&db, 3);
    println!("rows/s {:>18.0}   (rows {count})", rate);

    println!();
    println!("## Size on disk");
    let sz = db_size(&path);
    println!("file          {:>12} MiB", sz / (1024 * 1024));
    println!(
        "bytes/receipt {:>12.1}",
        sz as f64 / n as f64
    );

    // Reopen, which is what recovery does.
    println!();
    println!("## Reopen");
    drop(db);
    let t = Instant::now();
    let db2 = Database::open(&path).unwrap();
    println!("open {:.3}s", t.elapsed().as_secs_f64());
    let rtx = db2.begin_read().unwrap();
    let tbl = rtx.open_table(RECEIPTS).unwrap();
    println!("rows after reopen {}", tbl.len().unwrap());
    drop(rtx);
    drop(db2);

    let _ = std::fs::remove_dir_all(&dir);
}
