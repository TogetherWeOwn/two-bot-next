# Security gates for vote-kick, template assistant, RSVP/attendance and slash input bounds

This checklist is the security acceptance contract for runtime wiring of these
features and for slash-command input handling (IN- rows). A published command, pure-core test or merged documentation change
is not proof that its runtime path satisfies the gates. Requirements already
covered by code still need exact-head evidence; unresolved requirements stay
open. Slash-command input-handling PRs touching `/lfg`, `/command`, `/schedule*`, `/sticky` or `/feed*` cite the IN- rows the same way wiring PRs cite VK/RA/TA rows. This document does not authorize a feature enablement or production test.

## Source boundary

At source revision `b846a6094`, vote-kick and RSVP/attendance have runtime
handlers; they are not wholly unwired. `/templateassistant` stays unpublished
until its slice lands, and has no handler or HTTP transport yet. Apply these gates to hardening existing
paths as well as new wiring. This is source inspection, not live verification.

## Vote-kick

Keep the owner/original-creator protection, room and guild boundaries, strict
majority and one-ballot-per-member rules in
[the core contract](voice-vote-kick-core.md). In addition:

| ID | Required control | Minimum regression evidence |
| --- | --- | --- |
| VK-01 | Protect targets with effective **Kick Members** or **Administrator** permission, as well as the room owner and original creator. Resolve target authority in the interaction's guild, fail closed when it is unavailable, and recheck before enforcement so a promotion during a vote cannot be bypassed. | Deny each protected target separately at start and after a mid-vote promotion; an unavailable authority lookup causes no disconnect or permission edit. An ordinary target can still be voted on. Core: `protected_boundary_owner_admin_and_ordinary_member`, `privileged_promotion_mid_vote_cancels_before_enforcement`, `unavailable_lookup_mid_vote_cancels_without_enforcement` (`crates/core/src/voice_vote_kick.rs`); handler: `kick_members_and_administrator_targets_are_refused_with_no_effect` (`privileged_target` refusal row), `unavailable_target_authority_fails_closed_with_no_effect` (`authority_unavailable` refusal row), `promoted_target_mid_vote_cancels_and_enforces_nothing`, `unavailable_authority_mid_vote_cancels_and_enforces_nothing`, `promoted_target_after_pass_is_not_touched_on_dispatch` (`skipped_target_protected`), `unavailable_authority_after_pass_is_not_touched_on_dispatch` (`skipped_target_protected`), `ordinary_target_remains_votable_end_to_end` (`crates/bot/src/voice_kick_tests.rs`). |
| VK-02 | Ship a finite post-terminal cooldown and a per-initiator limit across targets. Pin the duration, limit, keys and pass/defeat/expiry/cancel behavior in the implementation and tests; a two-minute active-vote window alone is not a post-terminal cooldown. | Implemented: `VOTE_KICK_COOLDOWN_MS = 300_000` per guild + target after pass, expiry or either cancellation (expiry counts from the deadline, backdated when observed late); `VOTE_KICK_INITIATOR_LIMIT = 3` successful starts per `VOTE_KICK_INITIATOR_WINDOW_MS = 600_000` per guild + initiator, across targets and rooms; refusal order `ActiveVoteExists`, then `Cooldown`, then `InitiatorLimited`. A defeated (all-No) vote stays Active until its deadline. Core (`crates/core/src/voice_vote_kick.rs`): `passed_decision_is_room_scoped_and_never_emitted_on_replay`, `target_departure_cancels_and_rejoin_cannot_revive_it`, `elapsed_vote_does_not_block_new_vote_and_old_buttons_stay_expired`. Acceptance (`crates/core/tests/voice_vote_cooldown.rs`): `window_expiry_re_enables_a_fresh_vote_only_after_the_cooldown` (just-before/exactly-at boundaries, refused starts reserve nothing), `cooldown_after_a_pass_blocks_the_same_target_but_not_other_targets`, `cooldown_after_a_protective_cancellation_with_validation_precedence`, `votes_in_room_a_never_block_other_targets_in_room_a_or_room_b`, `target_moving_to_room_b_is_released_by_room_a_departure_refresh`, `initiator_cap_spans_targets_and_rooms_but_not_guilds`, `target_cooldown_precedes_the_initiator_cap`, `defeated_vote_cannot_be_reraised_against_the_target_until_its_window_ends`. Every refusal creates no vote, ballot, enforcement decision or cap entry. |
| VK-03 | Bound all retained vote state: `VoteKickCore::votes`, including terminal votes and their ballots, active-vote tracking, `vote_refs` and `vote_initiators`. Define coordinated retention across these stores. Pruning must not reopen a replay or drop an unresolved enforcement fence. If safe capacity cannot be recovered, refuse new admissions rather than grow without limit or forget an accepted interaction. | Implemented as bounded in-memory retention, not durable storage. The core keeps a terminal vote (and its `ReusedVoteId` replay rejection) only through `terminal_at + VOTE_KICK_COOLDOWN_MS` inclusive; every public method settles elapsed votes, evicts entries strictly past the horizon and prunes initiator starts past the 10-minute window, reporting every eviction through `prune`/`drain_evicted`. Past the horizon the vote is forgotten and its interaction ID may start a new vote (safe because Discord interaction IDs are unique per interaction; only a delayed duplicate delivery could collide, and those arrive within seconds). Core: `expired_cooldown_entries_reaped_without_new_starts`, `retained_state_stays_bounded_across_sustained_churn`, `evicted_terminal_id_may_start_a_fresh_vote` (`crates/core/src/voice_vote_kick.rs`); acceptance: `timer_prune_reaps_expired_state_without_new_starts`, `sustained_churn_keeps_retained_state_proportional_to_the_live_window` (`crates/core/tests/voice_vote_cooldown.rs`). The worker drops its per-vote `vote_refs`/`vote_initiators` maps for every evicted vote (`drop_evicted_vote_maps` after every core call; the timer settles every live vote before reaping), keeps the initiator fenced while its enforcement is still queued or the vote is still tracked live, and neither settles nor reaps while live evidence is not authoritative. Runtime (`crates/bot/src/voice_kick_tests.rs`): `timer_prune_reaps_evicted_vote_refs_past_the_cooldown_horizon`, `command_path_eviction_reports_to_the_worker_maps`, `queued_enforcement_keeps_its_initiator_past_the_horizon`, `outage_gates_reaping_and_reconnect_settles_before_it_reaps`, `long_outage_audits_every_expired_vote_not_just_the_first`, `start_before_first_ready_tick_does_not_swallow_the_expiry_audit`. The ledger does not survive restart: the core and worker maps are in-memory only (`GuildRoomWorker::load` starts them empty with no vote hydration), so bounded memory is not a restart replay guarantee, and no test replays a pre-restart vote ID against a fresh worker. |
| VK-04 | Render the public reason as bounded, mention-safe plain text without clickable links, markdown formatting or embeds under the bot's name. Do not echo raw input in errors or logs. | Everyone/role/user mentions, disguised links, bare URLs, markdown and overlong reasons cannot create a ping, link, embed or formatted bot endorsement in the final Discord payload. Proved by `hostile_reasons_render_as_mention_safe_plain_text` (ballot payload) in `crates/bot/src/voice_kick_tests.rs`; ordinary text passes through via `ordinary_reasons_render_intact_and_absent_reason_renders_no_line`. Refusal, error and log paths are reason-free by construction: `kick_start` takes no reason parameter, refusals render only from the `KickRefusal` enum via `kick_refusal_text` (fixed `&'static str` per variant, all 17 pinned by `kick_refusals_never_echo_initiator_text`), audit rows carry fixed outcome codes plus snowflakes, and follow-up ballot edits never take the reason; `hostile_reasons_never_echo_in_refusals_errors_or_logs` holds the hostile matrix in scope over those paths as a regression tripwire. |

The post-terminal cooldown (VK-02) and bounded retention (VK-03) have shipped
with the limits pinned in the rows above. The [cooldown checklist](voice-vote-kick-cooldown-checklist.md)
`Match` results record source conformance of the active-vote window, cooldown,
initiator limit and bounded replay horizon against that implementation; they are
evidence for the implemented limits, not approval to change them. Durable replay
retention across restarts remains a parent obligation and is explicitly not
promised: the ledger is in-memory only and post-restart replay is unverified.

## `/templateassistant`

The [config/command contract](voice-assistant-core.md) describes a pure-core
gate, not a secure HTTP transport. Before adding the handler or endpoint call:

| ID | Required control | Minimum regression evidence |
| --- | --- | --- |
| TA-01 | Use HTTPS to an explicitly approved, environment-fixed host/origin; interaction input and dashboard settings cannot choose a destination. Reject userinfo, plaintext, unapproved ports/hosts and private, loopback, link-local or reserved destinations. Validate the resolved destination too, including DNS changes; never follow redirects. | Invalid config disables the path before any request. Public-host-to-private-address resolution and every redirect class make no follow-up request. Test the actual transport, not only a URL string predicate. |
| TA-02 | Bind the endpoint credential through the approved runtime secret path before enabling calls. Wrap credentials and credential-bearing endpoints in `Secret<T>`; expose only at the authorization/transport boundary. Never put credentials in a request option, flag allowlist, dashboard value or URL. | Missing binding fails closed. Synthetic sentinels never appear in normal/pretty/nested Debug, Display, errors, logs, replies or remote-response diagnostics. |
| TA-03 | Enforce runtime Manage Guild authority and a finite per-user request limit before HTTP or quota consumption, alongside the per-guild monthly cap. Bound concurrent requests, request/response size and timeout; define charging and retry behavior so retries cannot multiply calls or cap consumption. | Same-user bursts and concurrent calls across guilds hit the chosen per-user limit; unauthorized, over-limit and cap-exhausted requests make no HTTP call. Boundary, timeout and retry tests pin resource/charging behavior. |
| TA-04 | Send only the validated request payload. Keep member IDs, names and presence out of the provider request. Treat responses as untrusted: use the validation and Apply/Refine/Cancel flow before changing a template, and return redacted, bounded errors rather than endpoint URLs or remote bodies. | Capture the serialized request and exercise hostile/malformed/oversized responses, failed validation and cancellation. None applies a template or leaks endpoint/credential details. |

Accepting a URL in `AssistantConfig::from_map` is not TA-01 compliance. A
per-guild monthly cap is not a per-user request limit. Endpoint credentials
must not be supplied merely to demonstrate this documentation change.

## RSVP and attendance

Snowflake syntax and a syntactically valid occurrence string are not authority
or existence checks. Apply the mutation gates before
[`rsvp_store::put_rsvp`, `write_audit` or `record_checkin`](../crates/core/src/rsvp_store.rs).
The read-only `/rsvp-attendance` path uses `list_rsvps`: preserve its historical
totals after event cancellation or deletion, without requiring a live-event lookup.
Its ephemeral reply also shows only the invoking member's saved response and
Discord-localized saved time, or explicit absence, from those same guild/event
rows. The event ID identifies stored history, not verified live availability;
no event link or attendee roster is emitted. Reads neither write nor consume
RSVP-write quota. Analytics opt-out retains transactional RSVP history; member
erasure removes it. Store failures refuse rather than claiming absence.
The protected `saved_response_acceptance` module in `rsvp_runtime` exercises
these boundaries through the shared router, executor and disposable store.

| ID | Required control | Minimum regression evidence |
| --- | --- | --- |
| RA-01 | Before RSVP or attendance mutations, verify that the event exists and belongs to the interaction's guild, and that the acting member currently belongs to that guild. Fail closed on missing/stale membership or event lookup failures before any mutation or associated audit write. This live-event gate does not apply to historical totals reads. | Valid-looking nonexistent IDs, another guild's event, departed members, DM invocations and failed lookups leave RSVP, attendance and associated audit row counts unchanged. A valid same-guild member/event succeeds. Historical `/rsvp-attendance` totals remain readable after cancellation/deletion, make no event lookup and write no rows. |
| RA-02 | Resolve the supplied occurrence through a trusted, guild-scoped event/occurrence binding, not arbitrary caller text. Preserve runtime **Manage Events** authority for every `/attendance` host check-in, including when the actor selects themself, and verify the selected target's current guild membership. Selecting oneself must not create an unprivileged self-check-in path or downgrade `AttendanceProof::HostCheckin`. | Unknown/cross-event occurrences, forged user IDs, cross-guild targets and unauthorized host check-ins create no attendance or audit rows. Explicitly test unauthorized self and on-behalf refusals, and authorized self and on-behalf paths that retain HostCheckin proof. |
| RA-03 | Bound admissions and storage growth even for valid events: pin per-user rate/cap limits and occurrence bounds, deduplicate retries, and define retention for RSVP and audit rows without deleting required recovery/audit evidence. Keep validation and writes race-safe. | Burst, duplicate and concurrent requests have bounded row counts. Event/occurrence removal or membership loss between lookup and write is refused under the chosen consistency contract; lookup/store errors do not become successful check-ins. |

The current [`event-occurrence` command contract](commands.md#attendance) states
the trusted rule: a bare scheduled-event id binds the event itself, any other
text must anchor as `{event_id}:{label}`, and bare slugs refuse. The
[parity contract](parity.md) records the bare-slug refusal as an intentional
security difference; the anchored free-text format is preserved, not removed.
Trusted resolution is defined and tested by the RA-02 hardening change, pinned
by `crates/core/src/rsvp.rs::tests::attendance_occurrence_binds_every_format_to_a_live_event_anchor`
(shape, refusal classes, no-echo refusal) and the
`crates/discord/tests/rsvp_runtime.rs` acceptance battery
(`unknown_and_cross_guild_occurrences_refuse_without_writes`,
`host_checkin_authorized_self_and_on_behalf_retain_host_checkin_proof`,
`permissions_gates_and_guild_fence_precede_store_access`,
`malformed_inputs_and_failed_ack_do_not_write`,
`non_member_and_unverifiable_targets_refuse_without_writes`), which assert zero
attendance/audit writes on every refusal. Two refusal paths are exceptions. A
refusal after post-write revalidation whose compensating delete also fails
leaves the attempt's rows; `failed_compensation_keeps_refusal_and_emits_one_diagnostic`
pins them and the `compensation_failed` diagnostic. A required audit write commits
in the same transaction as its RSVP response, pinned by
`audit_insert_failure_on_first_rsvp_leaves_no_response_or_audit` and
`audit_insert_failure_on_rsvp_rewrite_preserves_exact_prior_state`
(`crates/discord/tests/rsvp_atomic_audit.rs`): a failed audit insert rolls the
response back and refuses with the existing safe text. An unknown stored status
after commit still refuses without compensation, leaving the committed row; it
needs a row that bypassed the status CHECK, and no test pins it either.
Restricting input further to Discord-derived IDs alone would still require an explicit parity decision, a
recorded intentional difference and coordinated command reference, fixture and
regression updates. This checklist grants no such waiver and does not change
the accepted formats.

The store functions above are persistence primitives, not permission checks.
Exercise the actual interaction-to-store path with a disposable test database;
a store-only happy-path test cannot establish RA-01 or RA-02.

## Slash input bounds

The input-bound fix (PR #694) closed four gaps found in an internal
slash-command review: an unbounded `/lfg` role-key echo plus a raw reply
path that could abandon the deferral, unbounded custom-command audit
`target_key` values, empty/overlong schedule-id prefix matching, and
published string options with no `max_length` while handlers enforce caps
server-side. These rows record those gates so the next review does not
start from zero. They change no bound; the code and tests cited are the
implementation.

| ID | Required control | Minimum regression evidence |
| --- | --- | --- |
| IN-01 | Bound the `/lfg` role-key echo to a 128-character prefix of the raw key and reject role specs over the published UTF-16 bound (`MAX_ROLE_SPEC_CHARS`, 2339) before parsing. Route the LFG failure reply through the sanitizing interaction edit (truncate to the message bound with neutral mentions, `allowed_mentions` parse `[]`) so the deferred interaction never hangs thinking and hostile mentions never echo. | `crates/core/src/lfg.rs::tests::role_key_errors_echo_only_a_bounded_raw_prefix` (echo capped at 128 chars) and `role_spec_rejects_input_over_the_published_utf16_bound` (ASCII and multibyte over-long refused); hostile tail of `crates/discord/tests/lfg_interactions.rs::router_runs_create_signup_full_switch_leave_close_with_audit` (reply is `PATCH`, content within `MAX_MESSAGE_CHARS`, no `@everyone`, `parse == []`). |
| IN-02 | Cap custom-command audit `target_key` values at the command-name bound (`MAX_COMMAND_NAME_CHARS`, 32 chars) for put, reject, delete and run rows, so unbounded `/command name` input never lands raw in audit rows. | `crates/core/src/custom_commands.rs::tests::rejected_and_absent_audits_bound_untrusted_names` (overlong multibyte name bounded to 32 chars on reject and absent-delete paths) and `crates/core/src/custom_commands.rs::tests::successful_audits_bound_target_key_by_characters_at_the_name_limit` (put created and updated, ok delete, ok and failed run rows; under-limit, exact, one-over and overlong multibyte names bounded by characters, with action, outcome and reason pinned). Every audit constructor that writes `target_key` uses the shared `bounded_audit_name` helper (`custom_commands.rs:360,383,395,407`), and each is pinned by an executed overlong-name assertion. |
| IN-03 | Refuse empty and overlong (`MAX_RESOURCE_ID_CHARS`, 128 UTF-16 units) schedule-id prefixes in the shared schedule resolver (`scheduled::resolve_scheduled_id`) and the store wrapper (`scheduled_store::resolve_scheduled_id`) before any database access. The `/schedule-remove` handler guards (`schedule_runtime.rs:194-210`) and the `/lfg-close` guard (`lfg::valid_post_id` in `LfgInteractions::execute`) apply the same bounds at the interaction edge and mutate nothing: empty prefixes return the shared no-unique-match text, the overlong `/schedule-remove` reply is a hard-coded literal with the same suffix, and `/lfg-close` refuses with its own 1-to-128-characters message. | `crates/core/tests/schedule_queue_order.rs::prefix_resolution_refuses_missing_and_ambiguous_ids` (empty and 129-char prefixes resolve `Missing`, shared reply pins the suffix); `crates/core/src/scheduled_store.rs::tests::empty_and_overlong_prefixes_refuse_before_database_access` (lazy pool proves refusal happens before database access); `crates/bot/src/command_runtime_tests.rs::schedule_remove_invalid_ids_refuse_before_database_access` (missing/empty plus 129-unit ASCII, 129-unit multibyte, 130-unit surrogate and hostile overlong ids through the real router against a refused lazy pool: documented refusal, never the store failure, defer+PATCH only, hostile tail never echoed) and `crates/bot/src/command_runtime_tests.rs::schedule_remove_invalid_ids_leave_store_untouched` (disposable DB: the same invalid matrix plus the whitespace-only missing-ID control and two exact-128-unit boundary controls leave schedule rows and audit rows unchanged with defer+PATCH only; a valid-prefix control still deletes exactly one row). `/lfg-close` is pinned by `crates/discord/tests/lfg_interactions.rs::router_refuses_invalid_close_ids_without_store_or_channel_mutation` (empty, 129/300-char ASCII, 65-emoji and 129-e-acute multibyte overlong in UTF-16 units, hostile `@everyone` tail and the stored 129-char row refuse with the fixed 1-to-128 message; 100-e-acute byte control, missing row and whitespace-only keep the missing-row outcome; exact 128-ASCII and 64-emoji boundaries plus the ordinary post close; each refusal asserts unchanged posts/roles/signups/audits and zero `/api/v10/channels/` requests with exactly one deferred ack plus one mention-safe edit). |
| IN-04 | Advertise `max_length` on published string options equal to the runtime caps: `/command` name 32, template 2000, description 100, text-trigger 33, `/command-remove` name 32; `/schedule` body 2000, `/schedule-remove` id 128, `/schedule-retime` id 128 and at 64, `/sticky` body 2000; `/lfg` title 100, roles 2339, `/lfg-close` id 128; `/feed-add` source 2048 bytes, `/feed-remove` id 128, `/feed-pause` id 128, `/feed-resume` id 128. Server-side validators stay authoritative; picker bounds only restrict what Discord sends. | `crates/core/src/feature_commands.rs` automation/announcement `max_length` assertions (each option pinned to its constant); `crates/core/tests/support/registry_parity.rs::expected_registry` caps plus `crates/core/tests/registry_golden.rs::diff_detects_each_kind_of_unlisted_drift_including_exception_bodies` (drift on any unlisted `max_length` change); `crates/core/tests/feeds.rs::source_length_bound_matches_published_byte_limit_before_trimming` (byte ceiling enforced before trimming, multibyte over-ceiling refused). |

LFG Select IDs also use the IN-03 resource-ID bound in both
`lfg::parse_lfg_select` and `LfgInteractions::execute`, including direct callers.
`crates/discord/tests/lfg_select_audit_acceptance.rs` covers signup and leave
refusal before I/O, UTF-16 boundaries, applied-but-unaudited replies and
request-bound replay. See the [LFG interaction contract](lfg-interaction-contract.md)
for audit-loss limitations and conservative refusal of unbound legacy receipts.

## Wiring PR evidence contract

Each wiring or hardening PR must:

- Link this public checklist and list the requirement IDs affected by the slice.
  Keep private coordination references in the private tracker, not the PR.
- Map each affected ID to code and named regression tests on the exact head SHA.
  Cover a valid path, refusal classes and boundary/race/replay cases where relevant.
- Prove that refusals produce none of the effects prohibited by the affected
  gate, using captured HTTP/Discord writes or before/after database row counts,
  not only an error enum. Distinguish bounded refusal acknowledgements and
  permitted redacted security auditing from vote/enforcement/provider/template
  effects. RA-01 and RA-02 still require no RSVP/attendance or associated audit
  row writes on refusal.
- State unresolved IDs explicitly. A partial implementation is not approval to
  expose a path that depends on those missing controls.
- Distinguish source inspection, hermetic test execution and authorized staging
  evidence. Report commands/results and tests not run; never test against production.
- Pass the repository's required checks and independent review on that same head.
  Feature enablement and deployment retain their separate approval gates.

This documentation-only change implements none of the controls and makes no
runtime, staging or production security-compliance claim.
