//! Inert staging-read lifecycle vocabulary; this does not admit or perform reads.

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

#[cfg(test)]
mod tests {
    use super::MeasurementState::{self, *};
    use super::{fixed_read_slot, ReadOperation, ReadSlot};

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
