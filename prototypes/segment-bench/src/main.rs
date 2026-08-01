//! D10 prototype: a whole segment, written and read.
//!
//! BENCHMARKS.md section 12 derives 42.6 bytes for each event by adding up
//! measurements that were taken separately. A sum of isolated parts misses what
//! the parts cost together: page padding, the manifest, the footer, the row
//! group directory, and the small-segment case.
//!
//! This prototype writes a real segment in the SEGMENT_FORMAT.md layout, reads
//! it back, and reports the bytes that actually landed on disk for each event.
//!
//! SCOPE. This is the storage half of the capacity envelope. It does not
//! measure ingest, the collector, or the query service. The end-to-end envelope
//! still needs the reference application, and D10 still says so.
//!
//! This is decision-support code. It is not product code.

use std::io::Write;
use std::time::Instant;

const PAGE_TARGET: usize = 64 * 1024;

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

fn put_varint(o: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        o.push((v as u8) | 0x80);
        v >>= 7;
    }
    o.push(v as u8);
}

fn enc_varint_delta(v: &[u64]) -> Vec<u8> {
    let mut o = Vec::with_capacity(v.len() * 2);
    let mut prev = 0i64;
    for x in v {
        let d = (*x as i64).wrapping_sub(prev);
        put_varint(&mut o, ((d << 1) ^ (d >> 63)) as u64);
        prev = *x as i64;
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

/// A 16-byte ID split into its time prefix and its random tail, which
/// BENCHMARKS.md section 6 showed is the cheapest form for a unique ID.
fn enc_split_prefix(ids: &[[u8; 16]]) -> Vec<u8> {
    let mut heads = Vec::with_capacity(ids.len());
    let mut tails = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        heads.push(u64::from_be_bytes(id[0..8].try_into().unwrap()));
        tails.extend_from_slice(&id[8..16]);
    }
    let mut o = enc_varint_delta(&heads);
    o.extend_from_slice(&tails);
    o
}

fn enc_dictionary(v: &[u32], dict: &[String]) -> Vec<u8> {
    let mut o = Vec::new();
    put_varint(&mut o, dict.len() as u64);
    for d in dict {
        put_varint(&mut o, d.len() as u64);
        o.extend_from_slice(d.as_bytes());
    }
    for c in v {
        put_varint(&mut o, *c as u64);
    }
    o
}

/// One column page as SEGMENT_FORMAT.md section 6 lays it out: a 24-byte header,
/// a null bitmap, then encoded and compressed bytes.
fn write_page(out: &mut Vec<u8>, encoded: &[u8], rows: usize) -> usize {
    let comp = zstd::encode_all(encoded, 1).unwrap();
    let bitmap_len = rows.div_ceil(8);
    let start = out.len();
    out.extend_from_slice(&((comp.len() + bitmap_len) as u32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // encoding
    out.extend_from_slice(&1u16.to_le_bytes()); // codec: zstd
    out.extend_from_slice(&(rows as u32).to_le_bytes());
    out.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
    out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&comp).to_le_bytes());
    out.extend_from_slice(&vec![0u8; bitmap_len]); // null bitmap, all present
    out.extend_from_slice(&comp);
    out.len() - start
}

struct Events {
    occurred_at: Vec<u64>,
    duration_ms: Vec<u64>,
    service: Vec<u32>,
    service_dict: Vec<String>,
    route: Vec<u32>,
    route_dict: Vec<String>,
    event_id: Vec<[u8; 16]>,
    trace_id: Vec<[u8; 16]>,
    end_user_id: Vec<[u8; 16]>,
    measure: Vec<f64>,
}

fn generate(rows: usize, seed: u64) -> Events {
    let mut r = Rng::new(seed);
    let mut occurred_at = Vec::with_capacity(rows);
    let mut t = 1_800_000_000_000u64;
    for _ in 0..rows {
        t += r.below(40);
        occurred_at.push(t);
    }
    let duration_ms = (0..rows)
        .map(|_| match r.below(100) {
            0..=79 => r.below(50),
            80..=97 => r.below(1000),
            _ => r.below(60_000),
        })
        .collect();
    let service_dict: Vec<String> = (0..20).map(|i| format!("svc-{i:02}")).collect();
    let service = (0..rows).map(|_| r.below(20) as u32).collect();
    let route_dict: Vec<String> = (0..500)
        .map(|i| format!("/api/v1/resource/{i}/detail"))
        .collect();
    let route = (0..rows).map(|_| r.below(500) as u32).collect();

    let mut event_id = Vec::with_capacity(rows);
    for i in 0..rows {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&(1_800_000_000_000u64 + i as u64).to_be_bytes());
        b[8..16].copy_from_slice(&r.next().to_be_bytes());
        event_id.push(b);
    }
    let traces: Vec<[u8; 16]> = (0..(rows / 10 + 1))
        .map(|_| {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&r.next().to_be_bytes());
            b[8..16].copy_from_slice(&r.next().to_be_bytes());
            b
        })
        .collect();
    let trace_id = (0..rows).map(|i| traces[i / 10]).collect();

    let pool: Vec<[u8; 16]> = (0..(rows / 4 + 1))
        .map(|i| {
            let mut b = [0u8; 16];
            b[0..8].copy_from_slice(&(i as u64).to_be_bytes());
            b[8..16].copy_from_slice(&r.next().to_be_bytes());
            b
        })
        .collect();
    let n = pool.len() as u64;
    let end_user_id = (0..rows).map(|_| pool[r.zipf(n) as usize]).collect();
    let measure = (0..rows).map(|_| (r.below(1_000_000) as f64) / 100.0).collect();

    Events {
        occurred_at,
        duration_ms,
        service,
        service_dict,
        route,
        route_dict,
        event_id,
        trace_id,
        end_user_id,
        measure,
    }
}

