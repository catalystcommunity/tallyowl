//! D17 prototype: native column page encodings, compression, and sizing.
//!
//! Measures the encodings that SEGMENT_FORMAT.md section 6 lists, against
//! column shapes that real telemetry produces. Produces the numbers that D17
//! needs before the segment format becomes stable.
//!
//! This is decision-support code. It is not product code.

use std::time::Instant;

// ---------------------------------------------------------------------------
// Deterministic generator. One seed gives one identical run, so a benchmark
// result is reproducible. See TESTBED.md section 10.
// ---------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }
    fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Zipf-like skew: a few values dominate, a long tail stays unique. This is
    /// the shape that an actor ID and a session ID actually have.
    fn zipf(&mut self, n: u64) -> u64 {
        let r = self.next() % 1000;
        if r < 500 {
            self.below(n / 100 + 1)
        } else if r < 900 {
            self.below(n / 10 + 1)
        } else {
            self.below(n)
        }
    }
}

// ---------------------------------------------------------------------------
// Column shapes
// ---------------------------------------------------------------------------

enum Column {
    Ints(Vec<u64>),
    Text(Vec<String>),
    Floats(Vec<f64>),
    /// Fixed-width binary IDs, `width` bytes each, packed end to end. This is
    /// what common.csil now stores for every ID.
    Bin { data: Vec<u8>, width: usize },
}

impl Column {
    fn raw_bytes(&self) -> usize {
        match self {
            Column::Ints(v) => v.len() * 8,
            Column::Floats(v) => v.len() * 8,
            Column::Text(v) => v.iter().map(|s| s.len() + 4).sum(),
            Column::Bin { data, .. } => data.len(),
        }
    }
}

fn build_columns(rows: usize, seed: u64) -> Vec<(&'static str, Column)> {
    let mut r = Rng::new(seed);
    let mut out: Vec<(&'static str, Column)> = Vec::new();

    // occurred_at: near-monotonic milliseconds with jitter.
    let mut t: u64 = 1_800_000_000_000;
    let mut ts = Vec::with_capacity(rows);
    for _ in 0..rows {
        t += r.below(40);
        ts.push(t);
    }
    out.push(("occurred_at (ms, near-sorted)", Column::Ints(ts)));

    // duration_ms: skewed small values with a heavy tail.
    let mut dur = Vec::with_capacity(rows);
    for _ in 0..rows {
        let v = match r.below(100) {
            0..=79 => r.below(50),
            80..=97 => r.below(1_000),
            _ => r.below(60_000),
        };
        dur.push(v);
    }
    out.push(("duration_ms (skewed int)", Column::Ints(dur)));

    // service_name: low cardinality. The dictionary case.
    let services: Vec<String> = (0..20).map(|i| format!("svc-{i:02}")).collect();
    let mut sv = Vec::with_capacity(rows);
    for _ in 0..rows {
        sv.push(services[r.below(20) as usize].clone());
    }
    out.push(("service_name (20 distinct)", Column::Text(sv)));

    // route: medium cardinality.
    let routes: Vec<String> = (0..500)
        .map(|i| format!("/api/v1/resource/{}/detail", i))
        .collect();
    let mut rt = Vec::with_capacity(rows);
    for _ in 0..rows {
        rt.push(routes[r.below(500) as usize].clone());
    }
    out.push(("route (500 distinct)", Column::Text(rt)));

    // event_id: UUIDv7 as 16 raw bytes, unique on every row. The hard case.
    let mut ev = Vec::with_capacity(rows * 16);
    for i in 0..rows {
        let hi = 1_800_000_000_000u64 + i as u64;
        ev.extend_from_slice(&hi.to_be_bytes());
        ev.extend_from_slice(&r.next().to_be_bytes());
    }
    out.push(("event_id (unique UUIDv7 bytes)", Column::Bin { data: ev, width: 16 }));

    // trace_id: 16 raw bytes, shared by roughly 10 spans.
    let traces: Vec<[u8; 16]> = (0..(rows / 10 + 1))
        .map(|_| {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&r.next().to_be_bytes());
            b[8..16].copy_from_slice(&r.next().to_be_bytes());
            b
        })
        .collect();
    let mut tr = Vec::with_capacity(rows * 16);
    for i in 0..rows {
        tr.extend_from_slice(&traces[i / 10]);
    }
    out.push(("trace_id (10 spans each, bytes)", Column::Bin { data: tr, width: 16 }));

    // actor_id: Zipf-distributed high cardinality.
    let actors: Vec<String> = (0..(rows / 4 + 1)).map(|i| format!("u_{i:012x}")).collect();
    let n = actors.len() as u64;
    let mut ac = Vec::with_capacity(rows);
    for _ in 0..rows {
        ac.push(actors[r.zipf(n) as usize].clone());
    }
    out.push(("actor_id (Zipf)", Column::Text(ac)));

    // a float measure
    let mut fl = Vec::with_capacity(rows);
    for _ in 0..rows {
        fl.push((r.below(1_000_000) as f64) / 100.0);
    }
    out.push(("measure (float)", Column::Floats(fl)));

    out
}

