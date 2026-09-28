//! The identifiers a compatibility edge issues.
//!
//! A scraped sample and an OpenTelemetry data point carry no event identifier,
//! so the edge issues one: a UUID version 7, which is what a driver issues.
//!
//! # Why the layout is written out
//!
//! The head removes duplicates by event identifier, so two items that share
//! one are one row and the second is gone. A scrape builds every envelope in
//! one loop and most of them share a millisecond, so the time gives no
//! separation and the rest of the identifier has to.
//!
//! The version and the variant are fixed bits in the middle of the value:
//!
//! ```text
//! byte  0..6   the time, in milliseconds
//! byte  6      0111 rrrr   version 7, then 4 random bits
//! byte  7      rrrr rrrr
//! byte  8      10rr rrrr   the variant, then 6 random bits
//! byte  9..12  random
//! byte 12..16  a counter, most significant byte first
//! ```
//!
//! A counter written across bytes 6 and 8 loses the bits those two masks
//! cover, and then counters 16 apart are one identifier. So the counter sits
//! where no mask reaches it, and it is what makes two identifiers from one
//! process differ even when the random source fails. The 42 random bits are
//! what make two collectors differ.

use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A new identifier for an item or a batch this edge produced at `now_ms`.
pub fn new_id(now_ms: i64) -> Vec<u8> {
    let mut random = [0u8; 6];
    // A failed random source leaves zeros. The counter still separates every
    // identifier this process issues, so nothing repeats.
    let _ = getrandom::fill(&mut random);
    id_from(now_ms, COUNTER.fetch_add(1, Ordering::Relaxed), random)
}

fn id_from(now_ms: i64, counter: u32, random: [u8; 6]) -> Vec<u8> {
    let mut id = vec![0u8; 16];
    let ms = now_ms.max(0) as u64;
    id[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    id[6] = (random[0] & 0x0f) | 0x70;
    id[7] = random[1];
    id[8] = (random[2] & 0x3f) | 0x80;
    id[9..12].copy_from_slice(&random[3..]);
    id[12..].copy_from_slice(&counter.to_be_bytes());
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn a_hundred_thousand_identifiers_in_one_millisecond_are_all_different() {
        // The clock is the argument, so every one of these shares a time. The
        // random part is held at zero, which is the failed random source: the
        // counter alone has to separate them.
        let ids: HashSet<Vec<u8>> = (0..100_000u32)
            .map(|counter| id_from(1_700_000_000_000, counter, [0; 6]))
            .collect();
        assert_eq!(ids.len(), 100_000);
    }

    #[test]
    fn the_issued_identifiers_do_not_repeat_inside_one_millisecond() {
        let ids: HashSet<Vec<u8>> = (0..100_000).map(|_| new_id(1_700_000_000_000)).collect();
        assert_eq!(ids.len(), 100_000);
    }

    #[test]
    fn every_identifier_is_a_version_7_with_the_standard_variant() {
        for counter in [0u32, 15, 16, 255, 256, u32::MAX] {
            let id = id_from(1_700_000_000_000, counter, [0xff; 6]);
            assert_eq!(id.len(), 16);
            assert_eq!(id[6] >> 4, 7, "the version");
            assert_eq!(id[8] >> 6, 0b10, "the variant");
        }
    }

    #[test]
    fn the_time_leads_so_identifiers_sort_by_when_they_were_issued() {
        assert!(id_from(1_000, u32::MAX, [0xff; 6]) < id_from(1_001, 0, [0; 6]));
    }
}
