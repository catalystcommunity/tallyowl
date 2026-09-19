//! Translating one error taxonomy into two generated ones.
//!
//! `csil/types/common.csil` holds the normative error type, and every entry
//! specification includes it. That gives each generated package its own copy of
//! the same shape, so the head needs one translation for each package it
//! answers on. The taxonomy is single, and only the Rust type differs.

use tallyowl_obs::error::{ErrorCode, TallyOwlError};

pub fn to_collector_error(error: &TallyOwlError) -> tallyowl_collector_api::types::ServiceError {
    use tallyowl_collector_api::types::{ErrorCode as Wire, PropertyOrigin, ServiceError};
    use tallyowl_wire::{collector as bridge, Value};
    ServiceError {
        code: match error.code {
            ErrorCode::InvalidArgument => Wire::InvalidArgument,
            ErrorCode::Unauthenticated => Wire::Unauthenticated,
            ErrorCode::PermissionDenied => Wire::PermissionDenied,
            ErrorCode::NotFound => Wire::NotFound,
            ErrorCode::AlreadyExists => Wire::AlreadyExists,
            ErrorCode::ResourceExhausted => Wire::ResourceExhausted,
            ErrorCode::FailedPrecondition => Wire::FailedPrecondition,
            ErrorCode::Unavailable => Wire::Unavailable,
            ErrorCode::SchemaUnsupported => Wire::SchemaUnsupported,
            ErrorCode::BudgetExceeded => Wire::BudgetExceeded,
            ErrorCode::IncompleteResult => Wire::IncompleteResult,
            ErrorCode::Internal => Wire::Internal,
        },
        message: error.message.clone(),
        retryable: error.retryable,
        detail: (!error.detail.is_empty()).then(|| {
            error
                .detail
                .iter()
                .map(|(k, v)| {
                    bridge::property(k, Value::Text(v.clone()), PropertyOrigin::Collector)
                })
                .collect()
        }),
    }
}

pub fn to_control_error(error: &TallyOwlError) -> tallyowl_control_api::types::ServiceError {
    use tallyowl_control_api::types::{ErrorCode as Wire, PropertyOrigin, ServiceError};
    use tallyowl_wire::{control as bridge, Value};
    ServiceError {
        code: match error.code {
            ErrorCode::InvalidArgument => Wire::InvalidArgument,
            ErrorCode::Unauthenticated => Wire::Unauthenticated,
            ErrorCode::PermissionDenied => Wire::PermissionDenied,
            ErrorCode::NotFound => Wire::NotFound,
            ErrorCode::AlreadyExists => Wire::AlreadyExists,
            ErrorCode::ResourceExhausted => Wire::ResourceExhausted,
            ErrorCode::FailedPrecondition => Wire::FailedPrecondition,
            ErrorCode::Unavailable => Wire::Unavailable,
            ErrorCode::SchemaUnsupported => Wire::SchemaUnsupported,
            ErrorCode::BudgetExceeded => Wire::BudgetExceeded,
            ErrorCode::IncompleteResult => Wire::IncompleteResult,
            ErrorCode::Internal => Wire::Internal,
        },
        message: error.message.clone(),
        retryable: error.retryable,
        detail: (!error.detail.is_empty()).then(|| {
            error
                .detail
                .iter()
                .map(|(k, v)| {
                    bridge::property(k, Value::Text(v.clone()), PropertyOrigin::Collector)
                })
                .collect()
        }),
    }
}

// ---------------------------------------------------------------------------
// Workflows and notifications. Phase 10.
// ---------------------------------------------------------------------------

/// One workflow's state, as the operator interface reads it.
pub fn to_workflow_status(
    status: &crate::workflows::Status,
) -> tallyowl_control_api::types::WorkflowStatus {
    tallyowl_control_api::types::WorkflowStatus {
        kind: to_workflow_kind(
            crate::workflows::Kind::parse(&status.kind)
                .unwrap_or(crate::workflows::Kind::ProjectorRebuild),
        ),
        queue: status.queue.clone(),
        pending: status.pending,
        in_flight: status.in_flight,
        quarantined: status.quarantined,
        oldest_pending_age_ms: status.oldest_pending_age_ms,
        failures: status.failures,
        last_success_at: (status.last_success_at > 0).then_some(status.last_success_at),
        last_failure: (!status.last_failure.is_empty()).then(|| status.last_failure.clone()),
    }
}

pub fn to_workflow_kind(kind: crate::workflows::Kind) -> tallyowl_control_api::types::WorkflowKind {
    use tallyowl_control_api::types::WorkflowKind as Wire;
    match kind {
        crate::workflows::Kind::AlertEvaluation => Wire::AlertEvaluation,
        crate::workflows::Kind::Notification => Wire::Notification,
        crate::workflows::Kind::ProjectorRebuild => Wire::ProjectorRebuild,
        crate::workflows::Kind::Retention => Wire::Retention,
        crate::workflows::Kind::Deletion => Wire::Deletion,
        crate::workflows::Kind::Export => Wire::Export,
    }
}

pub fn from_workflow_kind(
    kind: &tallyowl_control_api::types::WorkflowKind,
) -> crate::workflows::Kind {
    use tallyowl_control_api::types::WorkflowKind as Wire;
    match kind {
        Wire::AlertEvaluation => crate::workflows::Kind::AlertEvaluation,
        Wire::Notification => crate::workflows::Kind::Notification,
        Wire::ProjectorRebuild => crate::workflows::Kind::ProjectorRebuild,
        Wire::Retention => crate::workflows::Kind::Retention,
        Wire::Deletion => crate::workflows::Kind::Deletion,
        Wire::Export => crate::workflows::Kind::Export,
    }
}

/// One notification attempt, delivered or not.
pub fn to_delivery(
    record: &tallyowl_store::control::NotificationRecord,
) -> tallyowl_control_api::types::NotificationDelivery {
    tallyowl_control_api::types::NotificationDelivery {
        rule_id: record.rule_id.clone(),
        target: record.target.clone(),
        state: crate::alerts::state_from(&record.state),
        attempts: record.attempts,
        delivered: record.delivered,
        last_failure: (!record.last_error.is_empty()).then(|| record.last_error.clone()),
        next_attempt_at: (record.next_attempt_at > 0).then_some(record.next_attempt_at),
        at: record.at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_retry_fact_survives_the_translation() {
        // A caller builds automation on `retryable`. A translation that dropped
        // it would turn a permanent rejection into a retry storm.
        let permanent = TallyOwlError::invalid_argument("no");
        assert!(!to_collector_error(&permanent).retryable);
        assert!(!to_control_error(&permanent).retryable);

        let transient = TallyOwlError::unavailable("later");
        assert!(to_collector_error(&transient).retryable);
    }

    #[test]
    fn a_detail_pair_survives_as_a_property() {
        let error = TallyOwlError::over_limit("Batch", "640 KiB", "512 KiB", "Send less.");
        let wire = to_collector_error(&error);
        let detail = wire.detail.expect("the limit and the observed value");
        assert!(detail.iter().any(|p| p.key == "limit"));
        assert!(detail.iter().any(|p| p.key == "observed"));
    }

    #[test]
    fn an_error_with_no_detail_carries_none_rather_than_an_empty_list() {
        assert!(to_control_error(&TallyOwlError::internal("x"))
            .detail
            .is_none());
    }
}
