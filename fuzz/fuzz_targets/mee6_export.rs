#![no_main]

use libfuzzer_sys::fuzz_target;
use two_bot_cutover::{mee6_rewards, mee6_xp};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Exercise JSON envelope variants, numeric fields, duplicate IDs and the
    // roles-snapshot form. These parsers never connect to Discord or a store.
    let _ = mee6_xp::parse_mee6_export(text);
    let _ = mee6_rewards::parse_mee6_role_rewards(text);
    let _ = mee6_rewards::parse_roles_snapshot(text);
});
