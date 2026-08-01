//! WAL group commit, from STORAGE.md section 15 item 2.
//!
//! The fsync ceiling measured for D3 is the hard limit on this hardware: about
//! 186 each second. STORAGE.md section 3.1 answers it with bounded group
//! commit, where one fsync makes many complete batch frames durable while each
//! caller still receives only its own receipt.
//!
//! That is the mitigation for the most important number in the whole
//! measurement set, so it needs its own test. This benchmark implements the
//! append path both ways and measures the difference.
//!
//! This is decision-support code. It is not product code.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// A checksummed, length-delimited frame, as STORAGE.md section 3.1 describes.
fn frame(payload: &[u8], position: u64) -> Vec<u8> {
    let mut f = Vec::with_capacity(payload.len() + 24);
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(&position.to_le_bytes());
    f.extend_from_slice(&xxhash_rust::xxh3::xxh3_64(payload).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

struct Shared {
    file: File,
    /// Bytes accepted but not yet made durable.
    pending: Vec<u8>,
    pending_frames: u64,
    /// Byte offset where the next write lands.
    write_offset: u64,
    /// Every byte below this offset is durable.
    durable_upto: u64,
    committing: bool,
    fsyncs: u64,
    max_group: u64,
    group_sum: u64,
}

struct Wal {
    m: Mutex<Shared>,
    cv: Condvar,
}

impl Wal {
    fn new(path: &std::path::Path) -> Arc<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        Arc::new(Wal {
            m: Mutex::new(Shared {
                file,
                pending: Vec::with_capacity(8 << 20),
                pending_frames: 0,
                write_offset: 0,
                durable_upto: 0,
                committing: false,
                fsyncs: 0,
                max_group: 0,
                group_sum: 0,
            }),
            cv: Condvar::new(),
        })
    }

    /// Append and wait until this frame is durable. One fsync may serve many
    /// callers, and each caller returns only after its own bytes are durable.
    ///
    /// The committer releases the lock while it writes and syncs. That is the
    /// whole point: other writers accumulate into the next buffer during the
    /// fsync, so the next round coalesces them. Holding the lock across the
    /// sync serializes everything and defeats group commit entirely.
    /// Group commit with an explicit linger. Without a linger a group only
    /// holds whatever arrived during the previous fsync. A linger trades a
    /// little latency for a much larger group.
    fn append_group_commit_linger(&self, payload: &[u8], linger: Duration) {
        let mut g = self.m.lock().unwrap();
        let pos = g.write_offset + g.pending.len() as u64;
        let f = frame(payload, pos);
        g.pending.extend_from_slice(&f);
        g.pending_frames += 1;
        let my_end = g.write_offset + g.pending.len() as u64;

        if g.committing {
            // Someone else is syncing. Wait for our bytes to become durable.
            while g.durable_upto < my_end {
                g = self.cv.wait(g).unwrap();
            }
            return;
        }

        // Become the committer and keep going until no work is left.
        g.committing = true;
        if !linger.is_zero() {
            drop(g);
            std::thread::sleep(linger);
            g = self.m.lock().unwrap();
        }
        loop {
            let buf = std::mem::take(&mut g.pending);
            let group = g.pending_frames;
            g.pending_frames = 0;
            if buf.is_empty() {
                g.committing = false;
                self.cv.notify_all();
                break;
            }
            let at = g.write_offset;
            g.write_offset += buf.len() as u64;
            if group > g.max_group {
                g.max_group = group;
            }
            g.group_sum += group;

            // Release the lock across the expensive part.
            drop(g);
            let end = {
                let gg = self.m.lock().unwrap();
                gg.file.write_all_at(&buf, at).unwrap();
                gg.file.sync_data().unwrap();
                at + buf.len() as u64
            };
            g = self.m.lock().unwrap();
            g.fsyncs += 1;
            if end > g.durable_upto {
                g.durable_upto = end;
            }
            self.cv.notify_all();

            if g.durable_upto >= my_end && g.pending.is_empty() {
                g.committing = false;
                self.cv.notify_all();
                return;
            }
        }
    }

    /// One fsync for each append. The naive path.
    fn append_each(&self, payload: &[u8]) {
        let mut g = self.m.lock().unwrap();
        let pos = g.write_offset;
        let f = frame(payload, pos);
        let at = g.write_offset;
        g.write_offset += f.len() as u64;
        g.file.write_all_at(&f, at).unwrap();
        g.file.sync_data().unwrap();
        g.fsyncs += 1;
    }
}

fn run(wal: Arc<Wal>, writers: usize, per_writer: usize, payload: usize, group: bool, linger: Duration) -> (f64, f64, Duration, Duration, f64, u64) {
    let payload_buf = vec![0x5au8; payload];
    let start = Instant::now();
    let mut handles = Vec::new();
    let lat = Arc::new(Mutex::new(Vec::<u64>::with_capacity(writers * per_writer)));
    for _ in 0..writers {
        let wal = wal.clone();
        let p = payload_buf.clone();
        let lat = lat.clone();
        handles.push(std::thread::spawn(move || {
            let mut mine = Vec::with_capacity(per_writer);
            for _ in 0..per_writer {
                let t = Instant::now();
                if group {
                    wal.append_group_commit_linger(&p, linger);
                } else {
                    wal.append_each(&p);
                }
                mine.push(t.elapsed().as_micros() as u64);
            }
            lat.lock().unwrap().extend(mine);
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let el = start.elapsed().as_secs_f64();
    let total = writers * per_writer;
    let g = wal.m.lock().unwrap();
    let fsyncs = g.fsyncs;
    let max_group = g.max_group;
    let mean_group = if g.fsyncs > 0 { g.group_sum as f64 / g.fsyncs as f64 } else { 0.0 };
    drop(g);
    let _ = mean_group;

    let mut l = lat.lock().unwrap().clone();
    l.sort_unstable();
    let p50 = Duration::from_micros(l[l.len() / 2]);
    let p99 = Duration::from_micros(l[l.len() * 99 / 100]);

    (
        total as f64 / el,
        fsyncs as f64 / el,
        p50,
        p99,
        total as f64 / fsyncs.max(1) as f64,
        max_group,
    )
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/tallyowl-bench");
    std::fs::create_dir_all(&dir).unwrap();
    let payload: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(4096);

    println!("# WAL group commit benchmark");
    println!("dir={}  payload={} bytes", dir.display(), payload);
    println!("NOTE: this path must be on real storage, never tmpfs.");
    println!();
    println!(
        "{:<16} {:>8} {:>12} {:>11} {:>11} {:>13} {:>11} {:>11}",
        "mode", "writers", "frames/s", "fsyncs/s", "p50", "p99", "mean group", "max group"
    );

    for (label, group, linger_ms) in [
        ("one per frame", false, 0u64),
        ("group, no linger", true, 0),
        ("group, 2ms linger", true, 2),
        ("group, 10ms linger", true, 10),
    ] {
        for writers in [1usize, 8, 32, 128] {
            let per = if group { 200 } else { 40 };
            let path = dir.join(format!("wal-{}-{}-{}.log", group, linger_ms, writers));
            let wal = Wal::new(&path);
            let (rate, fsyncs, p50, p99, meang, maxg) =
                run(wal, writers, per, payload, group, Duration::from_millis(linger_ms));
            println!(
                "{:<16} {:>8} {:>12.0} {:>11.0} {:>11?} {:>13?} {:>11.1} {:>11}",
                label, writers, rate, fsyncs, p50, p99, meang, maxg
            );
            let _ = std::fs::remove_file(&path);
        }
    }

    println!();
    println!("frames/s above the fsync ceiling means group commit is working.");
    println!("max group is how many frames one fsync made durable at its best.");
}
