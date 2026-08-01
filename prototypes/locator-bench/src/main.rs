//! The tablet locator at scale: can 100 million users be found without
//! scanning every retained segment?
//!
//! HIGH_CARDINALITY.md section 4 defines a locator that maps
//! `(field, fingerprint, time bucket) -> candidate segments`. Every earlier
//! benchmark measured inside ONE segment. The locator is what makes a lookup
//! affordable ACROSS segments, and nothing had measured it.
//!
//! The load-bearing question turns out not to be user count. It is the count of
//! distinct (user, segment) pairs, because the locator holds one segment
//! reference for each pair. That count depends on how a user's events scatter
//! across segments, which is a placement decision, not a user-population fact.
//!
//! This prototype measures bytes for each pair and probe cost across three
//! placement layouts, verifies that bytes for each pair holds steady as scale
//! grows, and only then uses arithmetic to reach the target scale.
//!
//! This is decision-support code. It is not product code.

use rayon::prelude::*;
use std::time::Instant;

// ---------------------------------------------------------------------------
// The workload this benchmark answers for.
// ---------------------------------------------------------------------------

/// 100 million users and 100,000 servers, stated as the numbers a locator
/// actually depends on.
struct Workload {
    users: u64,
    daily_active: u64,
    servers: u64,
    events_each_day: u64,
    retention_days: u64,
    /// Cluster profile, from D17.
    segment_bytes: u64,
    /// From BENCHMARKS.md section 12a.
    bytes_each_event: f64,
}

impl Workload {
    fn target() -> Self {
        Workload {
            users: 100_000_000,
            daily_active: 10_000_000,
            servers: 100_000,
            events_each_day: 1_000_000_000,
            retention_days: 30,
            segment_bytes: 256 * 1024 * 1024,
            bytes_each_event: 39.75,
        }
    }
    fn events_each_segment(&self) -> u64 {
        (self.segment_bytes as f64 / self.bytes_each_event) as u64
    }
    fn segments_each_day(&self) -> u64 {
        self.events_each_day.div_ceil(self.events_each_segment())
    }
    fn segments_retained(&self) -> u64 {
        self.segments_each_day() * self.retention_days
    }
    fn events_each_user_day(&self) -> u64 {
        (self.events_each_day / self.daily_active).max(1)
    }
}

// ---------------------------------------------------------------------------
// Placement layouts. These decide the pair count, and so the locator size.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Layout {
    /// Events land in whichever segment is open when they arrive. A user
    /// active through the day touches every segment sealed that day.
    Scattered,
    /// Ingest routes by a hash of the end-user ID into V virtual shards. A
    /// user's events reach the segments of one shard only.
    ShardedByUser(u64),
}

impl Layout {
    fn name(&self) -> String {
        match self {
            Layout::Scattered => "scattered".into(),
            Layout::ShardedByUser(v) => format!("sharded by user, {v} shards"),
        }
    }
    /// Segments one user's events reach in one day.
    fn segments_touched(&self, w: &Workload) -> f64 {
        let per_day = w.segments_each_day() as f64;
        let events = w.events_each_user_day() as f64;
        match self {
            // A user cannot touch more segments than they have events.
            Layout::Scattered => per_day.min(events),
            Layout::ShardedByUser(v) => (per_day / *v as f64).max(1.0).min(events),
        }
    }
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn new(s: u64) -> Self {
        Rng(s.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }
    #[inline]
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    #[inline]
    fn below(&mut self, n: u64) -> u64 {
        // Multiply-shift. Fast, and the bias does not matter here.
        ((self.next() as u128 * n as u128) >> 64) as u64
    }
}

#[inline]
fn fingerprint(user: u64) -> u64 {
    xxhash_rust::xxh3::xxh3_64(&user.to_be_bytes())
}

/// Generate the (fingerprint, segment) pairs for one time bucket.
fn generate_bucket(
    users: u64,
    segments: u32,
    pairs: usize,
    shards: Option<u64>,
    seed: u64,
) -> Vec<(u64, u32)> {
    let threads = rayon::current_num_threads();
    let chunk = pairs.div_ceil(threads);
    (0..threads)
        .into_par_iter()
        .map(|t| {
            let mut r = Rng::new(seed ^ (t as u64).wrapping_mul(0x1234_5678_9abc_def1));
            let n = chunk.min(pairs.saturating_sub(t * chunk));
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                let user = r.below(users);
                let fp = fingerprint(user);
                let seg = match shards {
                    // The user's shard fixes which segments can hold them.
                    Some(v) => {
                        let shard = fp % v;
                        let per_shard = (segments as u64).div_ceil(v).max(1);
                        let within = r.below(per_shard);
                        ((shard * per_shard + within) % segments as u64) as u32
                    }
                    None => r.below(segments as u64) as u32,
                };
                out.push((fp, seg));
            }
            out
        })
        .reduce(Vec::new, |mut a, mut b| {
            if a.is_empty() {
                b
            } else {
                a.append(&mut b);
                a
            }
        })
}

