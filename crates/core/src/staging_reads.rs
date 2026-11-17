//! Inert staging-read lifecycle vocabulary; this does not admit or perform reads.

pub mod canonical_candidate;
pub mod canonical_lifecycle;
pub mod canonical_owner;
pub mod canonical_prefix;
pub mod canonical_request;
pub mod canonical_slots;
pub mod canonical_tail;
pub mod canonical_target;
pub mod canonical_time;

/// The lifecycle phase of a counted staging-read measurement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MeasurementState {
    Challenged,
    Measuring,
    ReadsClosed,
    FinalSampled,
    Judged,
    Failed,
    Revoked,
    Expired,
}

impl MeasurementState {
    /// Checks ordering only. Callers must independently enforce all evidence,
    /// identity, timing, persistence and authorization requirements.
    pub fn permits_transition_to(self, next: Self) -> bool {
        use MeasurementState::{
            Challenged, Expired, Failed, FinalSampled, Judged, Measuring, ReadsClosed, Revoked,
        };
        matches!(
            (self, next),
            (Challenged, Measuring | Failed | Revoked | Expired)
                | (Measuring, ReadsClosed | Failed | Revoked | Expired)
                | (ReadsClosed, FinalSampled | Failed | Revoked | Expired)
                | (FinalSampled, Judged | Failed | Revoked | Expired)
        )
    }
}

/// The only operations in the fixed staging-read plan. This is not an
/// authorization to execute them or a representation of measured traffic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadOperation {
    Identity,
    GuildContext,
    BotMembership,
    EventList,
}

/// An inert plan entry, with its due time measured from the sealed anchor T.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadSlot {
    pub index: u8,
    pub operation: ReadOperation,
    pub due_after_seconds: u16,
}

/// Returns one of the 40 fixed slots; out-of-range indices have no slot.
///
/// Execution must independently enforce the one-second dispatch window,
/// no retries or catch-up, identity gating, and all other contract controls.
pub fn fixed_read_slot(index: u8) -> Option<ReadSlot> {
    const OPERATIONS: [ReadOperation; 4] = [
        ReadOperation::Identity,
        ReadOperation::GuildContext,
        ReadOperation::BotMembership,
        ReadOperation::EventList,
    ];
    if index >= 40 {
        return None;
    }
    Some(ReadSlot {
        index,
        operation: OPERATIONS[usize::from(index % 4)],
        due_after_seconds: 15 + 10 * u16::from(index),
    })
}

/// In-memory assertions about one fixed-plan outcome, not verified evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedReadOutcome {
    pub index: u8,
    pub http_status: Option<u16>,
    pub body_validated: bool,
    pub settlement_confirmed: bool,
    pub accounting_confirmed: bool,
    pub late_or_unknown: bool,
}

/// Checks a necessary recorded-outcome condition only. A true result does not
/// establish measured traffic, provenance, authorization or watch qualification.
/// A missing status does not establish that the operation had no remote effect.
pub fn recorded_workload_is_complete(outcomes: &[RecordedReadOutcome]) -> bool {
    outcomes.len() == 40
        && outcomes.iter().enumerate().all(|(position, outcome)| {
            usize::from(outcome.index) == position
                && fixed_read_slot(outcome.index).is_some()
                && outcome
                    .http_status
                    .is_some_and(|status| (200..=299).contains(&status))
                && outcome.body_validated
                && outcome.settlement_confirmed
                && outcome.accounting_confirmed
                && !outcome.late_or_unknown
        })
}

#[cfg(test)]
mod tests {
    use super::MeasurementState::{self, *};
    use super::{
        fixed_read_slot, recorded_workload_is_complete, ReadOperation, ReadSlot,
        RecordedReadOutcome,
    };

    fn complete_outcomes() -> Vec<RecordedReadOutcome> {
        (0..40)
            .map(|index| RecordedReadOutcome {
                index,
                http_status: Some(200),
                body_validated: true,
                settlement_confirmed: true,
                accounting_confirmed: true,
                late_or_unknown: false,
            })
            .collect()
    }

