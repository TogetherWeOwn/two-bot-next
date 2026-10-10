# Operator voice configuration apply (`voice-config-apply`)

`/import` needs a Discord admin session in the guild. A cutover that maps the
interim voice bot's generators onto two-bot-next creators should not depend
on one, so the Operator can apply the same V11 document from the cutover
host.

```sh
DISCORD_TOKEN=... TWO_DATABASE_URL=... \
  voice-config-apply --guild <guild-id> --file voice-config.json          # dry run: prints the diff
DISCORD_TOKEN=... TWO_DATABASE_URL=... \
  voice-config-apply --guild <guild-id> --file voice-config.json --apply  # writes
```

The decisions are the `/import` ones (`crates/cutover/src/voice_config_apply.rs`):

- The file goes through the strict V11 codec (`decode_configuration`) after
  the 256 KiB size cap; malformed JSON reports line and column only.
- The trusted inventory is read from Discord (guild roles, channels, members),
  never from the file. A partial read refuses.
- Entries on channels the guild does not have are reported and skipped;
  cross-guild documents, wrong-kind channels and bad templates refuse.
- Without `--apply` nothing is written. With it, `PgVoiceConfigStore::apply`
  writes every section in one transaction, with the printed snapshot as the
  compare-and-swap expectation: if the guild's configuration changed in
  between, nothing is written and the tool exits 3. After a write the tool
  reads the configuration back and fails if it differs from the candidate.
- The live guild refuses unless `--allow-live-guild` is passed.
- Configuration never touches live rooms (`voice_rooms`), and the voice
  runtime only reads it while `TWO_VOICE=1`.

Exit codes: 0 dry run, no changes or applied; 1 refused or failed; 2 usage or
live-guild fence; 3 compare-and-swap conflict.
