//! Inert timing consistency checks; these neither authenticate nor execute reads.

use super::fixed_read_slot;

/// A recorded millisecond tick in a caller-supplied sealed monotonic timeline.
/// Matching identifiers assert comparability; they do not authenticate evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordedTick {
    pub timeline: u64,
    pub millis: u64,
}

/// The required milestones and original frozen bounds for one recorded slot.
/// Missing evidence must remain missing rather than being replaced by a new bound.
#[derive(Clone, Copy, Debug)]
pub struct RecordedReadTiming {
    pub index: u8,
    pub anchor: Option<RecordedTick>,
    pub receipt: Option<RecordedTick>,
    pub admission: Option<RecordedTick>,
    pub wire: Option<RecordedTick>,
    pub headers: Option<RecordedTick>,
    pub body: Option<RecordedTick>,
    pub settlement: Option<RecordedTick>,
    pub attempt_cutoff: Option<RecordedTick>,
    pub operation_authority_cutoff: Option<RecordedTick>,
    pub settlement_cutoff: Option<RecordedTick>,
    pub settlement_authority_cutoff: Option<RecordedTick>,
    pub admission_confirmed: bool,
    pub body_validated: bool,
    pub settlement_confirmed: bool,
    pub accounting_confirmed: bool,
    pub late_or_unknown: bool,
}

