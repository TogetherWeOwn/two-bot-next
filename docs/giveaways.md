# Giveaway lists, saved details and entry rules

Use `/giveaway list` to find active IDs, then `/giveaway inspect id:<saved-id>`
in the giveaway's channel. `/help command:giveaway` lists the published
subcommands and their arguments. Administration requires Manage Server, the
configured guild and the existing automations feature. Inspection does not add
cross-channel visibility: an unknown ID, another guild's ID and a giveaway in
another channel produce the same private miss.

## Private active list

`/giveaway list` shows the existing same-guild staff inventory, in saved deadline
and ID order. Each complete row carries its stable ID, an inert prize preview,
saved destination, and `open` with a localized saved deadline or `paused (no
countdown)`. Paused rows do not show a ticking timestamp. This does not grant
cross-channel inspection rights or perform Discord visibility or message reads.

Only recorded, valid guild/channel/message IDs become message links. A saved
channel without a recorded message says `message unconfirmed`; an unavailable
destination says so and has no link. Links are historical saved references, not
proof the post still exists or that delivery succeeded. Check the destination
before recreating an unconfirmed giveaway.

All ten active IDs fit one private reply of at most 2,000 UTF-16 units. Prize
previews use at most 32 display characters, shortened further when needed;
`…` explicitly marks shortening and `?` means the saved prize has no displayable
text. Escaping and mention neutralization count toward the final budget. IDs,
links and deadlines are never cut and rows are never silently dropped. Empty
inventory says `No open giveaways.`; a failed read says unavailable, not empty.
The list makes no entry, lifecycle, audit, budget, claim or scheduling changes.

## Strict management options

Every `/giveaway` subcommand is checked as a whole before the bot reads or
changes anything. A payload that is ambiguous is refused with one private,
mention-safe reply that names the option to repair; it never picks a winner
among duplicates and never fills a default after bad input.

- Exactly one subcommand per invocation: `create`, `list`, `inspect`, `cancel`,
  `end`, `pause`, `resume`, `extend` or `reroll`.
- Each option may appear once, with its registered type. Unknown options are
  refused, including on `list`, which takes none.
- `id` is required for every subcommand except `create` and `list`: 1–64
  letters, digits, `_` or `-`.
- `create` needs `prize` (1–200 characters), a duration and `winners`
  (1–100). `required-role`, `min-account-age-days` and `min-server-age-days`
  (0–3650) are optional. A `channel` value is still accepted for older
  callers but never moves the post: giveaways open in the invoking channel.
- The duration (1–43200 minutes) is `duration` on `create` and
  `duration-minutes` on `extend`. Each command also accepts the other name for
  compatibility, but only one of the two, once. Supplying both is refused.
- `reroll` takes an optional `user`, at most once.

The refusal never repeats what was submitted. A refusal does not take an entry
budget, an audit row or any lifecycle step, and member entry buttons are
unaffected.

## Private saved-state inspection

Inspection shows the stable ID, bounded prize, saved lifecycle, localized
scheduled deadline, current saved entry count, configured winner count and
static requirements. A saved attached message is linked only in the invoking
channel. `pending` is unconfirmed entry-message delivery, never a jump link.

These are saved records, not live Discord reads. `drawn` means a draw was saved;
it does not confirm a public announcement. Only a saved `announced_at` receipt
is labeled announcement-confirmed, and even that is historical: it does not
prove the message still exists. A failed count/read reports unavailable rather
than inventing an empty giveaway or a successful send. Check the channel before
recreating a giveaway whose entry delivery is unconfirmed.

The response is ephemeral, mentions are suppressed, and content fits Discord's
2,000 UTF-16-unit limit. It does not expose a pre-draw nonce, claim token, entry
roster, winner roster or individual member timestamps. Inspection never enters,
uses entry budgets, draws, claims, refreshes a public post, writes an applied
audit or schedules work. Existing create, entry and lifecycle operations keep
their behavior.

## Rules before joining

Entry posts and private detail use one static `Requirements:` summary:

- `Everyone` when there are no saved role or age requirements.
- Inert role IDs, all required, and minimum account/server ages otherwise.
- An explicit unavailable summary for damaged saved requirements; never a guess
  that the giveaway is unrestricted. Unusually long role lists show a bounded
  prefix plus the number omitted and a request to ask staff for the rest.

These are rules, not a per-member eligibility check, an admission guarantee or
a winning promise. No member, role-name or profile fetch is performed to format
them. Existing entry/draw validation still decides admission and winners.
Saved prizes are rendered as bounded inert text so they cannot inject extra
requirement or status lines. Existing posts gain the summary when their normal
refresh runs; inspection itself never edits them.

## Verification

The protected `giveaway_options_acceptance` integration target covers every
malformed management shape above against the real routed runtime with a
connection-acquisition spy. The protected `giveaway_inspect_acceptance`
integration target covers saved
states, counts, same-channel privacy, malformed options, unavailable reads,
shared static summaries, hostile Unicode and no-effect controls. Run against
an authorized disposable database as described in [Contributing](../CONTRIBUTING.md#database-tests):

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test giveaway_inspect_acceptance -- --test-threads=1 --nocapture
```

This documentation and merged code do not establish deployment or command
publication. Existing activation and deployment gates remain unchanged.