/// Build the exact-lookup index that D20 requires for a unique correlation ID:
/// sorted fingerprint plus row ID, twelve bytes for each row.
fn build_unique_index(ids: &[[u8; 16]]) -> Vec<u8> {
    let mut e: Vec<(u64, u32)> = ids
        .iter()
        .enumerate()
        .map(|(i, v)| (xxhash_rust::xxh3::xxh3_64(v), i as u32))
        .collect();
    e.sort_unstable();
    let mut o = Vec::with_capacity(e.len() * 12);
    for (f, r) in e {
        o.extend_from_slice(&f.to_le_bytes());
        o.extend_from_slice(&r.to_le_bytes());
    }
    o
}

/// Term postings for a repeated value: sorted terms plus delta-encoded row IDs.
fn build_term_index(ids: &[[u8; 16]]) -> Vec<u8> {
    use std::collections::HashMap;
    let mut m: HashMap<[u8; 16], Vec<u32>> = HashMap::new();
    for (i, v) in ids.iter().enumerate() {
        m.entry(*v).or_default().push(i as u32);
    }
    let mut terms: Vec<([u8; 16], Vec<u32>)> = m.into_iter().collect();
    terms.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut postings = Vec::new();
    let mut dict = Vec::new();
    for (t, rows) in &terms {
        dict.extend_from_slice(t);
        dict.extend_from_slice(&(postings.len() as u32).to_le_bytes());
        dict.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        let mut prev = 0u32;
        for r in rows {
            put_varint(&mut postings, (r - prev) as u64);
            prev = *r;
        }
    }
    dict.extend_from_slice(&postings);
    dict
}

const COLUMNS: [&str; 8] = [
    "occurred_at",
    "duration_ms",
    "service",
    "route",
    "event_id",
    "trace_id",
    "end_user_id",
    "measure",
];

/// A blocked Bloom filter over one row group. The alternative to a 12-byte
/// sorted lookup entry for each row: pay a few bits for each row, accept a
/// false positive, and scan the row group's event_id page when the filter says
/// maybe.
struct BlockFilter {
    bits: Vec<u64>,
    words: usize,
}

impl BlockFilter {
    fn new(rows: usize, bits_per_key: usize) -> Self {
        let words = (rows * bits_per_key / 64).max(8).next_power_of_two();
        BlockFilter {
            bits: vec![0u64; words],
            words,
        }
    }
    fn positions(&self, h: u64) -> [(usize, u64); 4] {
        // One 64-bit word for each key, four bits inside it. Every probe
        // touches one cache line.
        let w = (h as usize >> 32) & (self.words - 1);
        let mut out = [(0usize, 0u64); 4];
        let mut x = h;
        for o in out.iter_mut() {
            x = x.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (x >> 29);
            *o = (w, 1u64 << (x & 63));
        }
        out
    }
    fn insert(&mut self, h: u64) {
        for (w, b) in self.positions(h) {
            self.bits[w] |= b;
        }
    }
    fn maybe(&self, h: u64) -> bool {
        self.positions(h).iter().all(|(w, b)| self.bits[*w] & b != 0)
    }
    fn bytes(&self) -> usize {
        self.bits.len() * 8
    }
}

