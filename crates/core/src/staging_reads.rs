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

#[cfg(test)]
mod tests {
    use super::MeasurementState::{self, *};

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
