//! The delivery task: what intake puts in the queue and a forwarder takes out.
//!
//! `docs/DELIVERY.md` section 4 names the gap this fills. Corndogs holds task
//! state, atomic claims, timeout state swaps, priority, and durable backends.
//! It holds no attempt count and no next-attempt time. TallyOwl therefore
//! carries retry counts, next-attempt time, and batch lineage in its own task
//! payload until Corndogs grows a native scheduling contract.
//!
//! Until this existed the payload was a bare encoded batch, so a forwarder had
//! no way to tell a first attempt from a hundredth. Every retryable failure
//! used the first entry of the backoff table for ever, which meant a head that
//! was down for an hour was asked once a second for that hour. See L012.
//!
//! # Compression
//!
//! A batch is CBOR full of repeated keys and repeated dimension values, so it
//! compresses well. Every byte saved is a byte the durable queue does not have
//! to write, fsync, and read back, and the queue is the part of the path that
//! is bounded by a device rather than by a processor.
//!
//! Compression is applied only when it helps. A small batch, or one that does
//! not shrink, travels as it is, and the task says which.
//!
//! # Refusing a payload before producing it
//!
//! The task declares its uncompressed size, so a reader refuses an oversized
//! payload before it allocates for it. **The absolute bound is what protects
//! memory.** A ratio bound does not separate a bomb from ordinary telemetry:
//! the shape that makes a columnar format work and the shape a bomb uses are
//! the same shape, and a real column of one release name reaches a ratio in the
//! thousands. See L021.

use tallyowl_collector_api::codec::{decode_delivery_task, encode_delivery_task};
use tallyowl_collector_api::types::{Batch, Compression, DeliveryTask};
use tallyowl_obs::error::TallyOwlError;

/// The task shape this build writes. A forwarder refuses a newer one rather
/// than guessing what changed.
pub const TASK_VERSION: u64 = 1;

/// Below this, compression is not worth the processor time. A batch of one
/// small event is already close to its floor.
const COMPRESS_ABOVE_BYTES: usize = 1024;

/// The zstd level. D17 measured this level for segment pages and found the step
/// to a higher one bought little. A queue payload lives for seconds, so the
/// same reasoning applies more strongly.
const ZSTD_LEVEL: i32 = 3;

/// Build the task for one accepted batch.
pub fn seal(batch: &Batch, source_id: &[u8], accepted_at: i64) -> DeliveryTask {
    let encoded = tallyowl_collector_api::codec::encode_batch(batch);
    let uncompressed_bytes = encoded.len() as u64;

    // Compress only when it helps. A payload that grew would cost the queue
    // bytes and cost a reader a decompression for nothing.
    let (payload, compression) = if encoded.len() > COMPRESS_ABOVE_BYTES {
        match zstd::encode_all(encoded.as_slice(), ZSTD_LEVEL) {
            Ok(squeezed) if squeezed.len() < encoded.len() => (squeezed, Compression::Zstd),
            _ => (encoded, Compression::None),
        }
    } else {
        (encoded, Compression::None)
    };

    DeliveryTask {
        task_version: TASK_VERSION,
        batch_id: batch.batch_id.clone(),
        source_id: source_id.to_vec(),
        batch: payload,
        compression,
        uncompressed_bytes,
        attempts: 0,
        accepted_at,
        last_attempt_at: None,
        next_attempt_at: None,
        last_failure: None,
    }
}

pub fn encode(task: &DeliveryTask) -> Vec<u8> {
    encode_delivery_task(task)
}

/// Read a task from a queue payload.
///
/// A payload this build cannot read is a permanent failure, not a retryable
/// one: no number of attempts turns an unreadable payload into a readable one.
pub fn decode(payload: &[u8]) -> Result<DeliveryTask, TallyOwlError> {
    let task = decode_delivery_task(payload).map_err(|e| {
        TallyOwlError::invalid_argument(format!("The stored batch could not be read: {e}"))
    })?;
    if task.task_version > TASK_VERSION {
        return Err(TallyOwlError::new(
            tallyowl_obs::ErrorCode::SchemaUnsupported,
            format!(
                "This batch was queued by a newer version of TallyOwl. It says task version {}, \
                 and this collector reads version {TASK_VERSION}. Upgrade this collector, or \
                 leave the batch for one that is already upgraded.",
                task.task_version
            ),
        )
        .retryable(false));
    }
    Ok(task)
}

