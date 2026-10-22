//! Four-row substitution pins for creation-receipt overwrite validation.
//!
//! Every duplicate/wrong-kind case in the receipt suite appends a fifth
//! overwrite row, so the length gate rejects before the row loop runs. These
//! pins keep exactly four rows, proving the duplicate check and the id+kind
//! target match each refuse a receipt on their own.
use serde_json::{json, Value};
use twilight_model::guild::Permissions;
use two_bot_discord::executor::TicketChannelRequest;

fn request() -> TicketChannelRequest<'static> {
    TicketChannelRequest {
        guild_id: "100",
        category_id: "200",
        staff_role_id: "300",
        bot_id: "400",
        opener_id: "500",
        username: "member",
        reservation_id: "reservation",
    }
}

fn receipt(overwrites: Value) -> Value {
    json!({
        "id": "600",
        "guild_id": "100",
        "parent_id": "200",
        "type": 0,
        "topic": "two-ticket:reservation",
        "permission_overwrites": overwrites,
    })
}

fn valid_overwrites() -> Value {
    let common = (Permissions::VIEW_CHANNEL
        | Permissions::SEND_MESSAGES
        | Permissions::READ_MESSAGE_HISTORY)
        .bits();
    json!([
        {"id":"100","type":0,"allow":"0","deny":Permissions::VIEW_CHANNEL.bits().to_string()},
        {"id":"400","type":1,"allow":(common | Permissions::MANAGE_CHANNELS.bits()).to_string(),"deny":"0"},
        {"id":"500","type":1,"allow":common.to_string(),"deny":"0"},
        {"id":"300","type":0,"allow":common.to_string(),"deny":"0"},
    ])
}

#[test]
fn valid_four_row_receipt_is_accepted() {
    assert_eq!(
        request().verify_receipt(&receipt(valid_overwrites())),
        Ok("600".to_owned())
    );
}

#[test]
fn duplicate_overwrite_in_place_is_refused() {
    // Replace the @everyone view-deny row with a second staff row: four rows,
    // no deny, one duplicate (role, 300) principal.
    let mut rows = valid_overwrites();
    rows[0] = rows[3].clone();
    assert!(request().verify_receipt(&receipt(rows)).is_err());
}

#[test]
fn flipped_overwrite_kind_in_place_is_refused() {
    // Same opener id, wrong principal kind: a role grant is not the opener
    // member, even when the permission bits match.
    let mut rows = valid_overwrites();
    rows[2]["type"] = json!(0);
    assert!(request().verify_receipt(&receipt(rows)).is_err());
}
