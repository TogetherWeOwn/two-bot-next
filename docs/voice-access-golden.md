# Voice access golden corpus (V3 / V7 / V10)

Independent QA reference for the V3b (private join requests), V7a
(alias/nick) and V10b (command access) core cards. Expectations are written
from `docs/voice-rooms.md` alone — V3 (`/private`, `/public`, the Join
channel), V7 (`/alias`, `/nick`) and V10 (the guild-level controls bullet) —
with no implementation read. Each core card wires the corpus into its tests
once both have merged; this corpus depends on no unmerged module.

- Fixture: `crates/core/tests/fixtures/voice_access_golden.json`
  (58 cases, 20 clause IDs).
- Inventory: `crates/core/tests/voice_access_golden.rs` checks fixture
  integrity (≥ 40 cases, each with clause ID + input + expected outcome),
  requires ≥ 1 positive and ≥ 1 negative case per clause, and requires every
  spec bullet in the fixture's `bullets` map to resolve to a clause.
- Polarity: **positive** = the specified effect occurs (grant, change,
  applies); **negative** = refused, absent, unchanged, or the complement
  boundary (no grant, no change, still refused).

## V3: owner room controls (privacy)

| Clause | Spec bullet | Cases (+/−) |
|---|---|---|
| `V3-PRIVATE-DENY` | `/private` denies Connect to @everyone; room stays visible | 5 (3/2) |
| `V3-EPHEMERAL` | accept: every reply is ephemeral with a clear message | 3 (2/1) |
| `V3-JOIN-CREATE` | companion channel "⇩ Join ‹owner›" created next to the room | 3 (2/1) |
| `V3-JOIN-APPROVE` | outsider join raises Approve/Deny/Block; Approve grants + moves in | 3 (2/1) |
| `V3-JOIN-DENY` | Deny refuses this request without blocking future requests | 3 (2/1) |
| `V3-JOIN-BLOCK` | Block stops further requests from that member | 4 (2/2) |
| `V3-PUBLIC-RESTORE` | `/public` restores access and deletes the Join channel | 4 (2/2) |
| `V3-PRIVACY-OWNERSHIP` | accept: privacy survives ownership changes | 3 (2/1) |
| `V3-JOIN-RENAME` | accept: Join channel follows the new owner's name | 3 (2/1) |
| `V3-JOIN-DELETE` | accept: Join channel deleted along with its room | 2 (1/1) |

## V7: aliases and nicknames

| Clause | Spec bullet | Cases (+/−) |
|---|---|---|
| `V7-ALIAS-APPLY` | aliases apply to `@@game_name@@` (majority game, after aliases) | 2 (1/1) |
| `V7-ALIAS-GAME-COND` | aliases apply to `GAME` conditions | 2 (1/1) |
| `V7-ALIAS-CRUD` | `/alias` panel adds, edits or removes aliases | 3 (2/1) |
| `V7-NICK-SET` | `/nick name`: any member sets the `@@owner@@` name for them | 3 (2/1) |
| `V7-NICK-RESET` | `/nick reset` restores the display name | 2 (1/1) |
| `V7-NICK-SCOPE` | nick is per-member display feeding `@@owner@@` only | 2 (1/1) |

## V10: guild-level controls

| Clause | Spec bullet | Cases (+/−) |
|---|---|---|
| `V10-CREATION-TOGGLE` | turn room creation on/off; commands keep working | 3 (2/1) |
| `V10-REQUIRED-ROLE` | optional role required to use room commands | 2 (1/1) |
| `V10-PER-COMMAND` | per-command role restrictions | 3 (1/2) |
| `V10-ADMIN-EXEMPT` | admins (Manage Channels) always exempt | 3 (2/1) |

## Boundary notes (not new behavior, kept explicit)

- `V3-PRIVATE-DENY-05`: private hides Connect only; the room stays visible.
- `V3-JOIN-BLOCK-03/04`: Block is member-scoped; it changes nothing for other
  outsiders and grants no access.
- `V10-ADMIN-EXEMPT-03`: the admin line is Manage Channels; Move Members alone
  does not exempt.
- `V10-PER-COMMAND-03`: per-command restriction leaves other commands working.
- `V7-NICK-SCOPE-02`: nicks feed `@@owner@@` only, never game-name rendering.