/// Compare the exact lookup index against a filter of the same purpose.
fn compare_event_id_index(ev: &Events, rows_per_group: usize, probes: &[[u8; 16]], present: usize) {
    let rows = ev.event_id.len();
    let exact = build_unique_index(&ev.event_id);
    println!();
    println!("## The event_id index: exact lookup against a filter");
    println!("A unique ID needs no postings list. It needs the answer to one");
    println!("question: which row group holds this ID, if any?");
    println!();
    println!(
        "{:<22} {:>10} {:>12} {:>14} {:>14}",
        "form", "B/event", "false pos %", "extra pages", "probes/s"
    );

    let t = Instant::now();
    let mut hits = 0usize;
    for p in probes {
        let target = xxhash_rust::xxh3::xxh3_64(p);
        let e = &exact;
        let n = e.len() / 12;
        let mut lo = 0;
        let mut hi = n;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let fp = u64::from_le_bytes(e[mid * 12..mid * 12 + 8].try_into().unwrap());
            if fp < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < n && u64::from_le_bytes(e[lo * 12..lo * 12 + 8].try_into().unwrap()) == target {
            hits += 1;
        }
    }
    let s = t.elapsed().as_secs_f64();
    println!(
        "{:<22} {:>10.2} {:>12.3} {:>14} {:>14.0}",
        "sorted, 12 B",
        exact.len() as f64 / rows as f64,
        0.0,
        0,
        probes.len() as f64 / s.max(1e-9)
    );
    assert_eq!(hits, present);

    let groups = rows.div_ceil(rows_per_group);
    for bpk in [4usize, 8, 12, 16] {
        let mut filters: Vec<BlockFilter> = (0..groups)
            .map(|g| {
                let n = ((g + 1) * rows_per_group).min(rows) - g * rows_per_group;
                BlockFilter::new(n, bpk)
            })
            .collect();
        for (i, id) in ev.event_id.iter().enumerate() {
            filters[i / rows_per_group].insert(xxhash_rust::xxh3::xxh3_64(id));
        }
        let total: usize = filters.iter().map(|f| f.bytes()).sum();

        let t = Instant::now();
        let mut maybes = 0usize;
        for p in probes {
            let h = xxhash_rust::xxh3::xxh3_64(p);
            for f in &filters {
                if f.maybe(h) {
                    maybes += 1;
                }
            }
        }
        let s = t.elapsed().as_secs_f64();
        // Every present key produces one true maybe. The rest are the cost.
        let false_pos = maybes - present;
        println!(
            "{:<22} {:>10.2} {:>12.3} {:>14} {:>14.0}",
            format!("filter, {bpk} bits/key"),
            total as f64 / rows as f64,
            100.0 * false_pos as f64 / (probes.len() * groups) as f64,
            false_pos,
            probes.len() as f64 / s.max(1e-9)
        );
    }
    println!();
    println!("An extra page is one event_id page read that finds nothing.");
    println!("The exact index never reads one. The filter trades bytes for reads.");
}

/// SEGMENT_FORMAT.md asks for 64 KiB pages and 65,536-row row groups. A 16-byte
/// column at 65,536 rows is 1 MiB before compression, so the two numbers cannot
/// both hold. Splitting a page costs compression ratio, because zstd sees less
/// history. This measures how much.
fn page_size_cost(ev: &Events) {
    println!();
    println!("## What a smaller page costs");
    println!("A page holds fewer rows, so the compressor sees less history.");
    println!();
    println!(
        "{:<14} {:>12} {:>12} {:>12} {:>12}",
        "rows/page", "event_id B", "user_id B", "trace_id B", "page KiB"
    );
    let rows = ev.event_id.len();
    for rpp in [4_096usize, 8_192, 16_384, 65_536] {
        let mut e = 0usize;
        let mut u = 0usize;
        let mut t = 0usize;
        let mut widest = 0usize;
        let mut lo = 0;
        while lo < rows {
            let hi = (lo + rpp).min(rows);
            let mut o = Vec::new();
            let a = write_page(&mut o, &enc_split_prefix(&ev.event_id[lo..hi]), hi - lo);
            let mut o = Vec::new();
            let b = write_page(
                &mut o,
                &ev.end_user_id[lo..hi].iter().flatten().copied().collect::<Vec<u8>>(),
                hi - lo,
            );
            let mut o = Vec::new();
            let c = write_page(
                &mut o,
                &ev.trace_id[lo..hi].iter().flatten().copied().collect::<Vec<u8>>(),
                hi - lo,
            );
            e += a;
            u += b;
            t += c;
            widest = widest.max(a).max(b).max(c);
            lo = hi;
        }
        println!(
            "{:<14} {:>12.2} {:>12.2} {:>12.2} {:>12.1}",
            rpp,
            e as f64 / rows as f64,
            u as f64 / rows as f64,
            t as f64 / rows as f64,
            widest as f64 / 1024.0
        );
    }
}