// ---------------------------------------------------------------------------
// Encodings from SEGMENT_FORMAT.md section 6
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn enc_plain_fixed_u64(v: &[u64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * 8);
    for x in v {
        o.extend_from_slice(&x.to_le_bytes());
    }
    o
}

fn enc_plain_fixed_f64(v: &[f64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * 8);
    for x in v {
        o.extend_from_slice(&x.to_le_bytes());
    }
    o
}

fn enc_varint(v: &[u64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * 2);
    for x in v {
        put_varint(&mut o, *x);
    }
    o
}

fn enc_varint_delta(v: &[u64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * 2);
    let mut prev = 0u64;
    for x in v {
        // zigzag so a negative delta stays small
        let d = (*x as i64).wrapping_sub(prev as i64);
        put_varint(&mut o, ((d << 1) ^ (d >> 63)) as u64);
        prev = *x;
    }
    o
}

fn enc_offset_bytes(v: &[String]) -> Vec<u8> {
    let mut offsets = Vec::with_capacity(v.len() * 4);
    let mut data = Vec::new();
    for s in v {
        put_varint(&mut offsets, data.len() as u64);
        data.extend_from_slice(s.as_bytes());
    }
    let mut o = Vec::with_capacity(offsets.len() + data.len() + 8);
    put_varint(&mut o, offsets.len() as u64);
    o.extend_from_slice(&offsets);
    o.extend_from_slice(&data);
    o
}

/// Split a fixed-width ID column into a leading part and a trailing part. A
/// UUIDv7 carries a time prefix that delta-encodes to almost nothing, while the
/// random tail does not compress at all. Keeping them apart lets the prefix win.
fn enc_split_prefix(data: &[u8], width: usize, prefix: usize) -> Vec<u8> {
    let n = data.len() / width;
    let mut heads = Vec::with_capacity(n);
    let mut tails = Vec::with_capacity(n * (width - prefix));
    for c in data.chunks(width) {
        let mut h = [0u8; 8];
        h[8 - prefix..].copy_from_slice(&c[..prefix]);
        heads.push(u64::from_be_bytes(h));
        tails.extend_from_slice(&c[prefix..]);
    }
    let mut o = enc_varint_delta(&heads);
    o.extend_from_slice(&tails);
    o
}

/// Dictionary encoding. Returns None when the dictionary does not measurably
/// reduce the page, which is the rule SEGMENT_FORMAT.md states.
fn enc_dictionary(v: &[String]) -> Option<Vec<u8>> {
    let mut dict: Vec<&str> = Vec::new();
    let mut index = std::collections::HashMap::new();
    let mut codes = Vec::with_capacity(v.len());
    for s in v {
        let next = dict.len() as u64;
        let code = *index.entry(s.as_str()).or_insert_with(|| {
            dict.push(s.as_str());
            next
        });
        codes.push(code);
    }
    // A dictionary that holds almost every row is not a dictionary.
    if dict.len() * 2 > v.len() {
        return None;
    }
    let mut o = Vec::new();
    put_varint(&mut o, dict.len() as u64);
    for d in &dict {
        put_varint(&mut o, d.len() as u64);
        o.extend_from_slice(d.as_bytes());
    }
    for c in &codes {
        put_varint(&mut o, *c);
    }
    Some(o)
}

