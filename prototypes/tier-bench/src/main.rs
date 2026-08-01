//! D24 prototype: cold tiering, range reads, and the bounded page cache.
//!
//! STORAGE.md section 13 claims that a cold query reads only the index and
//! column page ranges that it needs, and that a bounded cache makes a repeated
//! query cheap. Those are the two claims worth measuring, because they decide
//! whether cold data stays part of the logical database or becomes an export.
//!
//! Object-store latency dominates a cold read, and it varies by provider and by
//! network. The benchmark therefore injects a configurable first-byte latency
//! rather than measuring one particular bucket. What it measures is the design:
//! how many round trips and how many bytes each access pattern costs.
//!
//! It also verifies that Apache OpenDAL, which D24 names, performs the ranged
//! read that the design depends on.
//!
//! This is decision-support code. It is not product code.

use opendal::{services::Fs, Operator};
use std::time::{Duration, Instant};

const PAGE: usize = 64 * 1024; // the D17 compressed page target
const SEGMENT_PAGES: usize = 4096; // 4096 * 64 KiB = 256 MiB, the cluster target

/// Simulated object-store first-byte latency. A local bucket is a few
/// milliseconds; a remote one is tens.
fn injected_latency(ms: u64) -> Duration {
    Duration::from_millis(ms)
}

struct Access {
    name: &'static str,
    /// Which pages a query touches out of a 256 MiB segment.
    pages: Vec<usize>,
}

fn access_patterns() -> Vec<Access> {
    // A point lookup verifies one value: one index block plus one data page.
    let point = vec![0, 1];

    // A narrow aggregate reads one column across the segment. One column of a
    // sixteen-column segment is every sixteenth page.
    let one_column: Vec<usize> = (0..SEGMENT_PAGES).step_by(16).collect();

    // A wide scan reads four columns.
    let four_columns: Vec<usize> = (0..SEGMENT_PAGES)
        .filter(|p| p % 16 < 4)
        .collect();

    // A whole-segment read, which is what a design without range reads costs.
    let whole: Vec<usize> = (0..SEGMENT_PAGES).collect();

    vec![
        Access { name: "point lookup (2 pages)", pages: point },
        Access { name: "one column aggregate", pages: one_column },
        Access { name: "four column scan", pages: four_columns },
        Access { name: "whole segment", pages: whole },
    ]
}

/// A bounded page cache with least-recently-used eviction.
struct PageCache {
    capacity_pages: usize,
    order: std::collections::VecDeque<usize>,
    present: std::collections::HashSet<usize>,
    hits: usize,
    misses: usize,
}

impl PageCache {
    fn new(capacity_bytes: usize) -> Self {
        PageCache {
            capacity_pages: capacity_bytes / PAGE,
            order: Default::default(),
            present: Default::default(),
            hits: 0,
            misses: 0,
        }
    }
    fn get(&mut self, page: usize) -> bool {
        if self.present.contains(&page) {
            self.hits += 1;
            self.order.retain(|p| *p != page);
            self.order.push_back(page);
            true
        } else {
            self.misses += 1;
            if self.order.len() >= self.capacity_pages {
                if let Some(evict) = self.order.pop_front() {
                    self.present.remove(&evict);
                }
            }
            self.order.push_back(page);
            self.present.insert(page);
            false
        }
    }
}

/// Group sorted page numbers into contiguous runs. A run is one ranged request,
/// so this is what decides the round-trip count.
fn coalesce(pages: &[usize]) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &p in pages {
        match runs.last_mut() {
            Some((_start, end)) if *end + 1 == p => *end = p,
            _ => runs.push((p, p)),
        }
    }
    runs
}

