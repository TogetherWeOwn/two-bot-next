//! In-process policy acceptance with a scripted Discord double. The evidence
//! fixture models already-claimed rows; it does not test database durability,
//! concurrency, gateway translation or the future REST adapter.

use std::collections::{HashSet, VecDeque};

use two_bot_core::{
    plan_quarantine, quarantine_outcome, role_removal_status, ClaimedContainmentEvent,
    ContainmentEventState, ContainmentIncident, ContainmentIncidentState, ContainmentPolicy,
    ContainmentRole, DestructiveAction, DestructiveAuditEvent, QuarantineFailure, QuarantinePlan,
};

const NOW: i64 = 1_000_000;

#[derive(Default)]
struct MockDiscord {
    responses: VecDeque<Result<u16, QuarantineFailure>>,
    requests: Vec<String>,
    confirmed: Vec<String>,
}

impl MockDiscord {
    fn execute(&mut self, plan: QuarantinePlan) -> ContainmentIncidentState {
        match plan {
            QuarantinePlan::DryRun => ContainmentIncidentState::DryRun,
            QuarantinePlan::Refused { .. } => ContainmentIncidentState::Refused,
            QuarantinePlan::RemoveRoles { role_ids } => {
                let mut removed = Vec::new();
                for id in role_ids {
                    self.requests.push(id.clone());
                    let result = self
                        .responses
                        .pop_front()
                        .unwrap_or(Ok(204))
                        .and_then(role_removal_status);
                    if let Err(error) = result {
                        return quarantine_outcome(&removed, Some(error));
                    }
                    self.confirmed.push(id.clone());
                    removed.push(id);
                }
                quarantine_outcome(&removed, None)
            }
        }
    }
}

#[derive(Default)]
struct ClaimedEvidenceFixture {
    ids: HashSet<String>,
    rows: Vec<ClaimedContainmentEvent>,
    incidents: Vec<ContainmentIncident>,
}

impl ClaimedEvidenceFixture {
    fn observe(
        &mut self,
        policy: &ContainmentPolicy,
        event: DestructiveAuditEvent,
        now_ms: i64,
    ) -> Option<ContainmentIncident> {
        if !self.ids.insert(event.audit_entry_id.clone()) {
            return None;
        }
        let state = policy.disposition(&event, now_ms).state;
        self.rows.push(ClaimedContainmentEvent { event, state });
        let trigger = self.rows.last().unwrap();
        let incident = policy.incident_candidate(trigger, &self.rows, &self.incidents, now_ms)?;
        self.rows.last_mut().unwrap().state = ContainmentEventState::Contain;
        self.incidents.push(incident.clone());
        Some(incident)
    }

    fn complete(&mut self, id: &str, state: ContainmentIncidentState) {
        self.incidents
            .iter_mut()
            .find(|incident| incident.id == id)
            .unwrap()
            .state = state;
    }
}