/// Checks necessary timing consistency only, never authorization or watch success.
/// The caller must supply the earliest applicable original-authority cutoffs,
/// including applicable revocation and grant expiry, in the same sealed process timeline.
/// Receipt is local to the serving fence, before durable admission or scheduling.
/// Wire is the final local dispatch decision. Settlement is confirmed owner and
/// accounting settlement, not the caller's response.
pub fn recorded_timing_is_consistent(record: &RecordedReadTiming) -> bool {
    fn check(record: &RecordedReadTiming) -> Option<bool> {
        let slot = fixed_read_slot(record.index)?;
        let ticks = [
            record.anchor?,
            record.receipt?,
            record.admission?,
            record.wire?,
            record.headers?,
            record.body?,
            record.settlement?,
            record.attempt_cutoff?,
            record.operation_authority_cutoff?,
            record.settlement_cutoff?,
            record.settlement_authority_cutoff?,
        ];
        if ticks.iter().any(|tick| tick.timeline != ticks[0].timeline) {
            return Some(false);
        }
        let [t, r, a, w, h, b, c, f_op, g_op, f_settle, g_settle] = ticks.map(|tick| tick.millis);
        let start = t.checked_add(u64::from(slot.due_after_seconds).checked_mul(1000)?)?;
        let end = start.checked_add(1000)?;
        let operation_deadline = r.checked_add(5000)?.min(f_op).min(g_op);
        let settlement_deadline = operation_deadline
            .checked_add(1000)?
            .min(f_settle)
            .min(g_settle);
        Some(
            record.admission_confirmed
                && record.body_validated
                && record.settlement_confirmed
                && record.accounting_confirmed
                && !record.late_or_unknown
                && t <= r
                && r <= a
                && a <= w
                && w <= h
                && h <= b
                && b <= c
                && start <= a
                && w < end
                && b < operation_deadline
                && c < settlement_deadline,
        )
    }
    check(record).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(millis: u64) -> Option<RecordedTick> {
        Some(RecordedTick {
            timeline: 7,
            millis,
        })
    }

    fn valid(index: u8, open: u64) -> RecordedReadTiming {
        RecordedReadTiming {
            index,
            anchor: tick(0),
            receipt: tick(open),
            admission: tick(open),
            wire: tick(open + 999),
            headers: tick(open + 1000),
            body: tick(open + 4999),
            settlement: tick(open + 5999),
            attempt_cutoff: tick(u64::MAX),
            operation_authority_cutoff: tick(u64::MAX),
            settlement_cutoff: tick(u64::MAX),
            settlement_authority_cutoff: tick(u64::MAX),
            admission_confirmed: true,
            body_validated: true,
            settlement_confirmed: true,
            accounting_confirmed: true,
            late_or_unknown: false,
        }
    }

    macro_rules! assert_timing {
        ($base:expr, $expected:expr $(, $field:ident: $value:expr)* $(,)?) => {
            assert_eq!(
                recorded_timing_is_consistent(&RecordedReadTiming { $($field: $value,)* ..$base }),
                $expected,
                "{}",
                stringify!($($field: $value),*)
            );
        };
    }

    #[test]
    fn exclusive_bounds_apply_to_slot_work_and_settlement() {
        let v = valid(0, 15_000);
        assert!(recorded_timing_is_consistent(&v));
        assert_timing!(v, true, wire: tick(15_000));
        assert_timing!(v, false, receipt: tick(14_999), admission: tick(14_999));
        assert_timing!(v, false, wire: tick(16_000));
        assert_timing!(v, false, admission: tick(16_000), wire: tick(16_000));
        let last = valid(39, 405_000);
        assert!(recorded_timing_is_consistent(&last));
        assert_timing!(last, false, index: 40);
        assert_timing!(last, false, settlement: tick(411_000));

        // Early receipt is allowed, but admission does not reset its deadline.
        let early = RecordedReadTiming {
            receipt: tick(14_999),
            ..v
        };
        assert_timing!(early, true, body: tick(19_998), settlement: tick(19_998));
        assert_timing!(early, false, settlement: tick(19_999));
        assert_timing!(v, false, body: tick(20_000), settlement: tick(20_000));
        assert_timing!(v, false, settlement: tick(21_000));
    }

    #[test]
    fn original_cutoffs_tighten_operation_and_settlement_independently() {
        let v = valid(0, 15_000);
        assert_timing!(v, false, attempt_cutoff: tick(19_999));
        assert_timing!(v, false, operation_authority_cutoff: tick(19_999));
        assert_timing!(v, false, settlement_cutoff: tick(20_999));
        assert_timing!(v, false, settlement_authority_cutoff: tick(20_999));
        let bounded = RecordedReadTiming {
            attempt_cutoff: tick(19_000),
            ..v
        };
        assert_timing!(bounded, true, body: tick(18_999), settlement: tick(19_999));
        assert_timing!(bounded, false, body: tick(18_999), settlement: tick(20_000));
    }

    #[test]
    fn missing_incomparable_reversed_or_unconfirmed_evidence_fails() {
        let v = valid(0, 15_000);
        assert_timing!(v, false, anchor: None);
        assert_timing!(v, false, receipt: None);
        assert_timing!(v, false, admission: None);
        assert_timing!(v, false, wire: None);
        assert_timing!(v, false, headers: None);
        assert_timing!(v, false, body: None);
        assert_timing!(v, false, settlement: None);
        assert_timing!(v, false, attempt_cutoff: None);
        assert_timing!(v, false, operation_authority_cutoff: None);
        assert_timing!(v, false, settlement_cutoff: None);
        assert_timing!(v, false, settlement_authority_cutoff: None);
        assert_timing!(v, false, admission_confirmed: false);
        assert_timing!(v, false, body_validated: false);
        assert_timing!(v, false, settlement_confirmed: false);
        assert_timing!(v, false, accounting_confirmed: false);
        assert_timing!(v, false, late_or_unknown: true);
        assert_timing!(v, false, wire: Some(RecordedTick { timeline: 8, millis: 15_999 }));
        assert_timing!(v, false, receipt: tick(15_001));
        assert_timing!(v, false, headers: tick(15_998));
        assert_timing!(v, false, settlement: tick(19_998));
    }

    #[test]
    fn overflowing_deadlines_fail_even_with_a_finite_settlement_cutoff() {
        let open = u64::MAX - 5999;
        let v = RecordedReadTiming {
            anchor: tick(open - 15_000),
            settlement: tick(open + 4999),
            settlement_cutoff: tick(open + 5000),
            ..valid(0, open)
        };
        assert_timing!(v, true, attempt_cutoff: tick(open + 4999), body: tick(open + 4998));
        assert!(!recorded_timing_is_consistent(&v));
        assert_timing!(v, false, anchor: tick(u64::MAX));
        assert_timing!(v, false, receipt: tick(u64::MAX));
    }
}
