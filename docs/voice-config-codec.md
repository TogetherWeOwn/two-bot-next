# Voice configuration codec (V11 core)

`two_bot_core::voice_config` implements the standalone, versioned JSON contract
from `docs/voice-rooms.md`. It is original code written from that specification.
It has no Discord or database dependency and performs no writes. It reuses two
pure core modules for validation: the V7b template lint and the V7a alias table.

## API and version 1

- `export_configuration(&config, &inventory) -> Result<Vec<u8>, VoiceConfigError>`
  validates and returns pretty-printed UTF-8 JSON.
- `import_configuration(bytes, &inventory) -> Result<VoiceConfiguration, VoiceConfigError>`
  parses and validates the entire document before returning any candidate.
- `decode_configuration(bytes) -> Result<VoiceConfiguration, VoiceConfigError>` is
  the strict decode alone (size cap, objects only, no duplicate keys, no trailing
  data), for callers that must report and skip unknown channels before they
  validate. Never feed uploaded bytes to plain `serde_json::from_slice`: the
  derived top-level decoder also accepts the positional-array form.
- `validate_configuration(&config, &inventory)` also validates in-memory values.
- `VOICE_CONFIG_VERSION` is `1`; there are no implicit migrations or defaults
  between versions. Unknown fields and enum values fail rather than being lost.
- Import requires JSON objects at the document and every nested DTO boundary,
  including permission sources. Positional arrays and duplicate object keys fail.
- Every exported field must be present, including nullable `logging`, both
  creator/channel `status_template` fields and viewer/command role IDs. Explicit
  `null` disables or clears those values; omission is malformed, never a default.
- IDs are canonical, nonzero decimal `u64` **strings**, never JSON numbers.
  This preserves snowflakes above JavaScript's safe integer range.

The document contains `version`, `guild_id`, `creators`, `templates`, `aliases`,
`lists`, `logging` and `settings`. The exported configuration includes:

| Section | Version-1 data |
| --- | --- |
| Creators | Channel ID, name/status templates, default limit, privacy/text toggles, above/below placement, first number, category grouping, permission source |
| Templates | Permanent voice/stage channel IDs with name and optional status templates |
| Aliases | Ordered game/alias pairs, with unique game keys |
| Lists | Ordered named random lists, with unique names and nonblank choices |
| Logging | `null` for off, otherwise text channel, detail (`errors`, `lifecycle`, `verbose`), member and role mention IDs |
| Settings | Creation toggle, unique names, no-game label, single-game/inactive-member behaviour, timezone, text-channel name/viewer role, command role and per-command role lists |

Permission source is tagged data: `{"kind":"creator"}`, `{"kind":"category"}`
or `{"kind":"channel","channel_id":"105"}`. It does **not** evaluate or grant
permissions. Role restrictions and logging mention targets are also data only.
No runtime rooms, ownership, membership timestamps, random seeds, individual
nicknames or bitrate preferences are exported.

`crates/core/tests/voice_config.rs::fixture` is the complete synthetic example.
A typed round-trip preserves all values, Unicode, ordering, optional values and
64-bit IDs; repeated exports of the same candidate are byte-identical. Original
JSON whitespace/escape spelling is not part of the lossless contract.

## Validation boundary

The integration layer supplies a **trusted, current** `GuildInventory` for the
dispatch guild. Never build this inventory from uploaded data. It maps channel,
role and member IDs to their guild; channels also carry their kind. The document
guild must equal the dispatch guild. All explicit channel/role/member fields must
resolve in that guild, including permission sources, viewer/command roles and
logging mentions. Missing IDs and known foreign IDs are separate errors.

Creator channels must be voice channels; standalone template targets must be
voice/stage channels; logging targets must be text channels. A channel cannot
occur twice or be both a creator and standalone template target. Alias/list/
command keys and role/member lists reject duplicates, preventing silent
last-entry-wins behaviour.

Limits: default user limit `0..=99`, positive first number, nonblank timezone and
labels. Literal text-channel names and no-game labels have a 100-Unicode-scalar
ceiling. Template **source** is not limited to 100 characters: V5 truncates
rendered output, not source expressions.

Further checks, each naming only the field (never the uploaded text):

- **Templates.** Every creator and standalone `name_template` and `status_template`
  passes the V7b lint with the V5 passthrough policy. Lint *errors* refuse: an
  unclosed or unopened construct, an `@@token@@` outside the known list, source
  over 4096 bytes or nested deeper than 64 levels. Warnings (an empty render, a
  condition that never matches) stay valid, and an empty template is retained.
- **Aliases.** Entries load into the V7a `AliasTable`: at most 100 entries, keys and
  targets at most 100 characters, no control or bidi-override characters,
  case-folded unique keys and no alias chains.
- **Lists.** At most 100 lists of at most 100 choices; names and choices are
  nonblank, at most 100 characters and free of control characters.
- **Command roles.** `settings.command_roles[].command` must be an exact entry of
  `voice_access::VOICE_COMMANDS`.
Import accepts JSON only (YAML payloads fail as malformed) and refuses documents
over `MAX_IMPORT_BYTES` (256 KiB) on the document before parsing, so an
oversized upload can never partially apply.
Empty templates are retained because V5 defines fallback for empty rendered
names. Malformed JSON/type errors expose only line/column, not uploaded text.

This core is strict: an unknown channel fails the candidate without silently
skipping or partially applying it. V11's user-facing "reported and skipped"
behaviour belongs to the command preview: the parent must explicitly report and
remove unknown-channel entries, revalidate the proposed result, then ask for
confirmation. Cross-guild entries must never be treated as relocatable channels.

## Residual integration (not proven by these unit tests)

The parent V11 slice still owns:

1. Manage Server checks at **both** export/import dispatch and confirmation,
   attachment byte/count limits, ephemeral download and replies. An import that
   adds a creator row also needs Manage Channels (as `/create` does), checked at
   preview and again at confirmation.
2. Mapping the version-1 DTO to V1/V7 persistent settings, and compiling templates
   (including embedded `ROLE:id`/`MEMBER:id` conditions and named-list references)
   with those slices' own implementation. This codec validates explicit DTO
   references, not IDs or conditions embedded in template text. The runtime must
   also verify IANA timezone names with its own provider before applying a
   candidate.
3. Diff preview, unknown-channel reporting/skipping, confirmation and re-reading
   current inventory/settings before a single transaction. No room is created,
   deleted, renamed, re-owned or moved by this codec.
4. Permission-source capability checks (Manage Roles/category fallback), logging
   mention policies and actual Discord effects. Import must not bypass those
   existing runtime gates.
5. Bitrate preferences and guild-tier caps, which are not version-1 configuration
   fields, plus any live guild acceptance proof.

Hermetic verification:

```sh
cargo test -p two-bot-core --test voice_config --locked
cargo clippy -p two-bot-core --all-targets --locked -- -D warnings
cargo fmt --all -- --check
```

No environment credentials, network, Postgres or Redis are used by these tests.
Passing them proves the codec contract, **not** runtime parity or activation.
