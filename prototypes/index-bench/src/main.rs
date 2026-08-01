//! D20 and D25 prototype: exact lookup on high-cardinality values.
//!
//! HIGH_CARDINALITY.md section 9 gives the benchmark matrix. This prototype
//! implements the two segment-local layouts that SEGMENT_FORMAT.md section 7
//! names, measures them against the value distributions that real telemetry
//! produces, and compares the term-postings case with Tantivy as D25 requires.
//!
//! The question that matters: can TallyOwl retrieve a value that is unique on
//! every row without scanning every retained segment, and what does the index
//! cost in bytes and in build time?
//!
//! This is decision-support code. It is not product code.

use std::collections::HashMap;
use std::time::Instant;

// ---------------------------------------------------------------------------
// Deterministic generation
// ---------------------------------------------------------------------------

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
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn zipf(&mut self, n: u64) -> u64 {
        match self.next() % 1000 {
            0..=499 => self.below(n / 100 + 1),
            500..=899 => self.below(n / 10 + 1),
            _ => self.below(n),
        }
    }
}

/// A 16-byte ID, which is what common.csil now stores.
type Id = [u8; 16];

fn uuidv7(r: &mut Rng, ms: u64) -> Id {
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&ms.to_be_bytes());
    b[8..16].copy_from_slice(&r.next().to_be_bytes());
    b
}

struct Dataset {
    name: &'static str,
    values: Vec<Id>,
    distinct: usize,
}

fn datasets(rows: usize, seed: u64) -> Vec<Dataset> {
    let mut r = Rng::new(seed);
    let mut out = Vec::new();

    // 1. One repeated value across every row. The degenerate case.
    let one = uuidv7(&mut r, 1);
    out.push(Dataset {
        name: "single repeated value",
        values: vec![one; rows],
        distinct: 1,
    });

    // 2. One random request ID for each row. The case the design promises.
    let mut uniq = Vec::with_capacity(rows);
    for i in 0..rows {
        uniq.push(uuidv7(&mut r, 1_800_000_000_000 + i as u64));
    }
    out.push(Dataset {
        name: "unique on every row",
        values: uniq,
        distinct: rows,
    });

    // 3. Zipf-distributed actor IDs.
    let pool: Vec<Id> = (0..(rows / 4).max(1)).map(|i| uuidv7(&mut r, i as u64)).collect();
    let n = pool.len() as u64;
    let mut zipf = Vec::with_capacity(rows);
    for _ in 0..rows {
        zipf.push(pool[r.zipf(n) as usize]);
    }
    let d = {
        let mut s = std::collections::HashSet::new();
        for v in &zipf {
            s.insert(*v);
        }
        s.len()
    };
    out.push(Dataset {
        name: "Zipf actor IDs",
        values: zipf,
        distinct: d,
    });

    // 4. Trace IDs shared by roughly ten spans.
    let traces: Vec<Id> = (0..(rows / 10).max(1)).map(|i| uuidv7(&mut r, i as u64)).collect();
    let mut tr = Vec::with_capacity(rows);
    for i in 0..rows {
        tr.push(traces[i / 10]);
    }
    out.push(Dataset {
        name: "trace IDs, 10 spans each",
        values: tr,
        distinct: traces.len(),
    });

    out
}

// ---------------------------------------------------------------------------
// Layout 1: unique-lookup
//
// Sorted fixed-width fingerprints plus row IDs. A fingerprint prunes and never
// decides, so the reader verifies the full value before it returns a row. This
// is the layout for a mostly-unique column.
// ---------------------------------------------------------------------------

struct UniqueLookup {
    /// (fingerprint, row id), sorted by fingerprint.
    entries: Vec<(u64, u32)>,
    /// The full values, for verification after a fingerprint match.
    values: Vec<Id>,
}

impl UniqueLookup {
    fn build(values: &[Id]) -> Self {
        let mut entries: Vec<(u64, u32)> = values
            .iter()
            .enumerate()
            .map(|(i, v)| (xxhash_rust::xxh3::xxh3_64(v), i as u32))
            .collect();
        entries.sort_unstable();
        UniqueLookup {
            entries,
            values: values.to_vec(),
        }
    }

    fn bytes(&self) -> usize {
        self.entries.len() * 12
    }