/// Recover the batch a task carries.
///
/// `max_bytes` is the absolute bound. It is checked against the declared size
/// before anything is produced, and against the produced size afterwards,
/// because a declaration is a claim rather than a fact.
pub fn open(task: &DeliveryTask, max_bytes: u64) -> Result<Batch, TallyOwlError> {
    if task.uncompressed_bytes > max_bytes {
        return Err(TallyOwlError::over_limit(
            "Queued batch",
            &format!("{} bytes", task.uncompressed_bytes),
            &format!("{max_bytes} bytes"),
            "Lower the batch seal size, or raise `corndogs.maxPayloadBytes`.",
        ));
    }

    let bytes = match task.compression {
        Compression::None => task.batch.clone(),
        Compression::Zstd => {
            // The declared size is the allocation bound, so a payload that
            // claims a large expansion is refused above rather than served.
            let produced = zstd::bulk::decompress(&task.batch, task.uncompressed_bytes as usize)
                .map_err(|e| {
                    TallyOwlError::invalid_argument(format!(
                        "The stored batch could not be uncompressed: {e}"
                    ))
                })?;
            if produced.len() as u64 != task.uncompressed_bytes {
                // The payload disagreed with its own declaration. Refuse it
                // rather than trusting whichever of the two happens to be
                // convenient.
                return Err(TallyOwlError::invalid_argument(format!(
                    "The stored batch says it holds {} bytes and holds {}.",
                    task.uncompressed_bytes,
                    produced.len()
                )));
            }
            produced
        }
    };

    tallyowl_collector_api::codec::decode_batch(&bytes).map_err(|e| {
        TallyOwlError::invalid_argument(format!("The stored batch could not be read: {e}"))
    })
}

/// Record one failed attempt and say when the next one may happen.
pub fn note_attempt(task: &mut DeliveryTask, at: i64, delay_ms: i64, failure: &str) {
    task.attempts += 1;
    task.last_attempt_at = Some(at);
    task.next_attempt_at = Some(at + delay_ms);
    // A failure message reaches an operator's log and a quarantine record, so
    // it is bounded here rather than wherever it came from.
    task.last_failure = Some(clamp(failure, 512));
}