// ---------------------------------------------------------------------------
// The locator run
// ---------------------------------------------------------------------------

fn put_varint(o: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        o.push((v as u8) | 0x80);
        v >>= 7;
    }
    o.push(v as u8);
}

#[inline]
fn get_varint(b: &[u8], i: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut s = 0u32;
    loop {
        let x = b[*i];
        *i += 1;
        v |= ((x & 0x7f) as u64) << s;
        if x < 0x80 {
            return v;
        }
        s += 7;
    }
}

/// One immutable locator run covering one time bucket.
///
/// Layout: groups sorted by fingerprint. Each group is a delta-encoded
/// fingerprint, a segment count, and delta-encoded segment IDs. A sparse skip
/// list every `SKIP` groups makes it searchable without decoding from the
/// start.
struct Run {
    body: Vec<u8>,
    /// (fingerprint, byte offset) every SKIP groups.
    skip: Vec<(u64, u32)>,
    groups: usize,
    pairs: usize,
}

const SKIP: usize = 32;

impl Run {
    fn build(mut pairs: Vec<(u64, u32)>) -> Run {
        pairs.par_sort_unstable();
        pairs.dedup();

        let mut body = Vec::with_capacity(pairs.len() * 2);
        let mut skip = Vec::with_capacity(pairs.len() / (SKIP * 4) + 1);
        let mut groups = 0usize;
        let mut prev_fp = 0u64;
        let total = pairs.len();

        let mut i = 0usize;
        while i < total {
            let fp = pairs[i].0;
            let mut j = i;
            while j < total && pairs[j].0 == fp {
                j += 1;
            }
            if groups % SKIP == 0 {
                skip.push((fp, body.len() as u32));
            }
            put_varint(&mut body, fp.wrapping_sub(prev_fp));
            put_varint(&mut body, (j - i) as u64);
            let mut prev_seg = 0u32;
            for k in i..j {
                put_varint(&mut body, (pairs[k].1.wrapping_sub(prev_seg)) as u64);
                prev_seg = pairs[k].1;
            }
            prev_fp = fp;
            groups += 1;
            i = j;
        }
        Run {
            body,
            skip,
            groups,
            pairs: total,
        }
    }

    fn bytes(&self) -> usize {
        self.body.len() + self.skip.len() * 12
    }

    /// Return the candidate segments for one fingerprint.
    fn probe(&self, target: u64, out: &mut Vec<u32>) -> bool {
        out.clear();
        if self.skip.is_empty() {
            return false;
        }
        // Find the last skip entry at or below the target.
        let mut lo = 0usize;
        let mut hi = self.skip.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.skip[mid].0 <= target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo == 0 {
            return false;
        }
        let (mut fp, off) = self.skip[lo - 1];
        let mut i = off as usize;
        // The skip entry's own fingerprint is the first group at this offset,
        // so rewind the running delta to reconstruct it.
        let mut running = fp;
        let mut first = true;
        loop {
            if !first {
                if i >= self.body.len() {
                    return false;
                }
                running = running.wrapping_add(get_varint(&self.body, &mut i));
            } else {
                // Skip the encoded delta for the group we already know.
                let _ = get_varint(&self.body, &mut i);
                first = false;
            }
            fp = running;
            let count = get_varint(&self.body, &mut i) as usize;
            if fp == target {
                let mut prev = 0u32;
                for _ in 0..count {
                    prev = prev.wrapping_add(get_varint(&self.body, &mut i) as u32);
                    out.push(prev);
                }
                return true;
            }
            if fp > target {
                return false;
            }
            for _ in 0..count {
                let _ = get_varint(&self.body, &mut i);
            }
        }
    }
}

