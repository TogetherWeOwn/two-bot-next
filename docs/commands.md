# Command reference

The full [generated command reference](generated/commands.md) lists the compiled
built-in commands, options, bounds and default Discord permissions with all
publication gates enabled. Its rows are build outputs, not tracked snapshots.
A fresh checkout intentionally has no generated reference.

## Read the reference

For a local checkout, generate it explicitly before compiling workspace tests
or all-targets Clippy:

```sh
python3 scripts/generate_command_reference.py
cargo test -p two-bot-core --test reference_docs --locked
```

On the persistent controller, use bounded build admission:

```sh
python3 scripts/generate_command_reference.py --controller
python3 scripts/cargo_cache.py run -- test -p two-bot-core --test reference_docs
```

The generator emits `command_reference.v1` JSON with the output path, or a
nonzero error with `code`, `retryable` and `next_action`. It reuses the canonical
Rust renderer in `crates/core/tests/reference_docs.rs`. It validates the tracked
configuration reference first and never rewrites it. The ordinary reference
checks are read-only: missing, stale or invalid generated content fails until
explicit regeneration. Do not run generation inside a comparison check.

Without a checkout, open the [check workflow](https://github.com/TogetherWeOwn/two-bot-next/actions/workflows/check.yml),
choose the run for the exact commit, and download its **command-reference**
artifact. The artifact contains the rendered `commands.md`; match the run's SHA
rather than treating a different run as current.

## What this reference does not prove

Registry metadata is not evidence that a handler is enabled, wired or exercised
in a deployment. Read the [parity ledger](parity.md) and
[publication guide](command-publish.md) for those boundaries. Live guild fences,
feature gates and handler authorization still apply. Database-backed custom
commands and optional text triggers are dynamic, not part of this built-in list.

Use `/help` in Discord for this server's last confirmed publication and
`/help command:rank` for one published command. Help is private and does not run
the selected command or guarantee that the caller is authorized to use it.

The [staging E2E matrix](staging-e2e-command-matrix.md) and saved publication JSON
fixtures remain independent authored contracts. Neither a generated reference nor
those offline expectations claim a live staging observation.