// ---------------------------------------------------------------------------
// Measurement
// ---------------------------------------------------------------------------

struct Row {
    column: String,
    encoding: &'static str,
    raw: usize,
    encoded: usize,
    z1: usize,
    z3: usize,
    encode_mbps: f64,
    decompress_mbps: f64,
}

fn measure(column: &str, encoding: &'static str, raw: usize, encode: impl Fn() -> Vec<u8>) -> Row {
    // Encode throughput, averaged over enough passes to be stable.
    let passes = 3;
    let t0 = Instant::now();
    let mut encoded = Vec::new();
    for _ in 0..passes {
        encoded = encode();
    }
    let enc_s = t0.elapsed().as_secs_f64() / passes as f64;

    let z1 = zstd::encode_all(&encoded[..], 1).unwrap();

    let z3 = zstd::encode_all(&encoded[..], 3).unwrap();

    let t2 = Instant::now();
    let back = zstd::decode_all(&z1[..]).unwrap();
    let dec_s = t2.elapsed().as_secs_f64();
    assert_eq!(back.len(), encoded.len());

    let mb = raw as f64 / (1024.0 * 1024.0);
    Row {
        column: column.to_string(),
        encoding,
        raw,
        encoded: encoded.len(),
        z1: z1.len(),
        z3: z3.len(),
        encode_mbps: mb / enc_s,
        decompress_mbps: (encoded.len() as f64 / (1024.0 * 1024.0)) / dec_s,
    }
}

fn pct(part: usize, whole: usize) -> f64 {
    100.0 * part as f64 / whole as f64
}

/// Signed change from `a` to `b`. Level 3 is sometimes LARGER than level 1 on
/// already-encoded columns, so this must not be unsigned.
fn change(a: usize, b: usize) -> f64 {
    100.0 * (a as f64 - b as f64) / a as f64
}

