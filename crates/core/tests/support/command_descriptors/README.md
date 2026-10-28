# Command-owned descriptor acceptance

Protected acceptance for moving each slash command's metadata into one
command-owned descriptor. Implementers consume these files and never edit them.
Any disagreement with the interface below goes to the review card first.

## Contract

A command descriptor owns, for one command: its handler, wire schema, feature
gate, permission (including the refusal shown on denial), `/help` position,
E2E cases and parity metadata. Publication, dispatch, permission lookup,
`/help`, the E2E matrix and parity rows are derived from one deterministic
registry. Adding or removing a representative command needs only its own
module/descriptor and focused tests: no central match arm, no parallel name
list, no committed generated snapshot. The registry fails closed on a
duplicate name, an invalid name, a missing handler, any incomplete field and
any ambiguous permission; resembling a registered name never makes a command
reachable.

## Files

| File | Compiled today? | Phase | Purpose |
|---|---|---|---|
| `../../command_registry_baseline_acceptance.rs` | yes, discovered | baseline | Public-behavior characterization of the current router, publish set, `/help`, E2E matrix and permission table against the frozen oracle, in all 32 gate sets. Passes on `main`; must pass unchanged after the migration. |
| `frozen_baseline.rs` | via the baseline | oracle | Hand-written literal table of the 43 non-voice built-ins (handler, gate, permission bits, denial, policy hook, parity row, help layout, non-slash parity rows). Never derived from a registry. |
| `frozen_e2e.rs` | via the baseline | oracle | Exact success shape, permission label and full ordered denial list for every frozen command, shared with the phase-2 consumer contract. |
| `frozen_help.rs` | via the baseline | oracle | Full help-reply oracle: frozen groups plus legacy voice anchor/groups, RSVP guidance, Custom order, unique-name count and footer. No production renderer calls. |
| `contract_foundation.rs` | **no** (future API) | 1 | Synthetic-command contract for the descriptor, registry and router seam. |
| `contract_consumers.rs` | **no** (future API) | 2 | Shipping commands reproduce the frozen rows through the registry; central files hold no per-command list. |

The two `contract_*` files sit under `tests/support/` so Cargo does not
discover them; they reference an interface that does not exist yet, so they are
not placeholder always-failing tests and have not been compiled. To enable
phase 1, add this wrapper (the only file the builder writes):

```rust
// crates/core/tests/command_descriptor_contract_foundation.rs
#[path = "support/command_descriptors/contract_foundation.rs"]
mod contract_foundation;
```

and the analogous `command_descriptor_contract_consumers.rs` for phase 2, only
in the PR that has moved the shipping commands. Wrapper files must contain
nothing else.

## Pinned interface (everything the contract files use)

