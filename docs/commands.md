# Command reference

After generation, `docs/generated/commands.md` contains the compiled
built-in commands, options, bounds and default Discord permissions with all
publication gates enabled. Its rows are build outputs, not tracked snapshots.
A fresh checkout intentionally has no generated reference.

## Read the reference

Open the [check workflow](https://github.com/TogetherWeOwn/two-bot-next/actions/workflows/check.yml),
choose a Rust-selected run, and download its **command-reference** artifact.
The artifact contains the rendered `commands.md` from the `check` job's checkout.
For a PR run, verify that the run's full head SHA matches the PR head for CI
association, then read the checkout step's logs for the tested source SHA.
The default PR checkout is GitHub's synthetic merge commit, not the PR-head tree;
its artifact can include base-branch command changes absent from that head.
Record both SHAs and the run/job links when citing it. For a push run, likewise
verify the checkout source SHA against the requested commit. The run head alone
is not artifact-source proof. This is the normal reference path, including for
readers with a local checkout; it requires no local generation.

Docs-only changes may deselect Rust and produce no command-reference artifact.
Use that exact head's `ci-ok`, selector results and selected jobs as CI evidence;
do not generate locally to compensate. Another Rust-selected run's reference
can be read as historical only: label it with its run/job links, PR head (if
applicable) and checkout source SHA, not as evidence for the current head.

CI runs `python3 scripts/generate_command_reference.py` before test-target
compilation or all-targets Clippy, then runs read-only reference tests. These are
Actions steps, not a local verification recipe. Full suites and CI gates stay
on GitHub Actions under the
[verification policy](../CONTRIBUTING.md#verification-on-github-actions).

Local execution is allowed only to reproduce one failing test or crate under
the [focused failure policy](../CONTRIBUTING.md#focused-failure-reproduction-only).
Any heavy focused build uses the guarded `boat` PATH wrapper, sizing/budget/security
controls, `--no-env`, a source-only export and verified cleanup. A permitted
non-heavy controller reproduction/fallback uses the
[bounded Cargo cache wrapper](build-cache.md); the generator's `--controller`
option enforces that admission, not permission to mirror CI. Never disable
controller admission, run a heavy raw-host build, or use a fallback to run a
full suite/CI mirror. If these limits cannot be met, reproduce on Actions or
route the actual prerequisite.

The generator emits `command_reference.v1` JSON with the output path, or a
nonzero error with `code`, `retryable` and `next_action`. It reuses the canonical
Rust renderer in `crates/core/tests/reference_docs.rs`. It validates the tracked
configuration reference first and never rewrites it. The ordinary reference
checks are read-only: missing, stale or invalid generated content fails until
explicit regeneration. Do not run generation inside a comparison check.

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