fn event(id: &str, action: DestructiveAction, at: i64) -> DestructiveAuditEvent {
    DestructiveAuditEvent {
        audit_entry_id: id.into(),
        guild_id: "guild".into(),
        executor_id: Some("executor".into()),
        action,
        target_id: Some("target".into()),
        occurred_at_ms: Some(at),
    }
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn role(id: &str, position: i64, permissions: u64, managed: bool) -> ContainmentRole {
    ContainmentRole {
        id: id.into(),
        position,
        permissions,
        managed,
    }
}

fn snapshot() -> Vec<ContainmentRole> {
    vec![
        role("bot", 10, 1 << 28, false),
        role("safe", 1, 1 << 11, false),
        role("danger-a", 2, 1 << 3, false),
        role("danger-b", 3, 1 << 28, false),
        role("guild", 0, 1 << 3, false),
    ]
}

#[test]
fn weighted_threshold_and_duplicate_fixture_remove_only_dangerous_roles() {
    let policy = ContainmentPolicy::default();
    let mut fixture = ClaimedEvidenceFixture::default();
    let mut discord = MockDiscord {
        responses: [Ok(200), Ok(404)].into(),
        ..MockDiscord::default()
    };
    assert!(fixture
        .observe(
            &policy,
            event("a", DestructiveAction::ChannelDelete, NOW - 1_000),
            NOW
        )
        .is_none());
    assert!(fixture
        .observe(
            &policy,
            event("b", DestructiveAction::MemberKick, NOW - 500),
            NOW
        )
        .is_none());
    let trigger = event("c", DestructiveAction::MemberKick, NOW);
    let incident = fixture.observe(&policy, trigger.clone(), NOW).unwrap();
    let plan = plan_quarantine(
        "guild",
        &ids(&["safe", "danger-a", "guild", "danger-b"]),
        &ids(&["bot"]),
        &snapshot(),
        false,
    );
    let outcome = discord.execute(plan);
    fixture.complete(&incident.id, outcome);
    assert_eq!(outcome, ContainmentIncidentState::Contained);
    assert_eq!(discord.requests, ids(&["danger-a", "danger-b"]));
    assert_eq!(discord.confirmed, ids(&["danger-a", "danger-b"]));
    assert!(fixture.observe(&policy, trigger, NOW).is_none());
    assert_eq!(fixture.incidents.len(), 1);
    assert_eq!(
        fixture.rows.last().unwrap().state,
        ContainmentEventState::Contain
    );
}

#[test]
fn dry_run_threshold_records_incident_without_discord_writes() {
    let policy = ContainmentPolicy::new(60_000, 120_000, 3).unwrap();
    let mut fixture = ClaimedEvidenceFixture::default();
    let incident = fixture
        .observe(
            &policy,
            event("dry", DestructiveAction::ChannelDelete, NOW),
            NOW,
        )
        .unwrap();
    let mut discord = MockDiscord::default();
    let outcome = discord.execute(plan_quarantine(
        "guild",
        &ids(&["danger-a"]),
        &ids(&["bot"]),
        &snapshot(),
        true,
    ));
    fixture.complete(&incident.id, outcome);
    assert_eq!(fixture.incidents[0].state, ContainmentIncidentState::DryRun);
    assert!(discord.requests.is_empty());
}

#[test]
fn whole_plan_preflight_refuses_before_any_mock_write() {
    for (position, managed) in [(10, false), (2, true)] {
        let roles = [
            role("bot", 10, 0, false),
            role("danger-a", 1, 1 << 3, false),
            role("blocked", position, 1 << 28, managed),
        ];
        let plan = plan_quarantine(
            "guild",
            &ids(&["danger-a", "blocked"]),
            &ids(&["bot"]),
            &roles,
            false,
        );
        let mut discord = MockDiscord::default();
        assert_eq!(discord.execute(plan), ContainmentIncidentState::Refused);
        assert!(discord.requests.is_empty());
    }
}

#[test]
fn partial_removal_stops_and_uncertain_incident_blocks_after_cooldown() {
    let policy = ContainmentPolicy::new(1_000, 120_000, 3).unwrap();
    let mut fixture = ClaimedEvidenceFixture::default();
    let incident = fixture
        .observe(
            &policy,
            event("partial", DestructiveAction::RoleDelete, NOW),
            NOW,
        )
        .unwrap();
    let mut discord = MockDiscord {
        responses: [Ok(204), Ok(403), Ok(204)].into(),
        ..MockDiscord::default()
    };
    let mut roles = snapshot();
    roles.push(role("danger-c", 4, 1 << 2, false));
    let outcome = discord.execute(plan_quarantine(
        "guild",
        &ids(&["danger-a", "danger-b", "danger-c"]),
        &ids(&["bot"]),
        &roles,
        false,
    ));
    fixture.complete(&incident.id, outcome);
    assert_eq!(outcome, ContainmentIncidentState::Uncertain);
    assert_eq!(discord.confirmed, ids(&["danger-a"]));
    assert_eq!(discord.requests, ids(&["danger-a", "danger-b"]));
    assert_eq!(discord.responses.len(), 1, "no next role and no retry");
    assert!(fixture
        .observe(
            &policy,
            event("later", DestructiveAction::RoleDelete, NOW + 2_000),
            NOW + 2_000
        )
        .is_none());
    assert_eq!(fixture.incidents.len(), 1);
    assert_eq!(discord.requests.len(), 2);
}

#[test]
fn transient_failure_has_no_automatic_retry_or_second_incident() {
    for response in [
        Err(QuarantineFailure::Timeout),
        Err(QuarantineFailure::Unavailable),
        Ok(429),
        Ok(503),
    ] {
        let policy = ContainmentPolicy::new(1_000, 120_000, 3).unwrap();
        let mut fixture = ClaimedEvidenceFixture::default();
        let incident = fixture
            .observe(
                &policy,
                event("first", DestructiveAction::RoleDelete, NOW),
                NOW,
            )
            .unwrap();
        let mut discord = MockDiscord {
            responses: [response, Ok(204)].into(),
            ..MockDiscord::default()
        };
        let outcome = discord.execute(plan_quarantine(
            "guild",
            &ids(&["danger-a", "danger-b"]),
            &ids(&["bot"]),
            &snapshot(),
            false,
        ));
        fixture.complete(&incident.id, outcome);
        assert_eq!(outcome, ContainmentIncidentState::Uncertain);
        assert_eq!(discord.requests, ids(&["danger-a"]));
        assert!(discord.confirmed.is_empty());
        assert_eq!(discord.responses.len(), 1);
        assert!(fixture
            .observe(
                &policy,
                event("later", DestructiveAction::RoleDelete, NOW + 2_000),
                NOW + 2_000
            )
            .is_none());
    }
}