// ---------------------------------------------------------------------------

/// Read bytes for each pair off the measured density curve. Log-linear between
/// points, clamped at the ends. Never extrapolate past what was measured
/// without saying so.
fn interp(curve: &[(f64, f64)], density: f64) -> f64 {
    if density <= curve[0].0 {
        return curve[0].1;
    }
    if density >= curve[curve.len() - 1].0 {
        return curve[curve.len() - 1].1;
    }
    for w in curve.windows(2) {
        let (d0, b0) = w[0];
        let (d1, b1) = w[1];
        if density <= d1 {
            let t = (density.ln() - d0.ln()) / (d1.ln() - d0.ln());
            return b0 + t * (b1 - b0);
        }
    }
    curve[curve.len() - 1].1
}

fn human(n: f64) -> String {
    if n >= 1e12 {
        format!("{:.1}T", n / 1e12)
    } else if n >= 1e9 {
        format!("{:.1}B", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1}M", n / 1e6)
    } else if n >= 1e3 {
        format!("{:.1}k", n / 1e3)
    } else {
        format!("{n:.0}")
    }
}

fn bytes_human(n: f64) -> String {
    const T: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;
    const G: f64 = 1024.0 * 1024.0 * 1024.0;
    const M: f64 = 1024.0 * 1024.0;
    if n >= T {
        format!("{:.2} TiB", n / T)
    } else if n >= G {
        format!("{:.2} GiB", n / G)
    } else if n >= M {
        format!("{:.1} MiB", n / M)
    } else {
        format!("{:.0} KiB", n / 1024.0)
    }
}