    /// Returns matching row IDs and the count of fingerprint hits that the full
    /// value check rejected.
    fn lookup(&self, needle: &Id, rows_out: &mut Vec<u32>) -> usize {
        rows_out.clear();
        let fp = xxhash_rust::xxh3::xxh3_64(needle);
        let mut i = self.entries.partition_point(|(f, _)| *f < fp);
        let mut rejected = 0usize;
        while i < self.entries.len() && self.entries[i].0 == fp {
            let row = self.entries[i].1;
            if &self.values[row as usize] == needle {
                rows_out.push(row);
            } else {
                rejected += 1;
            }
            i += 1;
        }
        rejected
    }
}

// ---------------------------------------------------------------------------
// Layout 2: term-postings
//
// A sorted term dictionary plus delta and varint compressed row-ID postings.
// This is the layout for a repeated value.
// ---------------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

struct TermPostings {
    /// Sorted terms, each with an offset into the postings block.
    terms: Vec<(Id, u32, u32)>, // value, postings offset, count
    postings: Vec<u8>,
}

impl TermPostings {
    fn build(values: &[Id]) -> Self {
        let mut map: HashMap<Id, Vec<u32>> = HashMap::new();
        for (i, v) in values.iter().enumerate() {
            map.entry(*v).or_default().push(i as u32);
        }
        let mut terms: Vec<(Id, Vec<u32>)> = map.into_iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));

        let mut postings = Vec::new();
        let mut index = Vec::with_capacity(terms.len());
        for (term, rows) in &terms {
            let off = postings.len() as u32;
            let mut prev = 0u32;
            for r in rows {
                put_varint(&mut postings, (r - prev) as u64);
                prev = *r;
            }
            index.push((*term, off, rows.len() as u32));
        }
        TermPostings {
            terms: index,
            postings,
        }
    }

    fn bytes(&self) -> usize {
        self.terms.len() * 24 + self.postings.len()
    }

    fn lookup(&self, needle: &Id) -> usize {
        match self.terms.binary_search_by(|(t, _, _)| t.cmp(needle)) {
            Ok(i) => self.terms[i].2 as usize,
            Err(_) => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Measurement helpers
// ---------------------------------------------------------------------------

fn percentiles(mut v: Vec<u64>) -> (u64, u64, u64) {
    v.sort_unstable();
    let p = |q: f64| v[((v.len() as f64 - 1.0) * q) as usize];
    (p(0.50), p(0.95), p(0.99))
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
    let probes: usize = 20_000;

    println!("# D20 and D25 exact index benchmark");
    println!("rows={rows} seed={seed} probes={probes}");
    println!();

    let sets = datasets(rows, seed);
    let mut r = Rng::new(seed ^ 0xabcd);

    println!("## Native layouts");
    println!(
        "{:<26} {:>10} {:>12} {:>10} {:>9} {:>9} {:>9} {:>9}",
        "dataset", "distinct", "layout", "bytes/row", "build M/s", "p50 ns", "p95 ns", "p99 ns"
    );

    let mut summary = Vec::new();

    for ds in &sets {
        // Probe set: half values that exist, half that do not.
        let mut probe_vals: Vec<Id> = Vec::with_capacity(probes);
        for i in 0..probes {
            if i % 2 == 0 {
                let k = (r.next() as usize) % ds.values.len();
                probe_vals.push(ds.values[k]);
            } else {
                let ms = r.next();
                probe_vals.push(uuidv7(&mut r, ms));
            }
        }

        // unique-lookup
        let t = Instant::now();
        let ul = UniqueLookup::build(&ds.values);
        let build_s = t.elapsed().as_secs_f64();
        let mut out = Vec::new();
        let mut lat = Vec::with_capacity(probes);
        let mut rejected_total = 0usize;
        let mut hits = 0usize;
        for p in &probe_vals {
            let t0 = Instant::now();
            rejected_total += ul.lookup(p, &mut out);
            lat.push(t0.elapsed().as_nanos() as u64);
            if !out.is_empty() {
                hits += 1;
            }
        }
        let (p50, p95, p99) = percentiles(lat);
        println!(
            "{:<26} {:>10} {:>12} {:>10.1} {:>9.2} {:>9} {:>9} {:>9}",
            ds.name,
            ds.distinct,
            "unique-lookup",
            ul.bytes() as f64 / rows as f64,
            rows as f64 / build_s / 1e6,
            p50,
            p95,
            p99
        );
        summary.push((
            ds.name,
            "unique-lookup",
            ul.bytes(),
            hits,
            rejected_total,
        ));

        // term-postings
        let t = Instant::now();
        let tp = TermPostings::build(&ds.values);
        let build_s = t.elapsed().as_secs_f64();
        let mut lat = Vec::with_capacity(probes);
        let mut hits2 = 0usize;
        for p in &probe_vals {
            let t0 = Instant::now();
            let n = tp.lookup(p);
            lat.push(t0.elapsed().as_nanos() as u64);
            if n > 0 {
                hits2 += 1;
            }
        }
        let (q50, q95, q99) = percentiles(lat);
        println!(
            "{:<26} {:>10} {:>12} {:>10.1} {:>9.2} {:>9} {:>9} {:>9}",
            "",
            "",
            "term-postings",
            tp.bytes() as f64 / rows as f64,
            rows as f64 / build_s / 1e6,
            q50,
            q95,
            q99
        );
        summary.push((ds.name, "term-postings", tp.bytes(), hits2, 0));
    }

    println!();
    println!("## Index size against the data it indexes");
    println!(
        "{:<26} {:<14} {:>12} {:>14} {:>12}",
        "dataset", "layout", "index KiB", "data KiB (raw)", "overhead"
    );
    let data_kib = rows * 16 / 1024;
    for (name, layout, bytes, _, _) in &summary {
        println!(
            "{:<26} {:<14} {:>12} {:>14} {:>11.0}%",
            name,
            layout,
            bytes / 1024,
            data_kib,
            100.0 * *bytes as f64 / (rows * 16) as f64
        );
    }

    println!();
    println!("## Fingerprint collisions rejected by full-value verification");
    for (name, layout, _, hits, rejected) in &summary {
        if *layout == "unique-lookup" {
            println!(
                "{:<26} hits {:>8}   rejected by verification {:>6}",
                name, hits, rejected
            );
        }
    }

    // -----------------------------------------------------------------------
    // D25: compare the term-postings case with Tantivy.
    // -----------------------------------------------------------------------
    println!();
    println!("## D25: Tantivy comparison");
    tantivy_compare(&sets, probes, seed);
}

fn tantivy_compare(sets: &[Dataset], probes: usize, seed: u64) {
    use tantivy::collector::Count;
    use tantivy::query::TermQuery;
    use tantivy::schema::{Schema, STRING};
    use tantivy::{doc, Index, IndexWriter, Term};

    let mut r = Rng::new(seed ^ 0x1234);

    println!(
        "{:<26} {:>12} {:>12} {:>12} {:>12} {:>12}",
        "dataset", "engine", "build M/s", "index KiB", "p50 ns", "p99 ns"
    );

    for ds in sets {
        // Tantivy indexes text, so the value becomes hexadecimal here. That is
        // part of what the comparison measures: an outside index does not take
        // the native 16-byte form.
        let mut schema_builder = Schema::builder();
        let field = schema_builder.add_text_field("id", STRING);
        let schema = schema_builder.build();

        let dir = tempfile::tempdir().unwrap();
        let index = Index::create_in_dir(dir.path(), schema).unwrap();
        let mut writer: IndexWriter = index.writer(200_000_000).unwrap();

        let t = Instant::now();
        for v in &ds.values {
            let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
            writer.add_document(doc!(field => hex)).unwrap();
        }
        writer.commit().unwrap();
        let build_s = t.elapsed().as_secs_f64();

        let reader = index.reader().unwrap();
        let searcher = reader.searcher();

        let mut size = 0u64;
        for e in std::fs::read_dir(dir.path()).unwrap() {
            let e = e.unwrap();
            if let Ok(m) = e.metadata() {
                size += m.len();
            }
        }

        let mut lat = Vec::with_capacity(probes / 10);
        for i in 0..(probes / 10) {
            let v = if i % 2 == 0 {
                let k = (r.next() as usize) % ds.values.len();
                ds.values[k]
            } else {
                let ms = r.next();
                uuidv7(&mut r, ms)
            };
            let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
            let term = Term::from_field_text(field, &hex);
            let q = TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
            let t0 = Instant::now();
            let _ = searcher.search(&q, &Count).unwrap();
            lat.push(t0.elapsed().as_nanos() as u64);
        }
        let (p50, _p95, p99) = percentiles(lat);

        println!(
            "{:<26} {:>12} {:>12.2} {:>12} {:>12} {:>12}",
            ds.name,
            "tantivy",
            ds.values.len() as f64 / build_s / 1e6,
            size / 1024,
            p50,
            p99
        );
    }
}