fn main() {
    let rows: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let seed: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(42);

    println!("# D17 page encoding benchmark");
    println!("rows={rows} seed={seed}");
    println!();

    let cols = build_columns(rows, seed);
    let mut results = Vec::new();

    for (name, col) in &cols {
        let raw = col.raw_bytes();
        match col {
            Column::Ints(v) => {
                results.push(measure(name, "plain-fixed", raw, || enc_plain_fixed_u64(v)));
                results.push(measure(name, "varint", raw, || enc_varint(v)));
                results.push(measure(name, "varint-delta", raw, || enc_varint_delta(v)));
            }
            Column::Floats(v) => {
                results.push(measure(name, "plain-fixed", raw, || enc_plain_fixed_f64(v)));
            }
            Column::Bin { data, width } => {
                results.push(measure(name, "plain-fixed", raw, || data.clone()));
                results.push(measure(name, "split-prefix", raw, || {
                    enc_split_prefix(data, *width, 8)
                }));
            }
            Column::Text(v) => {
                results.push(measure(name, "offset-bytes", raw, || enc_offset_bytes(v)));
                if enc_dictionary(v).is_some() {
                    results.push(measure(name, "dictionary", raw, || {
                        enc_dictionary(v).unwrap()
                    }));
                } else {
                    println!(
                        "note: dictionary refused for {name} (distinct count too high)"
                    );
                }
            }
        }
    }

    println!();
    println!(
        "{:<30} {:<13} {:>9} {:>9} {:>7} {:>9} {:>7} {:>9} {:>9} {:>9}",
        "column", "encoding", "raw KiB", "enc KiB", "enc%", "zstd1 KiB", "z1%", "zstd3 KiB",
        "enc MB/s", "dec MB/s"
    );
    for r in &results {
        println!(
            "{:<30} {:<13} {:>9} {:>9} {:>6.1}% {:>9} {:>6.1}% {:>9} {:>9.0} {:>9.0}",
            r.column,
            r.encoding,
            r.raw / 1024,
            r.encoded / 1024,
            pct(r.encoded, r.raw),
            r.z1 / 1024,
            pct(r.z1, r.raw),
            r.z3 / 1024,
            r.encode_mbps,
            r.decompress_mbps
        );
    }

    // Best encoding per column, and the zstd level question.
    println!();
    println!("## Best encoding for each column");
    println!(
        "{:<30} {:<13} {:>10} {:>12} {:>12}",
        "column", "winner", "z1 ratio", "z3 gain", "z3 worth it?"
    );
    let mut total_z1 = 0usize;
    let mut total_z3 = 0usize;
    let mut total_raw = 0usize;
    for (name, _) in &cols {
        let best = results
            .iter()
            .filter(|r| &r.column == name)
            .min_by_key(|r| r.z1)
            .unwrap();
        let gain = change(best.z1, best.z3);
        total_z1 += best.z1;
        total_z3 += best.z3;
        total_raw += best.raw;
        println!(
            "{:<30} {:<13} {:>9.2}x {:>+11.1}% {:>12}",
            name,
            best.encoding,
            best.raw as f64 / best.z1 as f64,
            gain,
            if gain >= 5.0 { "yes" } else { "no" }
        );
    }

    println!();
    println!("## Totals with the winning encoding for each column");
    println!("raw          {:>10} KiB", total_raw / 1024);
    println!(
        "zstd level 1 {:>10} KiB   ratio {:.2}x",
        total_z1 / 1024,
        total_raw as f64 / total_z1 as f64
    );
    println!(
        "zstd level 3 {:>10} KiB   ratio {:.2}x   further gain {:+.1}%",
        total_z3 / 1024,
        total_raw as f64 / total_z3 as f64,
        change(total_z1, total_z3)
    );

    println!();
    println!("## Page size sweep (D17 target is 64 KiB compressed)");
    println!(
        "{:<30} {:>10} {:>12} {:>12} {:>12}",
        "column (winning encoding)", "page KiB", "zstd1 KiB", "vs whole", "pages"
    );
    for (name, col) in &cols {
        let encoded = match col {
            Column::Ints(v) => enc_varint_delta(v),
            Column::Floats(v) => enc_plain_fixed_f64(v),
            Column::Text(v) => enc_dictionary(v).unwrap_or_else(|| enc_offset_bytes(v)),
            Column::Bin { data, width } => enc_split_prefix(data, *width, 8),
        };
        let whole = zstd::encode_all(&encoded[..], 1).unwrap().len();
        for target in [16usize, 64, 256] {
            let chunk = target * 1024;
            let mut total = 0usize;
            let mut pages = 0usize;
            for part in encoded.chunks(chunk) {
                total += zstd::encode_all(part, 1).unwrap().len();
                pages += 1;
            }
            println!(
                "{:<30} {:>10} {:>12} {:>+11.1}% {:>12}",
                if target == 16 { name.to_string() } else { String::new() },
                target,
                total / 1024,
                change(whole, total),
                pages
            );
        }
    }

    // Checksum cost on the hot path. D44 selects xxHash3 for pages and BLAKE3
    // for identity, so the difference has to be worth the second function.
    println!();
    println!("## Checksum cost (D44)");
    let buf = vec![0x5au8; 64 * 1024 * 1024];
    let t = Instant::now();
    let mut acc = 0u64;
    for chunk in buf.chunks(64 * 1024) {
        acc ^= xxhash_rust::xxh3::xxh3_64(chunk);
    }
    let xx = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let mut h = blake3::Hasher::new();
    h.update(&buf);
    let _ = h.finalize();
    let b3 = t.elapsed().as_secs_f64();
    println!("xxHash3-64 over 64 KiB pages  {:>8.0} MB/s (acc {acc:x})", 64.0 / xx);
    println!("BLAKE3-256 over the whole buf {:>8.0} MB/s", 64.0 / b3);
    println!("ratio {:.1}x", (64.0 / xx) / (64.0 / b3));
}
