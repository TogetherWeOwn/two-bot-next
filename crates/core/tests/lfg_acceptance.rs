//! LFG reserved leave-action role-key refusal acceptance (TOG-12869).
//!
//! Pins parity section 13 gap (legacy 8b5d1e1, #414): LFG role keys must not
//! collide with the reserved leave action (`LFG_LEAVE_VALUE = "__leave__"`).
//! All assertions go through the public `two_bot_core::lfg` API only.
//! Pure offline: no DB, REST, router, timers, or feature flags. Runtime wiring
//! (commands/selects, defer-before-I/O) stays on TOG-10260.

use two_bot_core::lfg::{parse_role_spec, valid_role_key, RoleSpecError, LFG_LEAVE_VALUE};

#[test]
fn reserved_leave_value_is_the_legacy_sentinel() {
    assert_eq!(LFG_LEAVE_VALUE, "__leave__");
}

#[test]
fn parse_role_spec_refuses_exact_reserved_key() {
    assert_eq!(
        parse_role_spec("__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey("__leave__".to_owned()))
    );
    // Reserved key refuses wherever it appears in the spec.
    assert_eq!(
        parse_role_spec("tank:Tank:1,__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey("__leave__".to_owned()))
    );
}

#[test]
fn reserved_refusal_message_names_the_reservation() {
    let err = parse_role_spec("__leave__:Leave:1").expect_err("must refuse");
    let message = err.to_string();
    assert!(
        message.contains("__leave__"),
        "message must name the key: {message}"
    );
    assert!(
        message.contains("reserved for leaving the group"),
        "message must name the reservation: {message}"
    );
}

#[test]
fn reserved_key_pattern_vs_reservation_layer() {
    // `valid_role_key` enforces only the `^[a-z0-9_-]{1,32}$` shape, so the
    // sentinel itself is shape-valid; the reservation is enforced by
    // `parse_role_spec` (ReservedKey), keeping the leave action distinct.
    assert!(valid_role_key(LFG_LEAVE_VALUE));
    assert_eq!(
        parse_role_spec("__leave__:Leave:1"),
        Err(RoleSpecError::ReservedKey(LFG_LEAVE_VALUE.to_owned()))
    );
}

#[test]
fn representative_valid_keys_accept() {
    for key in ["tank", "healer", "dps", "leave", "tank-2", "dps_1"] {
        assert!(valid_role_key(key), "{key} must be shape-valid");
        let spec = format!("{key}:Label:1");
        let roles = parse_role_spec(&spec).expect("valid key must parse");
        assert_eq!(roles.len(), 1);
        assert_eq!(roles[0].key, key);
    }
}