fn clamp(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    text.chars().take(most - 1).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;
    use tallyowl_collector_api::types::{
        Envelope, EventPayload, PropertyOrigin, TelemetryItem, TelemetryKind,
    };
    use tallyowl_wire::{collector as wire, collector_items_bridge as items, Value};

    const MAX: u64 = 16 * 1024 * 1024;

    fn item(n: u8, name: &str, properties: usize) -> TelemetryItem {
        let mut envelope = Envelope {
            event_id: vec![n; 16],
            kind: TelemetryKind::Event,
            schema_version: 1,
            occurred_at: 1_785_628_800_000,
            observed_at: None,
            received_at: None,
            workspace_id: None,
            project_id: None,
            source_id: None,
            sequence: None,
            release: Some("2026.8.1".into()),
            service_name: Some("checkout".into()),
            request_id: None,
            session_id: None,
            end_user_id: None,
            anonymous_id: None,
            trace_id: None,
            span_id: None,
            consent: None,
            sdk_name: "test".into(),
            sdk_version: "0".into(),
            properties: Vec::new(),
            measurements: None,
        };
        for index in 0..properties {
            envelope.properties.push(wire::property(
                &format!("dimension_{index}"),
                Value::Text("us-west2".into()),
                PropertyOrigin::Client,
            ));
        }
        items::event(
            envelope,
            EventPayload {
                name: name.into(),
                route: None,
                page_title: None,
            },
        )
    }

    fn batch(items: Vec<TelemetryItem>) -> Batch {
        Batch {
            batch_id: vec![7; 16],
            items,
            common_properties: None,
            sealed_at: 1_785_628_800_000,
            compression: None,
        }
    }

    #[test]
    fn a_small_batch_travels_uncompressed() {
        let sealed = seal(&batch(vec![item(1, "checkout-started", 0)]), &[9; 16], 1);
        assert_eq!(sealed.compression, Compression::None);
        assert_eq!(sealed.batch.len() as u64, sealed.uncompressed_bytes);
    }

    #[test]
    fn a_repetitive_batch_shrinks_and_comes_back_exactly() {
        // The shape a real batch has: many items, each with the same dimension
        // names and the same dimension values.
        let original = batch((0..60).map(|n| item(n, "checkout-started", 12)).collect());
        let sealed = seal(&original, &[9; 16], 1);

        assert_eq!(sealed.compression, Compression::Zstd);
        assert!(
            (sealed.batch.len() as u64) < sealed.uncompressed_bytes / 2,
            "a repetitive batch should halve at least: {} of {}",
            sealed.batch.len(),
            sealed.uncompressed_bytes
        );
        assert_eq!(open(&sealed, MAX).unwrap(), original);
    }

    #[test]
    fn a_task_round_trips_through_the_queue_payload() {
        let original = batch((0..40).map(|n| item(n, "purchase", 8)).collect());
        let sealed = seal(&original, &[9; 16], 1_000);
        let read = decode(&encode(&sealed)).expect("the payload reads");
        assert_eq!(read, sealed);
        assert_eq!(open(&read, MAX).unwrap(), original);
    }

    #[test]
    fn a_declared_size_over_the_bound_is_refused_before_anything_is_produced() {
        let mut sealed = seal(&batch(vec![item(1, "e", 0)]), &[9; 16], 1);
        sealed.uncompressed_bytes = 64 * 1024 * 1024;
        let failure = open(&sealed, MAX).unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::ResourceExhausted);
        assert!(failure.message.contains("Queued batch"));
    }

    #[test]
    fn a_payload_that_disagrees_with_its_own_declaration_is_refused() {
        // The declaration is a claim. A reader that trusted it would store
        // fewer rows than arrived and say nothing.
        let original = batch((0..40).map(|n| item(n, "purchase", 8)).collect());
        let mut sealed = seal(&original, &[9; 16], 1);
        assert_eq!(sealed.compression, Compression::Zstd);
        sealed.uncompressed_bytes -= 1;
        let failure = open(&sealed, MAX).unwrap_err();
        // Refused either because the declared bound stopped the production or
        // because the produced size did not match. Both are the same answer to
        // the caller, and both are permanent.
        assert!(!failure.retryable);
        assert!(
            failure.message.contains("stored batch"),
            "a message an operator can act on: {}",
            failure.message
        );
    }

    #[test]
    fn a_damaged_payload_is_a_permanent_failure_rather_than_a_retryable_one() {
        // No number of attempts turns an unreadable payload into a readable
        // one, so a retry loop over it burns the queue for nothing.
        let original = batch((0..40).map(|n| item(n, "purchase", 8)).collect());
        let mut sealed = seal(&original, &[9; 16], 1);
        let middle = sealed.batch.len() / 2;
        sealed.batch[middle] ^= 0xff;
        let failure = open(&sealed, MAX).unwrap_err();
        assert!(!failure.retryable);
    }

    #[test]
    fn a_task_from_a_newer_collector_is_refused_by_name() {
        let mut sealed = seal(&batch(vec![item(1, "e", 0)]), &[9; 16], 1);
        sealed.task_version = TASK_VERSION + 1;
        let failure = decode(&encode(&sealed)).unwrap_err();
        assert_eq!(failure.code, tallyowl_obs::ErrorCode::SchemaUnsupported);
        assert!(!failure.retryable);
        assert!(failure.message.contains("Upgrade this collector"));
    }

    #[test]
    fn an_attempt_is_counted_and_the_next_time_is_recorded() {
        let mut sealed = seal(&batch(vec![item(1, "e", 0)]), &[9; 16], 1_000);
        assert_eq!(sealed.attempts, 0);
        note_attempt(&mut sealed, 2_000, 5_000, "the head was unreachable");
        assert_eq!(sealed.attempts, 1);
        assert_eq!(sealed.last_attempt_at, Some(2_000));
        assert_eq!(sealed.next_attempt_at, Some(7_000));
        assert_eq!(
            sealed.last_failure.as_deref(),
            Some("the head was unreachable")
        );
        assert_eq!(sealed.accepted_at, 1_000, "the acceptance time never moves");
    }

    #[test]
    fn a_long_failure_message_is_bounded_to_what_the_contract_permits() {
        let mut sealed = seal(&batch(vec![item(1, "e", 0)]), &[9; 16], 1);
        note_attempt(&mut sealed, 1, 1, &"x".repeat(5_000));
        let held = sealed.last_failure.clone().unwrap();
        assert_eq!(held.chars().count(), 512);
        // And it still encodes, which is the point of bounding it.
        assert!(decode(&encode(&sealed)).is_ok());
    }
}