    #[test]
    fn recorded_workload_requires_exact_ordered_plan() {
        // A true result is not measured traffic or qualification evidence.
        let valid = complete_outcomes();
        assert!(recorded_workload_is_complete(&valid));
        let mut all_299 = valid.clone();
        for outcome in &mut all_299 {
            outcome.http_status = Some(299);
        }
        assert!(recorded_workload_is_complete(&all_299));
        for length in [0, 1, 9, 10, 39] {
            assert!(!recorded_workload_is_complete(&valid[..length]));
        }
        let mut extra = valid.clone();
        extra.push(RecordedReadOutcome {
            index: 40,
            ..valid[0]
        });
        assert!(!recorded_workload_is_complete(&extra));
        let mut swapped = valid.clone();
        swapped.swap(0, 1);
        assert!(!recorded_workload_is_complete(&swapped));
        let mut duplicate = valid.clone();
        duplicate[1].index = 0;
        assert!(!recorded_workload_is_complete(&duplicate));
        for index in [40, 255] {
            let mut invalid = valid.clone();
            invalid[39].index = index;
            assert!(!recorded_workload_is_complete(&invalid));
        }
    }

    #[test]
    fn every_record_requires_success_and_confirmed_outcomes() {
        let valid = complete_outcomes();
        for position in 0..40 {
            // None is not an assertion that the operation had zero effects.
            for status in [
                None,
                Some(0),
                Some(199),
                Some(300),
                Some(400),
                Some(429),
                Some(500),
                Some(599),
                Some(600),
                Some(u16::MAX),
            ] {
                let mut outcomes = valid.clone();
                outcomes[position].http_status = status;
                assert!(
                    !recorded_workload_is_complete(&outcomes),
                    "position {position}, status {status:?}"
                );
            }
            for status in [200, 299] {
                let mut outcomes = valid.clone();
                outcomes[position].http_status = Some(status);
                assert!(recorded_workload_is_complete(&outcomes));
            }
            for mutate in [
                |outcome: &mut RecordedReadOutcome| outcome.body_validated = false,
                |outcome: &mut RecordedReadOutcome| outcome.settlement_confirmed = false,
                |outcome: &mut RecordedReadOutcome| outcome.accounting_confirmed = false,
                |outcome: &mut RecordedReadOutcome| outcome.late_or_unknown = true,
            ] {
                let mut outcomes = valid.clone();
                mutate(&mut outcomes[position]);
                assert!(
                    !recorded_workload_is_complete(&outcomes),
                    "position {position}"
                );
            }
        }
    }

    #[test]
    fn fixed_plan_contains_exact_order_and_timing() {
        let operations = [
            ReadOperation::Identity,
            ReadOperation::GuildContext,
            ReadOperation::BotMembership,
            ReadOperation::EventList,
        ];
        for round in 0_u8..10 {
            for (position, operation) in operations.into_iter().enumerate() {
                let index = round * 4 + position as u8;
                assert_eq!(
                    fixed_read_slot(index),
                    Some(ReadSlot {
                        index,
                        operation,
                        due_after_seconds: 15 + 40 * u16::from(round) + 10 * position as u16,
                    })
                );
            }
        }
        for index in 40..=u8::MAX {
            assert_eq!(fixed_read_slot(index), None);
        }
    }

    #[test]
    fn lifecycle_transition_matrix_is_closed_and_forward_only() {
        let states = [
            Challenged,
            Measuring,
            ReadsClosed,
            FinalSampled,
            Judged,
            Failed,
            Revoked,
            Expired,
        ];
        let allowed = [
            (Challenged, Measuring),
            (Measuring, ReadsClosed),
            (ReadsClosed, FinalSampled),
            (FinalSampled, Judged),
            (Challenged, Failed),
            (Challenged, Revoked),
            (Challenged, Expired),
            (Measuring, Failed),
            (Measuring, Revoked),
            (Measuring, Expired),
            (ReadsClosed, Failed),
            (ReadsClosed, Revoked),
            (ReadsClosed, Expired),
            (FinalSampled, Failed),
            (FinalSampled, Revoked),
            (FinalSampled, Expired),
        ];
        for from in states {
            for to in states {
                assert_eq!(
                    MeasurementState::permits_transition_to(from, to),
                    allowed.contains(&(from, to)),
                    "{from:?} -> {to:?}"
                );
            }
        }
    }
}