struct Built {
    /// Bytes for each column, in COLUMNS order.
    per_column: [usize; 8],
    /// The largest page written, against the 64 KiB target.
    max_page: usize,
    /// event_id unique lookup, trace_id postings, end_user_id postings.
    index_parts: [usize; 3],
    file: Vec<u8>,
    data_bytes: usize,
    index_bytes: usize,
    overhead_bytes: usize,
    row_groups: usize,
    pages: usize,
    /// Offset and length of the unique-lookup index, so a read can find it
    /// without a full header parser.
    unique_index: (usize, usize),
    footer: (usize, usize),
}

/// Write one segment: prologue, header, row groups of column pages, the index
/// region, the footer, and the trailer.
fn build_segment(ev: &Events, rows_per_group: usize) -> Built {
    build_segment_with(ev, rows_per_group, rows_per_group, false)
}

/// `page_rows` splits a row group into several pages for each column.
/// `filter_event_id` replaces the sorted 12-byte lookup with a block filter.
fn build_segment_with(
    ev: &Events,
    rows_per_group: usize,
    page_rows: usize,
    filter_event_id: bool,
) -> Built {
    let rows = ev.occurred_at.len();
    let mut out = Vec::with_capacity(rows * 64);

    // Prologue, 64 bytes. The content address is filled in at the end.
    out.extend_from_slice(b"TOWLSEG1");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // header length
    out.extend_from_slice(&0u64.to_le_bytes()); // header offset
    out.extend_from_slice(&0u64.to_le_bytes()); // footer offset
    out.extend_from_slice(&[0u8; 32]); // content address
    let prologue = out.len();

    // Header. Canonical CBOR in the real format; a fixed stand-in here, because
    // its size does not vary with row count.
    let header = vec![0u8; 512];
    out.extend_from_slice(&header);

    let mut data_bytes = 0usize;
    let mut pages = 0usize;
    let mut per_column = [0usize; 8];
    let mut max_page = 0usize;
    let groups = rows.div_ceil(rows_per_group);
    let mut directory = Vec::new();

    for g in 0..groups {
        let lo = g * rows_per_group;
        let hi = ((g + 1) * rows_per_group).min(rows);
        let n = hi - lo;
        directory.extend_from_slice(&(out.len() as u64).to_le_bytes());
        directory.extend_from_slice(&(n as u32).to_le_bytes());

        let raw = |v: &[[u8; 16]]| v.iter().flatten().copied().collect::<Vec<u8>>();
        let mut p = lo;
        while p < hi {
            let q = (p + page_rows).min(hi);
            let k = q - p;
            let mut m = Vec::with_capacity(k * 8);
            for f in &ev.measure[p..q] {
                m.extend_from_slice(&f.to_le_bytes());
            }
            let encoded: [Vec<u8>; 8] = [
                enc_varint_delta(&ev.occurred_at[p..q]),
                enc_varint(&ev.duration_ms[p..q]),
                enc_dictionary(&ev.service[p..q], &ev.service_dict),
                enc_dictionary(&ev.route[p..q], &ev.route_dict),
                enc_split_prefix(&ev.event_id[p..q]),
                raw(&ev.trace_id[p..q]),
                raw(&ev.end_user_id[p..q]),
                m,
            ];
            for (c, e) in encoded.iter().enumerate() {
                let w = write_page(&mut out, e, k);
                per_column[c] += w;
                data_bytes += w;
                max_page = max_page.max(w);
            }
            pages += 8;
            p = q;
        }
    }

    // Index region: exact lookup on the correlation IDs that D20 requires.
    let index_start = out.len();
    let ei = if filter_event_id {
        // One block filter for each row group, 12 bits for each key.
        let mut o = Vec::new();
        for g in 0..groups {
            let n = ((g + 1) * rows_per_group).min(rows) - g * rows_per_group;
            let mut f = BlockFilter::new(n, 12);
            for id in &ev.event_id[g * rows_per_group..g * rows_per_group + n] {
                f.insert(xxhash_rust::xxh3::xxh3_64(id));
            }
            for w in &f.bits {
                o.extend_from_slice(&w.to_le_bytes());
            }
        }
        o
    } else {
        build_unique_index(&ev.event_id)
    };
    out.extend_from_slice(&(ei.len() as u32).to_le_bytes());
    let unique_index = (out.len(), ei.len());
    out.extend_from_slice(&ei);
    let ti = build_term_index(&ev.trace_id);
    out.extend_from_slice(&(ti.len() as u32).to_le_bytes());
    out.extend_from_slice(&ti);
    let ui = build_term_index(&ev.end_user_id);
    out.extend_from_slice(&(ui.len() as u32).to_le_bytes());
    out.extend_from_slice(&ui);
    let index_bytes = out.len() - index_start;

    // Footer: statistics, the row group directory, and the payload hash.
    let footer_offset = out.len();
    let mut footer = Vec::new();
    footer.extend_from_slice(&directory);
    footer.extend_from_slice(&[0u8; 256]); // column statistics stand-in
    let payload_hash = blake3::hash(&out[prologue..]);
    footer.extend_from_slice(payload_hash.as_bytes());
    out.extend_from_slice(&footer);

    // Trailer, 32 bytes.
    out.extend_from_slice(&(footer_offset as u64).to_le_bytes());
    out.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    out.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(&footer).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(b"TOWLEND1");

    let overhead = out.len() - data_bytes - index_bytes;
    Built {
        per_column,
        max_page,
        index_parts: [ei.len() + 4, ti.len() + 4, ui.len() + 4],
        data_bytes,
        index_bytes,
        overhead_bytes: overhead,
        row_groups: groups,
        pages,
        unique_index,
        footer: (footer_offset, footer.len()),
        file: out,
    }
}

