# Channel-access resolution acceptance

`crates/discord/tests/channel_access.rs` pins the existing public API of
`crates/discord/src/channel_access.rs` (port of legacy
`src/discord/channelAccess.ts`, consumed by `crates/bot/src/preflight.rs`):
`guild_permissions`, `resolve_channel_access` and `ChannelAccess`.
Twilight model types only, in-memory; no network, no token.

```sh
python3 scripts/cargo_cache.py run -- test -p two-bot-discord --test channel_access
```

## Acceptance matrix

| Case | Evidence asserted |
| --- | --- |
| ADMINISTRATOR short-circuit | Full access (`view/send/embed/manage_messages/admin`) despite deny-all overwrites at @everyone, role and member stages; also when admin comes from @everyone alone |
| Guild role union | `guild_permissions` covers @everyone + held roles only; unheld role bits never surface in resolution |
| Overwrite order | @everyone deny removes access; held-role allow rescues it; role allow beats sibling role deny; member overwrite decides after role overwrites (deny drops send/embed, allow rescues deny) |
| Capability gating | Send needs view, embed needs send, manage needs view (embed-only / manage-only denies; send-without-view unusable) |
| No cross-channel/member leakage | Unheld-role and foreign-member overwrites do not change the result; sibling channel overwrite slices resolve independently |

Unit coverage already in `crates/discord/src/channel_access.rs` (`legacy_channel_access_table`,
`legacy_staff_alert_permission_values`,
`game_categories_do_not_grant_access_to_unsynced_children`) stays as the
legacy parity table; this integration file pins the four acceptance properties
above against the public API only.
