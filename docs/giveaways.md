# Giveaway saved details and entry rules

Use `/giveaway list` to find active IDs, then `/giveaway inspect id:<saved-id>`
in the giveaway's channel. `/help command:giveaway` lists the published
subcommands and their arguments. Administration requires Manage Server, the
configured guild and the existing automations feature. Inspection does not add
cross-channel visibility: an unknown ID, another guild's ID and a giveaway in
another channel produce the same private miss.

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

The protected `giveaway_inspect_acceptance` integration target covers saved
states, counts, same-channel privacy, malformed options, unavailable reads,
shared static summaries, hostile Unicode and no-effect controls. Run against
an authorized disposable database as described in [Contributing](../CONTRIBUTING.md#database-tests):

```sh
TWO_TEST_DATABASE_URL=postgres://agent_test:@agent-testdb:5432/two_bot_test_local python3 scripts/cargo_cache.py run -- test -p two-bot-discord --features db --test giveaway_inspect_acceptance -- --test-threads=1 --nocapture
```

This documentation and merged code do not establish deployment or command
publication. Existing activation and deployment gates remain unchanged.