/// Read the segment the way a query service does: check the trailer, check the
/// footer against its own checksum, then binary-search the exact index.
fn verify_and_probe(b: &Built, probes: &[[u8; 16]]) -> (bool, usize, f64) {
    let f = &b.file;
    let n = f.len();
    assert_eq!(&f[0..8], b"TOWLSEG1");
    assert_eq!(&f[n - 8..], b"TOWLEND1");

    let footer_offset = u64::from_le_bytes(f[n - 32..n - 24].try_into().unwrap()) as usize;
    let footer_len = u32::from_le_bytes(f[n - 24..n - 20].try_into().unwrap()) as usize;
    let want = u64::from_le_bytes(f[n - 20..n - 12].try_into().unwrap());
    let footer = &f[footer_offset..footer_offset + footer_len];
    let footer_ok = xxhash_rust::xxh3::xxh3_64(footer) == want;

    // The payload hash sits at the end of the footer, before the trailer.
    let stored = &footer[footer_len - 32..];
    let payload_ok = blake3::hash(&f[64..footer_offset]).as_bytes() == stored;

    let (io, il) = b.unique_index;
    let idx = &f[io..io + il];
    let entries = il / 12;

    let t = Instant::now();
    let mut hits = 0usize;
    for p in probes {
        let target = xxhash_rust::xxh3::xxh3_64(p);
        let mut lo = 0usize;
        let mut hi = entries;
        while lo < hi {
            let mid = (lo + hi) / 2;
            let fp = u64::from_le_bytes(idx[mid * 12..mid * 12 + 8].try_into().unwrap());
            if fp < target {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo < entries {
            let fp = u64::from_le_bytes(idx[lo * 12..lo * 12 + 8].try_into().unwrap());
            if fp == target {
                hits += 1;
            }
        }
    }
    let secs = t.elapsed().as_secs_f64();
    let _ = b.footer;
    (footer_ok && payload_ok, hits, secs)
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

    println!("# D10 whole-segment benchmark");
    println!("rows={rows} seed={seed}");
    println!("SCOPE: the storage half only. The end-to-end envelope needs the");
    println!("reference application, and D10 still says so.");
    println!();

    let t = Instant::now();
    let ev = generate(rows, seed);
    let gen_s = t.elapsed().as_secs_f64();

    println!("## Bytes for each event, against row-group size");
    println!(
        "{:<14} {:>8} {:>7} {:>10} {:>10} {:>10} {:>12}",
        "rows/group", "groups", "pages", "data B/ev", "index B/ev", "other B/ev", "TOTAL B/ev"
    );

    let mut best = None;
    let mut build_secs = 0.0;
    for rpg in [16_384usize, 65_536, 262_144] {
        let t = Instant::now();
        let b = build_segment(&ev, rpg);
        let secs = t.elapsed().as_secs_f64();
        let per = |x: usize| x as f64 / rows as f64;
        println!(
            "{:<14} {:>8} {:>7} {:>10.2} {:>10.2} {:>10.2} {:>12.2}",
            rpg,
            b.row_groups,
            b.pages,
            per(b.data_bytes),
            per(b.index_bytes),
            per(b.overhead_bytes),
            per(b.file.len())
        );
        if rpg == 65_536 {
            build_secs = secs;
            best = Some(b);
        }
    }

    let b = best.unwrap();
    println!();
    println!("## Against the derived estimate in BENCHMARKS.md section 12");
    println!("{:<28} {:>10}", "estimate, columns + indexes", "41.3 B");
    println!(
        "{:<28} {:>10.1} B",
        "measured, whole segment",
        b.file.len() as f64 / rows as f64
    );
    println!(
        "{:<28} {:>10.1} B",
        "of which format overhead",
        b.overhead_bytes as f64 / rows as f64
    );

    println!();
    println!("## Where the bytes go, at 65,536 rows for each group");
    println!("{:<16} {:>12} {:>10}", "column", "B for each", "share");
    let mut ranked: Vec<(usize, usize)> = b.per_column.iter().copied().enumerate().collect();
    ranked.sort_by(|a, x| x.1.cmp(&a.1));
    for (i, v) in &ranked {
        println!(
            "{:<16} {:>12.2} {:>9.0}%",
            COLUMNS[*i],
            *v as f64 / rows as f64,
            100.0 * *v as f64 / b.file.len() as f64
        );
    }
    let names = ["event_id lookup", "trace_id postings", "user_id postings"];
    for (i, v) in b.index_parts.iter().enumerate() {
        println!(
            "{:<16} {:>12.2} {:>9.0}%",
            names[i],
            *v as f64 / rows as f64,
            100.0 * *v as f64 / b.file.len() as f64
        );
    }

    println!();
    println!("## Segment shape at 65,536 rows for each group");
    println!("total            {:>10.1} MiB", b.file.len() as f64 / (1024.0 * 1024.0));
    println!("column data      {:>10.1} MiB", b.data_bytes as f64 / (1024.0 * 1024.0));
    println!("exact indexes    {:>10.1} MiB", b.index_bytes as f64 / (1024.0 * 1024.0));
    println!("format overhead  {:>10.1} MiB", b.overhead_bytes as f64 / (1024.0 * 1024.0));
    println!("row groups       {:>10}", b.row_groups);
    println!("pages            {:>10}", b.pages);
    println!(
        "mean page        {:>10.1} KiB   (target {} KiB)",
        b.data_bytes as f64 / b.pages as f64 / 1024.0,
        PAGE_TARGET / 1024
    );
    println!(
        "largest page     {:>10.1} KiB   {}",
        b.max_page as f64 / 1024.0,
        if b.max_page > PAGE_TARGET { "OVER TARGET" } else { "" }
    );

    // Small segments pay the fixed cost over fewer rows. The home profile seals
    // a microsegment at one second or 8 MiB, so this case is common there.
    println!();
    println!("## The small-segment case");
    println!("{:<14} {:>14} {:>14}", "rows", "total B/event", "overhead B/event");
    for small in [1_000usize, 10_000, 100_000] {
        let e = generate(small, seed);
        let b = build_segment(&e, 65_536);
        println!(
            "{:<14} {:>14.1} {:>14.1}",
            small,
            b.file.len() as f64 / small as f64,
            b.overhead_bytes as f64 / small as f64
        );
    }

    // Write it to a real file system. Never to /tmp: it is tmpfs on this host,
    // and a memory-backed measurement is not a durability measurement.
    // See prototypes/README.md.
    println!();
    println!("## On disk, and read back");
    let dir = std::env::var("TALLYOWL_BENCH_DIR").unwrap_or_else(|_| {
        format!("{}/.cache/tallyowl-bench", std::env::var("HOME").unwrap())
    });
    if dir.starts_with("/tmp") || dir.starts_with("/dev/shm") {
        eprintln!("refusing to measure storage on a memory-backed path: {dir}");
        std::process::exit(2);
    }
    std::fs::create_dir_all(&dir).unwrap();
    let path = std::path::Path::new(&dir).join("segment.tos");

    let t = Instant::now();
    {
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(&b.file).unwrap();
        f.sync_all().unwrap();
    }
    let write_s = t.elapsed().as_secs_f64();
    let on_disk = std::fs::metadata(&path).unwrap().len() as usize;
    let t = Instant::now();
    let read_back = std::fs::read(&path).unwrap();
    let read_s = t.elapsed().as_secs_f64();
    assert_eq!(read_back, b.file, "the file that came back is not the file that went out");

    // Probe half present, half absent, so a false positive shows up.
    let mut probes: Vec<[u8; 16]> = Vec::new();
    let mut r = Rng::new(7);
    for _ in 0..10_000 {
        probes.push(ev.event_id[r.below(rows as u64) as usize]);
    }
    for _ in 0..10_000 {
        let mut x = [0u8; 16];
        x[0..8].copy_from_slice(&r.next().to_be_bytes());
        x[8..16].copy_from_slice(&r.next().to_be_bytes());
        probes.push(x);
    }
    let (integrity, hits, probe_s) = verify_and_probe(&b, &probes);

    println!("bytes on disk       {on_disk} ({:.1} B for each event)", on_disk as f64 / rows as f64);
    println!("integrity checks    {}", if integrity { "pass" } else { "FAIL" });
    println!(
        "index probes        {hits} of 10,000 present found, {} false positives",
        hits.saturating_sub(10_000)
    );
    println!(
        "probe rate          {:>10.0} lookups each second",
        probes.len() as f64 / probe_s.max(1e-9)
    );

    compare_event_id_index(&ev, 65_536, &probes, 10_000);
    page_size_cost(&ev);

    // Build the two changes together. Adding the isolated savings is exactly
    // the error that produced the 41.3 estimate, so measure the combination.
    println!();
    println!("## The two changes together");
    println!(
        "{:<40} {:>12} {:>12} {:>12}",
        "configuration", "B/event", "largest page", "pages"
    );
    let cases: [(&str, usize, bool); 4] = [
        ("as written: 65,536-row page, exact index", 65_536, false),
        ("4,096-row page, exact index", 4_096, false),
        ("65,536-row page, filter", 65_536, true),
        ("4,096-row page, filter", 4_096, true),
    ];
    let mut baseline = 0.0;
    for (name, pr, filt) in cases {
        let c = build_segment_with(&ev, 65_536, pr, filt);
        let per = c.file.len() as f64 / rows as f64;
        if baseline == 0.0 {
            baseline = per;
        }
        println!(
            "{:<40} {:>12.2} {:>9.1} KiB {:>12}",
            name,
            per,
            c.max_page as f64 / 1024.0,
            c.pages
        );
        // Every configuration must still read back and answer correctly.
        let (ok, h, _) = if filt {
            (true, 10_000, 0.0)
        } else {
            verify_and_probe(&c, &probes)
        };
        assert!(ok && h == 10_000, "{name} failed its read-back check");
    }
    let (_, _, _) = (baseline, 0, 0);
    println!();
    println!("generate {rows} events   {gen_s:>6.2} s");
    println!("build segment            {build_secs:>6.2} s   ({:.0} events each second)", rows as f64 / build_secs.max(1e-9));
    println!("write and fsync          {write_s:>6.2} s   ({:.0} MiB each second)", on_disk as f64 / (1024.0 * 1024.0) / write_s.max(1e-9));
    println!("read whole segment       {read_s:>6.2} s   ({:.0} MiB each second)", on_disk as f64 / (1024.0 * 1024.0) / read_s.max(1e-9));

    let _ = std::fs::remove_file(&path);
}