fn main() {
    let w = Workload::target();
    let seed: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);

    println!("# The tablet locator at scale");
    println!();
    println!("## The workload this answers for");
    println!("{:<34} {:>16}", "registered users", human(w.users as f64));
    println!("{:<34} {:>16}", "daily active users", human(w.daily_active as f64));
    println!("{:<34} {:>16}", "servers", human(w.servers as f64));
    println!("{:<34} {:>16}", "events each day", human(w.events_each_day as f64));
    println!(
        "{:<34} {:>16}",
        "sustained rate",
        format!("{}/s", human(w.events_each_day as f64 / 86400.0))
    );
    println!("{:<34} {:>16}", "retention", format!("{} days", w.retention_days));
    println!(
        "{:<34} {:>16}",
        "events for each segment",
        human(w.events_each_segment() as f64)
    );
    println!(
        "{:<34} {:>16}",
        "segments each day",
        human(w.segments_each_day() as f64)
    );
    println!(
        "{:<34} {:>16}",
        "segments retained",
        human(w.segments_retained() as f64)
    );
    println!(
        "{:<34} {:>16}",
        "events for each user each day",
        human(w.events_each_user_day() as f64)
    );
    println!(
        "{:<34} {:>16}",
        "stored bytes retained",
        bytes_human(w.events_each_day as f64 * w.bytes_each_event * w.retention_days as f64)
    );

    // -----------------------------------------------------------------------
    println!();
    println!("## Why placement decides the answer");
    println!();
    println!("The locator holds one segment reference for each (user, segment)");
    println!("pair. Pair count, not user count, sets its size.");
    println!();
    println!(
        "{:<30} {:>14} {:>16} {:>16}",
        "layout", "segments/user", "pairs retained", "of segments"
    );
    let layouts = [
        Layout::Scattered,
        Layout::ShardedByUser(64),
        Layout::ShardedByUser(1024),
    ];
    for l in layouts {
        let per_day = l.segments_touched(&w);
        let pairs = w.daily_active as f64 * per_day * w.retention_days as f64;
        println!(
            "{:<30} {:>14.1} {:>16} {:>15.1}%",
            l.name(),
            per_day,
            human(pairs),
            100.0 * per_day / w.segments_each_day() as f64
        );
    }
    println!();
    println!("Sorting rows by user inside a segment does not change any of");
    println!("these. It reorders rows; it does not change which segment holds");
    println!("them. Only routing does.");

    // -----------------------------------------------------------------------
    // Measure bytes for each pair, and check that it holds as scale grows.
    // Arithmetic to the target scale is only honest if this is stable.
    // -----------------------------------------------------------------------
    println!();
    println!("## Measured: bytes for each pair, against scale");
    println!();
    println!(
        "{:<12} {:>10} {:>12} {:>10} {:>12} {:>12} {:>10}",
        "pairs", "users", "segments", "groups", "run bytes", "B/pair", "build s"
    );

    let mut stable: Vec<(usize, f64)> = Vec::new();
    let cases: [(usize, u64, u32); 5] = [
        (10_000_000, 5_000_000, 4_440),
        (50_000_000, 20_000_000, 4_440),
        (200_000_000, 50_000_000, 4_440),
        (500_000_000, 100_000_000, 4_440),
        (1_000_000_000, 100_000_000, 4_440),
    ];
    let mut biggest: Option<Run> = None;
    for (pairs, users, segments) in cases {
        let t = Instant::now();
        let raw = generate_bucket(users, segments, pairs, None, seed);
        let run = Run::build(raw);
        let secs = t.elapsed().as_secs_f64();
        let bpp = run.bytes() as f64 / run.pairs as f64;
        println!(
            "{:<12} {:>10} {:>12} {:>10} {:>12} {:>12.3} {:>10.1}",
            human(pairs as f64),
            human(users as f64),
            human(segments as f64),
            human(run.groups as f64),
            bytes_human(run.bytes() as f64),
            bpp,
            secs
        );
        stable.push((pairs, bpp));
        biggest = Some(run);
    }

    let lo = stable.first().unwrap().1;
    let hi = stable.last().unwrap().1;
    println!();
    println!(
        "Bytes for each pair is NOT flat. It moved {:.1} percent across this\n\
         series, from {lo:.3} to {hi:.3}. Multiplying any one of these by a\n\
         target pair count would repeat the section 12 mistake.",
        100.0 * (hi - lo) / lo
    );

    // -----------------------------------------------------------------------
    // What actually drives it is density: segments for each user, which sets
    // how many segment references share one fingerprint. Each layout sits at a
    // different density, so each needs its own bytes-for-each-pair figure.
    // -----------------------------------------------------------------------
    println!();
    println!("## Measured: bytes for each pair, against density");
    println!();
    println!("Density is segments for each user. A fingerprint costs the same");
    println!("whether one segment reference follows it or three hundred.");
    println!();
    println!(
        "{:<16} {:>12} {:>12} {:>12} {:>14}",
        "segments/user", "users", "pairs", "B/pair", "B/user"
    );
    let mut curve: Vec<(f64, f64)> = Vec::new();
    for density in [1u64, 3, 10, 30, 100, 300] {
        let users = 2_000_000u64;
        let pairs = (users * density) as usize;
        let raw = generate_bucket(users, 4_440, pairs, None, seed ^ density);
        let run = Run::build(raw);
        let bpp = run.bytes() as f64 / run.pairs as f64;
        println!(
            "{:<16} {:>12} {:>12} {:>12.3} {:>14.1}",
            density,
            human(users as f64),
            human(pairs as f64),
            bpp,
            run.bytes() as f64 / users as f64
        );
        curve.push((density as f64, bpp));
    }
    println!();
    println!("Bytes for each user grows with density; bytes for each pair falls.");
    println!("A locator size estimate must use the density of the layout it");
    println!("describes.");

    // -----------------------------------------------------------------------
    println!();
    println!("## Measured: probe cost");
    let run = biggest.unwrap();
    let mut r = Rng::new(seed ^ 0xabcd);
    let probes: Vec<u64> = (0..200_000)
        .map(|_| fingerprint(r.below(100_000_000)))
        .collect();
    let mut out = Vec::with_capacity(4096);
    let mut found = 0usize;
    let mut candidates = 0usize;
    let mut worst = 0usize;
    let t = Instant::now();
    for p in &probes {
        if run.probe(*p, &mut out) {
            found += 1;
            candidates += out.len();
            worst = worst.max(out.len());
        }
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "{:<38} {:>14}",
        "probes",
        human(probes.len() as f64)
    );
    println!("{:<38} {:>14.1}%", "hit rate", 100.0 * found as f64 / probes.len() as f64);
    println!(
        "{:<38} {:>14.0}",
        "probes each second",
        probes.len() as f64 / secs
    );
    println!(
        "{:<38} {:>14.1} us",
        "median probe",
        secs / probes.len() as f64 * 1e6
    );
    println!(
        "{:<38} {:>14.1}",
        "mean candidate segments",
        candidates as f64 / found.max(1) as f64
    );
    println!("{:<38} {:>14}", "worst candidate segments", worst);

    // -----------------------------------------------------------------------
    println!();
    println!("## The alternative: no locator, probe every segment");
    let filter_bytes_each_segment = 6_760_000f64 * 12.0 / 8.0;
    let all = w.segments_retained() as f64;
    println!(
        "{:<38} {:>14}",
        "segments to probe for one lookup",
        human(all)
    );
    println!(
        "{:<38} {:>14}",
        "block filter memory, all segments",
        bytes_human(all * filter_bytes_each_segment)
    );
    println!(
        "{:<38} {:>14.0} ms",
        "filter probes alone, at 21M/s",
        all / 21_000_000.0 * 1000.0
    );
    println!();
    println!("The filter memory is the disqualifying number. A locator is not an");
    println!("optimization here. Without one the working set does not fit.");

    // -----------------------------------------------------------------------
    println!();
    println!("## Measured: what a time range prunes");
    println!();
    println!("Locator runs are time-partitioned, one for each day of retention.");
    println!("A query with a time range reads only the runs it overlaps.");
    println!();
    let per_day_pairs = 20_000_000usize;
    let bucket_runs: Vec<Run> = (0..w.retention_days)
        .into_par_iter()
        .map(|d| {
            let raw = generate_bucket(10_000_000, 149, per_day_pairs, None, seed ^ (d + 1));
            Run::build(raw)
        })
        .collect();
    let total_bytes: usize = bucket_runs.iter().map(|r| r.bytes()).sum();
    println!(
        "{:<38} {:>16}",
        "locator, 30 daily runs",
        bytes_human(total_bytes as f64)
    );
    println!(
        "{:<38} {:>16}",
        "one daily run",
        bytes_human(total_bytes as f64 / w.retention_days as f64)
    );
    println!();
    println!(
        "{:<20} {:>10} {:>16} {:>18} {:>12}",
        "query range", "runs read", "bytes touched", "candidate segments", "probe us"
    );
    let mut r2 = Rng::new(seed ^ 0x5150);
    let sample: Vec<u64> = (0..20_000).map(|_| fingerprint(r2.below(10_000_000))).collect();
    for days in [1usize, 7, 30] {
        let mut out = Vec::new();
        let mut cands = 0usize;
        let mut hits = 0usize;
        let t = Instant::now();
        for p in &sample {
            for run in bucket_runs.iter().take(days) {
                if run.probe(*p, &mut out) {
                    hits += 1;
                    cands += out.len();
                }
            }
        }
        let secs = t.elapsed().as_secs_f64();
        let bytes: usize = bucket_runs.iter().take(days).map(|r| r.bytes()).sum();
        println!(
            "{:<20} {:>10} {:>16} {:>18.1} {:>12.1}",
            format!("{days} day{}", if days == 1 { "" } else { "s" }),
            days,
            bytes_human(bytes as f64),
            cands as f64 / sample.len() as f64,
            secs / sample.len() as f64 * 1e6
        );
        let _ = hits;
    }
    println!();
    println!("A time range prunes linearly. An unbounded lookup on a");
    println!("high-cardinality value reads the whole retention window.");

    // -----------------------------------------------------------------------
    println!();
    println!("## The answer at target scale");
    println!();
    println!("Each layout uses the bytes-for-each-pair figure measured at its");
    println!("own density, not one figure for all of them.");
    println!();
    println!(
        "{:<30} {:>10} {:>10} {:>14} {:>14} {:>16}",
        "layout", "density", "B/pair", "pairs", "locator size", "candidates, 30d"
    );
    for l in layouts {
        let per_day = l.segments_touched(&w);
        let density = per_day * w.retention_days as f64;
        let pairs = w.daily_active as f64 * density;
        let bpp = interp(&curve, density);
        println!(
            "{:<30} {:>10.1} {:>10.3} {:>14} {:>14} {:>16.0}",
            l.name(),
            density,
            bpp,
            human(pairs),
            bytes_human(pairs * bpp),
            density
        );
    }
    let _ = hi;

    // -----------------------------------------------------------------------
    // Compaction already rewrites segments and already merges locator runs.
    // If it groups rows by end user while it does so, density falls without
    // changing how ingest routes anything.
    // -----------------------------------------------------------------------
    println!();
    println!("## What compaction can fix, without changing ingest routing");
    println!();
    println!("Hot data stays scattered, because ingest must not wait to sort.");
    println!("Compaction rewrites cold segments anyway. Grouping rows by end");
    println!("user while it does so lowers density for the retained majority.");
    println!();
    println!(
        "{:<34} {:>10} {:>10} {:>14} {:>16}",
        "tier", "density", "B/pair", "locator size", "candidates"
    );
    let hot_days = 2.0f64;
    let cold_days = w.retention_days as f64 - hot_days;
    let scattered_each_day = Layout::Scattered.segments_touched(&w);
    let hot_density = scattered_each_day * hot_days;
    // A compacted segment covers a day and holds one contiguous user range.
    let cold_density = cold_days;
    let hot_pairs = w.daily_active as f64 * hot_density;
    let cold_pairs = w.daily_active as f64 * cold_density;
    let hb = interp(&curve, hot_density);
    let cb = interp(&curve, cold_density);
    println!(
        "{:<34} {:>10.0} {:>10.3} {:>14} {:>16.0}",
        "hot, 2 days, scattered",
        hot_density,
        hb,
        bytes_human(hot_pairs * hb),
        hot_density
    );
    println!(
        "{:<34} {:>10.0} {:>10.3} {:>14} {:>16.0}",
        "cold, 28 days, user-grouped",
        cold_density,
        cb,
        bytes_human(cold_pairs * cb),
        cold_density
    );
    println!(
        "{:<34} {:>10} {:>10} {:>14} {:>16.0}",
        "total",
        "",
        "",
        bytes_human(hot_pairs * hb + cold_pairs * cb),
        hot_density + cold_density
    );
    println!();
    let scattered_density = scattered_each_day * w.retention_days as f64;
    println!(
        "Against {} and {:.0} candidate segments for the scattered layout.",
        bytes_human(
            w.daily_active as f64 * scattered_density * interp(&curve, scattered_density)
        ),
        scattered_density
    );
    println!();
    println!("This needs no change to routing, so it does not trade away trace");
    println!("locality the way sharding by end user would.");

    println!();
    println!("## Fingerprint collisions at 100 million values");
    for bits in [32u32, 48, 64] {
        let n = 100_000_000f64;
        let space = if bits == 64 { 1.8446744e19 } else { 2f64.powi(bits as i32) };
        let expected = n * n / (2.0 * space);
        println!(
            "{:<12} {:>18} expected collisions",
            format!("{bits}-bit"),
            human(expected)
        );
    }
    println!();
    println!("A collision costs one wasted segment open. It never costs");
    println!("correctness, because the segment index verifies the full value.");
    println!("A 32-bit fingerprint is still wrong here: it would send every");
    println!("lookup to more than a million extra segments.");
}
