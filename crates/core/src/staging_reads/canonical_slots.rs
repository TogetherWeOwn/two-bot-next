//! Inert encoding of the fixed manifest slots array, not a manifest validator.
//!
//! This fragment does not compare trusted expected sets, authenticate requests,
//! authorize reads, or change the fixed workload's execution and timing rules.

use super::{fixed_read_slot, ReadOperation, ReadSlot};
use std::fmt::Write;

/// Encodes only the exact ordered fixed plan as the canonical slots array.
/// Any missing, extra, reordered, or modified entry is refused.
pub fn encode_slots(slots: &[ReadSlot]) -> Option<String> {
    if slots.len() != 40
        || slots
            .iter()
            .enumerate()
            .any(|(index, slot)| fixed_read_slot(index as u8).as_ref() != Some(slot))
    {
        return None;
    }

    let mut output = String::from("[");
    for (position, slot) in slots.iter().enumerate() {
        if position != 0 {
            output.push(',');
        }
        let operation = match slot.operation {
            ReadOperation::Identity => "identity",
            ReadOperation::GuildContext => "guild_context",
            ReadOperation::BotMembership => "bot_membership",
            ReadOperation::EventList => "event_list",
        };
        write!(
            output,
            "{{\"index\":\"{}\",\"operation\":\"{}\",\"offset_ms\":\"{}\"}}",
            slot.index,
            operation,
            u32::from(slot.due_after_seconds) * 1000
        )
        .ok()?;
    }
    output.push(']');
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Vec<ReadSlot> {
        (0..40)
            .map(|index| fixed_read_slot(index).unwrap())
            .collect()
    }

    #[test]
    fn encodes_exact_interoperability_slots() {
        let expected = concat!(
            "[",
            "{\"index\":\"0\",\"operation\":\"identity\",\"offset_ms\":\"15000\"},",
            "{\"index\":\"1\",\"operation\":\"guild_context\",\"offset_ms\":\"25000\"},",
            "{\"index\":\"2\",\"operation\":\"bot_membership\",\"offset_ms\":\"35000\"},",
            "{\"index\":\"3\",\"operation\":\"event_list\",\"offset_ms\":\"45000\"},",
            "{\"index\":\"4\",\"operation\":\"identity\",\"offset_ms\":\"55000\"},",
            "{\"index\":\"5\",\"operation\":\"guild_context\",\"offset_ms\":\"65000\"},",
            "{\"index\":\"6\",\"operation\":\"bot_membership\",\"offset_ms\":\"75000\"},",
            "{\"index\":\"7\",\"operation\":\"event_list\",\"offset_ms\":\"85000\"},",
            "{\"index\":\"8\",\"operation\":\"identity\",\"offset_ms\":\"95000\"},",
            "{\"index\":\"9\",\"operation\":\"guild_context\",\"offset_ms\":\"105000\"},",
            "{\"index\":\"10\",\"operation\":\"bot_membership\",\"offset_ms\":\"115000\"},",
            "{\"index\":\"11\",\"operation\":\"event_list\",\"offset_ms\":\"125000\"},",
            "{\"index\":\"12\",\"operation\":\"identity\",\"offset_ms\":\"135000\"},",
            "{\"index\":\"13\",\"operation\":\"guild_context\",\"offset_ms\":\"145000\"},",
            "{\"index\":\"14\",\"operation\":\"bot_membership\",\"offset_ms\":\"155000\"},",
            "{\"index\":\"15\",\"operation\":\"event_list\",\"offset_ms\":\"165000\"},",
            "{\"index\":\"16\",\"operation\":\"identity\",\"offset_ms\":\"175000\"},",
            "{\"index\":\"17\",\"operation\":\"guild_context\",\"offset_ms\":\"185000\"},",
            "{\"index\":\"18\",\"operation\":\"bot_membership\",\"offset_ms\":\"195000\"},",
            "{\"index\":\"19\",\"operation\":\"event_list\",\"offset_ms\":\"205000\"},",
            "{\"index\":\"20\",\"operation\":\"identity\",\"offset_ms\":\"215000\"},",
            "{\"index\":\"21\",\"operation\":\"guild_context\",\"offset_ms\":\"225000\"},",
            "{\"index\":\"22\",\"operation\":\"bot_membership\",\"offset_ms\":\"235000\"},",
            "{\"index\":\"23\",\"operation\":\"event_list\",\"offset_ms\":\"245000\"},",
            "{\"index\":\"24\",\"operation\":\"identity\",\"offset_ms\":\"255000\"},",
            "{\"index\":\"25\",\"operation\":\"guild_context\",\"offset_ms\":\"265000\"},",
            "{\"index\":\"26\",\"operation\":\"bot_membership\",\"offset_ms\":\"275000\"},",
            "{\"index\":\"27\",\"operation\":\"event_list\",\"offset_ms\":\"285000\"},",
            "{\"index\":\"28\",\"operation\":\"identity\",\"offset_ms\":\"295000\"},",
            "{\"index\":\"29\",\"operation\":\"guild_context\",\"offset_ms\":\"305000\"},",
            "{\"index\":\"30\",\"operation\":\"bot_membership\",\"offset_ms\":\"315000\"},",
            "{\"index\":\"31\",\"operation\":\"event_list\",\"offset_ms\":\"325000\"},",
            "{\"index\":\"32\",\"operation\":\"identity\",\"offset_ms\":\"335000\"},",
            "{\"index\":\"33\",\"operation\":\"guild_context\",\"offset_ms\":\"345000\"},",
            "{\"index\":\"34\",\"operation\":\"bot_membership\",\"offset_ms\":\"355000\"},",
            "{\"index\":\"35\",\"operation\":\"event_list\",\"offset_ms\":\"365000\"},",
            "{\"index\":\"36\",\"operation\":\"identity\",\"offset_ms\":\"375000\"},",
            "{\"index\":\"37\",\"operation\":\"guild_context\",\"offset_ms\":\"385000\"},",
            "{\"index\":\"38\",\"operation\":\"bot_membership\",\"offset_ms\":\"395000\"},",
            "{\"index\":\"39\",\"operation\":\"event_list\",\"offset_ms\":\"405000\"}",
            "]"
        );
        assert_eq!(encode_slots(&plan()).as_deref(), Some(expected));
    }

    #[test]
    fn rejects_non_plan_slots_at_every_position() {
        let valid = plan();
        assert_eq!(encode_slots(&valid[..39]), None);
        let mut extra = valid.clone();
        extra.push(valid[0]);
        assert_eq!(encode_slots(&extra), None);
        let mut swapped = valid.clone();
        swapped.swap(0, 1);
        assert_eq!(encode_slots(&swapped), None);
        for position in 0..40 {
            let mut changed = valid.clone();
            changed[position].index = valid[(position + 1) % 40].index;
            assert_eq!(encode_slots(&changed), None);
            changed = valid.clone();
            changed[position].operation = valid[(position + 1) % 40].operation;
            assert_eq!(encode_slots(&changed), None);
            changed = valid.clone();
            changed[position].due_after_seconds += 1;
            assert_eq!(encode_slots(&changed), None);
        }
    }
}
