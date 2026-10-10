# Security gates for vote-kick, template assistant and RSVP/attendance

This checklist is the security acceptance contract for runtime wiring of these
features. A published command, pure-core test or merged documentation change
is not proof that its runtime path satisfies the gates. Requirements already
covered by code still need exact-head evidence; unresolved requirements stay
open. This document does not authorize a feature enablement or production test.

## Source boundary

At source revision `b846a6094`, vote-kick and RSVP/attendance have runtime
handlers; they are not wholly unwired. `/templateassistant` can be published
but has no handler or HTTP transport. Apply these gates to hardening existing
paths as well as new wiring. This is source inspection, not live verification.

## Vote-kick

Keep the owner/original-creator protection, room and guild boundaries, strict
majority and one-ballot-per-member rules in
[the core contract](voice-vote-kick-core.md). In addition:

| ID | Required control | Minimum regression evidence |
| --- | --- | --- |
| VK-01 | Protect targets with effective **Kick Members** or **Administrator** permission, as well as the room owner and original creator. Resolve target authority in the interaction's guild, fail closed when it is unavailable, and recheck before enforcement so a promotion during a vote cannot be bypassed. | Deny each protected target separately at start and after a mid-vote promotion; an unavailable authority lookup causes no disconnect or permission edit. An ordinary target can still be voted on. |
| VK-02 | Ship a finite post-terminal cooldown and a per-initiator limit across targets. Pin the duration, limit, keys and pass/defeat/expiry/cancel behavior in the implementation and tests; a two-minute active-vote window alone is not a post-terminal cooldown. | Repeated attempts, fresh interaction IDs and target/room changes cannot evade the chosen limits. Test just before and exactly at the deadline and show refused attempts create no vote or enforcement effect. |
| VK-03 | Bound all retained vote state: `VoteKickCore::votes`, including terminal votes and their ballots, active-vote tracking, `vote_refs` and `vote_initiators`. Define coordinated retention across these stores. Pruning must not reopen a replay or drop an unresolved enforcement fence. If safe capacity cannot be recovered, refuse new admissions rather than grow without limit or forget an accepted interaction. | At-capacity and high-churn tests assert finite counts of active/terminal votes, retained ballots and both runtime maps; replay after pruning/restart is refused for the documented replay horizon, and pending enforcement remains fenced. |
| VK-04 | Render the public reason as bounded, mention-safe plain text without clickable links, markdown formatting or embeds under the bot's name. Do not echo raw input in errors or logs. | Everyone/role/user mentions, disguised links, bare URLs, markdown and overlong reasons cannot create a ping, link, embed or formatted bot endorsement in the final Discord payload. Proved by `hostile_reasons_render_as_mention_safe_plain_text` (ballot payload) in `crates/bot/src/voice_kick_tests.rs`; ordinary text passes through via `ordinary_reasons_render_intact_and_absent_reason_renders_no_line`. Refusal, error and log paths are reason-free by construction: `kick_start` takes no reason parameter, refusals render only from the `KickRefusal` enum via `kick_refusal_text` (fixed `&'static str` per variant, all 17 pinned by `kick_refusals_never_echo_initiator_text`), audit rows carry fixed outcome codes plus snowflakes, and follow-up ballot edits never take the reason; `hostile_reasons_never_echo_in_refusals_errors_or_logs` holds the hostile matrix in scope over those paths as a regression tripwire. |

The [cooldown checklist](voice-vote-kick-cooldown-checklist.md) records the
existing active-vote window and the intentional **no post-terminal cooldown**
gap. Its `Match` result is not fulfillment of VK-02. Agree and test the new
cooldown semantics before treating that requirement as complete.

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
attendance/audit writes on every refusal. Restricting input further to
Discord-derived IDs alone would still require an explicit parity decision, a
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
| IN-02 | Cap custom-command audit `target_key` values at the command-name bound (`MAX_COMMAND_NAME_CHARS`, 32 chars) for put, reject, delete and run rows, so unbounded `/command name` input never lands raw in audit rows. | `crates/core/src/custom_commands.rs::tests::rejected_and_absent_audits_bound_untrusted_names` (overlong multibyte name bounded to 32 chars on reject and absent paths). |
| IN-03 | Refuse empty and overlong (`MAX_RESOURCE_ID_CHARS`, 128 UTF-16 units) schedule-id prefixes before any store resolution, on `/schedule-remove`, the shared schedule resolver and `/lfg-close`. Refusals on the schedule surfaces return the shared no-unique-match outcome (the `/lfg-close` guard refuses with its own 1-to-128-characters message); all mutate nothing and make no database access. | `crates/core/tests/schedule_queue_order.rs::prefix_resolution_refuses_missing_and_ambiguous_ids` (empty and 129-char prefixes resolve `Missing`); `crates/core/src/scheduled_store.rs::tests::empty_and_overlong_prefixes_refuse_before_database_access` (lazy pool proves refusal happens before database access). |
| IN-04 | Advertise `max_length` on published string options equal to the runtime caps: `/command` name 32, template 2000, description 100, text-trigger 33, `/command-remove` name 32; `/schedule` body 2000, `/schedule-remove` id 128, `/sticky` body 2000; `/lfg` title 100, roles 2339, `/lfg-close` id 128; `/feed-add` source 2048 bytes, `/feed-remove` id 128. Server-side validators stay authoritative; picker bounds only restrict what Discord sends. | `crates/core/src/feature_commands.rs` automation/announcement `max_length` assertions (each option pinned to its constant); `crates/core/tests/support/registry_parity.rs::expected_registry` caps plus `crates/core/tests/registry_golden.rs::diff_detects_each_kind_of_unlisted_drift_including_exception_bodies` (drift on any unlisted `max_length` change); `crates/core/tests/feeds.rs::source_length_bound_matches_published_byte_limit_before_trimming` (byte ceiling enforced before trimming, multibyte over-ceiling refused). |

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
