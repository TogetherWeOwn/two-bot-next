# Operator voice configuration apply (`voice-config-apply`)

`/import` needs a Discord admin session in the guild. A cutover that maps the
interim voice bot's generators onto two-bot-next creators should not depend
on one, so the Operator can apply the same V11 document from the cutover
host.

```sh
DISCORD_TOKEN=... TWO_DATABASE_URL=... \
  voice-config-apply --guild <guild-id> --file voice-config.json   # dry run: prints the diff and its hash
DISCORD_TOKEN=... TWO_DATABASE_URL=... \
  voice-config-apply --guild <guild-id> --file voice-config.json \
    --apply --expect-hash <hash-from-the-dry-run>                   # writes
```

The decisions are the `/import` ones (`crates/cutover/src/voice_config_apply.rs`):

- The file goes through the strict V11 codec (`decode_configuration`) after
  the 256 KiB size cap; malformed JSON reports line and column only.
- The trusted inventory is read from Discord (guild roles, channels, members),
  never from the file. A partial read refuses.
- Entries on channels the guild does not have are reported and skipped;
  cross-guild documents, wrong-kind channels and bad templates refuse.
- Without `--apply` nothing is written. `--apply` requires `--expect-hash`
  with the hash printed by the reviewed dry run, the way `/import` Confirm is
  bound to its preview: the hash covers the stored configuration and the
  candidate, so a configuration change since the dry run (or another file)
  refuses with exit 3. `PgVoiceConfigStore::apply` then writes every section
  in one transaction under its own compare-and-swap. After a write the tool
  reads the configuration back and fails if it differs from the candidate.
- `--guild` must be a canonical snowflake (no sign, no leading zero), and the
  live guild refuses unless `--allow-live-guild` is passed.
- Configuration never touches live rooms (`voice_rooms`), and the voice
  runtime only reads it while `TWO_VOICE=1`.

Exit codes: 0 dry run, no changes or applied; 1 refused or failed; 2 usage or
live-guild fence; 3 hash mismatch or compare-and-swap conflict.