#[tokio::main]
async fn main() {
    let latency_ms: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);

    println!("# D24 cold tiering benchmark");
    println!(
        "page={} KiB  segment={} MiB  injected first-byte latency={} ms",
        PAGE / 1024,
        SEGMENT_PAGES * PAGE / (1024 * 1024),
        latency_ms
    );
    println!();

    // ---------------------------------------------------------------------
    // Verify that OpenDAL does the ranged read the design depends on.
    // ---------------------------------------------------------------------
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap())
        .join(".cache/tallyowl-bench/tier");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let op = Operator::new(Fs::default().root(dir.to_str().unwrap()))
        .unwrap()
        .finish();

    // Write a segment-shaped object.
    let mut segment = vec![0u8; SEGMENT_PAGES * PAGE];
    for (i, b) in segment.iter_mut().enumerate() {
        *b = (i / PAGE) as u8;
    }
    let t = Instant::now();
    op.write("segment-0001.tos", segment.clone()).await.unwrap();
    let write_s = t.elapsed().as_secs_f64();
    println!("## OpenDAL check");
    println!(
        "wrote {} MiB in {:.2}s ({:.0} MiB/s)",
        SEGMENT_PAGES * PAGE / (1024 * 1024),
        write_s,
        (SEGMENT_PAGES * PAGE) as f64 / (1024.0 * 1024.0) / write_s
    );

    // A ranged read of one page.
    let t = Instant::now();
    let got = op
        .read_with("segment-0001.tos")
        .range((7 * PAGE) as u64..((8 * PAGE) as u64))
        .await
        .unwrap();
    let range_us = t.elapsed().as_micros();
    let bytes = got.to_vec();
    assert_eq!(bytes.len(), PAGE);
    assert_eq!(bytes[0], 7, "ranged read returned the wrong page");
    println!(
        "ranged read of one 64 KiB page: {} bytes, first byte {}, {} us",
        bytes.len(),
        bytes[0],
        range_us
    );
    println!("OpenDAL supports the ranged read that cold queries need.");

    // ---------------------------------------------------------------------
    // Access patterns: bytes fetched and round trips.
    // ---------------------------------------------------------------------
    println!();
    println!("## Bytes and round trips for each access pattern");
    println!(
        "{:<26} {:>8} {:>12} {:>10} {:>14} {:>14}",
        "pattern", "pages", "MiB fetched", "requests", "latency cost", "vs whole seg"
    );
    let whole_bytes = SEGMENT_PAGES * PAGE;
    for a in &access_patterns() {
        let runs = coalesce(&a.pages);
        let bytes = a.pages.len() * PAGE;
        let lat = injected_latency(latency_ms) * runs.len() as u32;
        println!(
            "{:<26} {:>8} {:>12.1} {:>10} {:>13.1}s {:>13.1}%",
            a.name,
            a.pages.len(),
            bytes as f64 / (1024.0 * 1024.0),
            runs.len(),
            lat.as_secs_f64(),
            100.0 * bytes as f64 / whole_bytes as f64
        );
    }

    println!();
    println!("note: a strided column read does not coalesce, so each page is its");
    println!("own request. That is the cost that matters, not the byte count.");

    // ---------------------------------------------------------------------
    // Column-major layout: the same query when a column is contiguous.
    // ---------------------------------------------------------------------
    println!();
    println!("## The same patterns when one column is contiguous on the object");
    println!(
        "{:<26} {:>8} {:>12} {:>10} {:>14}",
        "pattern", "pages", "MiB fetched", "requests", "latency cost"
    );
    for (name, pages) in [
        ("one column aggregate", SEGMENT_PAGES / 16),
        ("four column scan", SEGMENT_PAGES / 4),
    ] {
        // Contiguous: one request for the column, four for four columns.
        let requests = if name.starts_with("four") { 4 } else { 1 };
        let lat = injected_latency(latency_ms) * requests as u32;
        println!(
            "{:<26} {:>8} {:>12.1} {:>10} {:>13.1}s",
            name,
            pages,
            (pages * PAGE) as f64 / (1024.0 * 1024.0),
            requests,
            lat.as_secs_f64()
        );
    }

    // ---------------------------------------------------------------------
    // Bounded cache behaviour under a realistic dashboard pattern.
    // ---------------------------------------------------------------------
    println!();
    println!("## Bounded page cache, dashboard refresh pattern");
    println!(
        "{:<18} {:>12} {:>10} {:>10} {:>14}",
        "cache size", "hit rate", "hits", "misses", "MiB fetched"
    );

    // A dashboard runs the same few queries repeatedly, with a long tail of ad
    // hoc ones. Model 20 repeats of a hot set plus a cold tail.
    let hot: Vec<usize> = (0..SEGMENT_PAGES).step_by(16).take(64).collect();
    let mut sequence: Vec<usize> = Vec::new();
    for round in 0..20 {
        sequence.extend_from_slice(&hot);
        // an ad hoc query each round touches a different slice
        let base = (round * 137) % (SEGMENT_PAGES - 64);
        sequence.extend((base..base + 64).collect::<Vec<_>>());
    }

    for cap_mib in [4usize, 16, 64, 256] {
        let mut cache = PageCache::new(cap_mib * 1024 * 1024);
        for p in &sequence {
            cache.get(*p);
        }
        let total = cache.hits + cache.misses;
        println!(
            "{:<18} {:>11.1}% {:>10} {:>10} {:>14.1}",
            format!("{cap_mib} MiB"),
            100.0 * cache.hits as f64 / total as f64,
            cache.hits,
            cache.misses,
            (cache.misses * PAGE) as f64 / (1024.0 * 1024.0)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
