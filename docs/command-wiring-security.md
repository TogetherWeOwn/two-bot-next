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
| VK-03 | Bound the active-vote and `vote_refs` replay ledgers and define their retention. Pruning must not reopen a replay or drop an unresolved enforcement fence. If safe capacity cannot be recovered, refuse new admissions rather than grow without limit or forget an accepted interaction. | At-capacity and high-churn tests assert finite ledger sizes; replay after pruning/restart is refused for the documented replay horizon, and pending enforcement remains fenced. |
| VK-04 | Render the public reason as bounded, mention-safe plain text without clickable links, markdown formatting or embeds under the bot's name. Do not echo raw input in errors or logs. | Everyone/role/user mentions, disguised links, bare URLs, markdown and overlong reasons cannot create a ping, link, embed or formatted bot endorsement in the final Discord payload. |

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
or existence checks. Before a handler calls `RsvpStore`:

| ID | Required control | Minimum regression evidence |
| --- | --- | --- |
| RA-01 | Verify that the event exists and belongs to the interaction's guild, and that the acting member currently belongs to that guild. Fail closed on missing/stale membership or event lookup failures before RSVP or audit writes. | Valid-looking nonexistent IDs, another guild's event, departed members, DM invocations and failed lookups leave RSVP and audit row counts unchanged. A valid same-guild member/event succeeds. |
| RA-02 | Resolve an occurrence from that existing event, not arbitrary caller text. Bind self-check-in to the authenticated member; checking in another member requires explicit runtime staff authority and verifies that target's guild membership too. | Unknown/cross-event occurrences, forged user IDs, cross-guild targets and unauthorized on-behalf check-ins create no attendance or audit rows. Test authorized on-behalf and self paths separately. |
| RA-03 | Bound admissions and storage growth even for valid events: pin per-user rate/cap limits and occurrence bounds, deduplicate retries, and define retention for RSVP and audit rows without deleting required recovery/audit evidence. Keep validation and writes race-safe. | Burst, duplicate and concurrent requests have bounded row counts. Event removal or membership loss between lookup and write is refused under the chosen consistency contract; lookup/store errors do not become successful check-ins. |

Store methods are persistence primitives, not permission checks. Exercise the
actual interaction-to-store path with a disposable test database; a store-only
happy-path test cannot establish RA-01 or RA-02.

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