New leaf module `two_bot_core::command_descriptor` (the module name is part of
the contract; where it is declared is the builder's choice):

```text
CommandDescriptor            Clone + Debug, immutable once built
  CommandDescriptor::builder(name: &'static str) -> DescriptorBuilder
DescriptorBuilder            by-value chain, every method returns Self
  definition(CommandDefinition)           wire schema; reused type
  handler(HandlerId)
  gate(E2eGate)                           reused: Always|Scorecard|Automations|Announcements|Moderation
  open()                                  everyone may use it at runtime
  requires(bits: u64, denial: RouterRefusal)
  also_allowed_by(bits: u64)              extra bits that satisfy it (the /game-mix Administrator rule)
  policy_hook(PolicyHook)                 optional
  help(HelpPlacement)
  help_note(line: &'static str)           optional, repeatable, descriptor-owned guidance
  e2e_case(success_shape: &'static str, denials: &'static [&'static str])   repeatable
  parity_row(u8)
  publish_order(u32)
  build() -> Result<CommandDescriptor, DescriptorError>
HelpPlacement { group: &'static str, group_order: u32, needs: Option<&'static str>, position: u32 }
DescriptorError              Clone + Debug + Display + std::error::Error, #[non_exhaustive]
  DuplicateName { command }  InvalidName { command }  MissingHandler { command }
  Incomplete { command, field: &'static str }   // field in: definition, permission, gate, help, e2e, parity, publish_order
  AmbiguousPermission { command }  ConflictingHelpGroup { group }
DescriptorRegistry           Clone + Debug, immutable value
  builtin() -> Self                                   all shipping descriptors
  from_descriptors(impl IntoIterator<Item = CommandDescriptor>) -> Result<Self, DescriptorError>
  with(&self, CommandDescriptor) -> Result<Self, DescriptorError>   original untouched; a refused add changes nothing
  without(&self, name: &str) -> Self                  unknown name is a no-op
  names() -> Vec<&'static str>                        canonical order
  permission(&self, name: &str) -> Option<CommandPermission>        built-in slash rows only
  permissions() -> Vec<CommandPermission>             all parity rows in canonical order
  e2e_matrix() -> Vec<E2eMatrixRow>                   one row per declared case
  help_text(&self, published: &[CommandDefinition]) -> String
  unregistered_handlers(&self, router: &InteractionRouter) -> Vec<&'static str>   command names whose handler is not registered
InteractionRouter::with_registry(gates: RouterGates, registry: &DescriptorRegistry) -> InteractionRouter
  InteractionRouter::new(gates) == with_registry(gates, &DescriptorRegistry::builtin())
HandlerId::Descriptor(&'static str)         one additive variant: the identity of a leaf-owned handler

Phase 2 consumer seam (exported from two_bot_core, like help_text):
  help_text_with_registry(published: &[CommandDefinition], registry: &DescriptorRegistry) -> String
  help_text(published) == help_text_with_registry(published, &DescriptorRegistry::builtin())
```

Semantics the tests pin beyond the signatures:

- Canonical order is `(publish_order, name)`; input order never matters.
- Names are 1-32 characters of `a-z`, `0-9`, `-`, `_`; the wire definition's name must equal the descriptor's.
- Permission is unambiguous only when exactly one of `open()` / `requires(..)` is declared, `requires` bits are nonzero, the runtime bits equal the definition's published `default_member_permissions` (`None` means 0), and `also_allowed_by` appears only with `requires`.
- A parity row may be shared by different names (LFG and LFG-edit are row 26); a name may not be shared.
- Help groups are keyed by title; members must agree on `group_order` and `needs`; members render by `position`.
- `E2eMatrixRow.permission_name` is the Discord permission constant name for a single known bit (`ManageGuild`), `Everyone` for 0; `handler` is the `Debug` spelling of the `HandlerId`.
- Gate off keeps the name routed and refused (`Refuse`), never `Unknown`; guild fence first, gate second, permission third.
- Registered metadata is not an executing handler: `unregistered_handlers` reports it.
- Shipping registration comparisons cover the 43 frozen commands. Their exact five member-handler gaps are independent literals, not expectations derived from the production exception helper. A later command's exception does not enlarge that frozen set or fail its cardinality check. Every reported registry gap still names a registered descriptor and appears only once.

## Help scope and unchanged member-visible output

`DescriptorRegistry::help_text` renders **only descriptors in that registry whose
names are in `published`**. It ignores all other names, including another
registry's synthetic probe, voice-only names and custom rows. Its header counts
those unique descriptor names; it uses the legacy framing/footer. It has no
Custom fallback. A descriptor's `help_note` lines render after its group's name
line, in member-position order and then builder-call order, only while that
descriptor is published. The `rsvp-attendance` descriptor owns the two existing
guidance lines verbatim; no migrated-command literal is needed in `help.rs`.

The phase-2 **public consumer** `help_text_with_registry` uses the supplied
registry's group/needs/position/note metadata, then preserves the legacy voice
and Custom surfaces. `help_text` delegates with `builtin()`; it must not discard
voice or custom definitions to make the two renderers equal:

- All three voice groups, their order and permission labels stay unchanged.
  They require the published `textchannels` anchor; without it, voice-looking
  names remain under Custom. The shared `kick` keeps its frozen Kick group.
- Any published name not grouped by the supplied registry or anchored voice
  groups appears under Custom in input order. A removed descriptor's still-
  published definition therefore falls back to Custom here, but is ignored by
  registry-only help. Removing metadata never fabricates a new route.
- The public header counts **all unique published names**, including voice and
  Custom, and the introductory sentence/footer stay byte-identical. Repeated
  Custom definitions preserve the legacy repeated-name output, not the count.
- Registry-only help equals public help for built-in, voice-off publish sets.
  With voice or Custom present, equality applies to `help_text_with_registry`
  and `help_text`, not to registry-only help. The baseline freezes complete
  output bytes in all 32 gate sets plus anchor/duplicate/empty-input controls.

The optional note builder method and phase-2 consumer seam preserve existing
output and expose the supplied-registry control; they do not move voice into
the 43-command E2E/parity registry or require a new registration dependency.

## Open interface questions for the builder

1. `HandlerId` is a closed central enum. Without an open variant a new leaf command cannot exist without editing `router.rs`. The contract assumes one additive `HandlerId::Descriptor(&'static str)`. Propose another open identity on the card if you prefer it; the tests then adapt in one place.
2. How `builtin()` aggregates leaf modules (module list, build-time collection) is unconstrained. The contract only needs `builtin()` to be deterministic and to equal the frozen rows once everything has moved.
3. Voice `kick` stays shadowed by moderation `kick` and the voice sink keeps owning its other names while `TWO_VOICE=1`. That is baseline behavior (see the baseline file); the registry must not turn it into a duplicate-name error or a new route.

## Out of scope and fences

- Schema bytes, the registry-golden oracle (`registry_golden.rs`, `support/registry_parity.rs`), the E2E coverage test, the permission-table tests and the bot-crate registration conformance keep running unmodified; this suite adds to them.
- Generated command/parity output belongs to the generated-reference pipeline, not here. No contract file reads or writes `docs/commands.md`, the reference renderer or any workflow.
- No database, network or Discord. Pure values only.
