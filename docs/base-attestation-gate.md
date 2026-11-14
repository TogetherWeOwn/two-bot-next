# Base-controlled attestation validation

Status: additive source preparation. Required-workflow binding and live
qualification are not established by this change. All existing required checks
remain required. Do not replace `pr-lint` with a check-name-only binding.

## Source and permissions

`.github/workflows/base-attestation.yml` runs metadata validation on
`pull_request_target`, using a pinned checkout action and only the immutable
base SHA from the event. It checks out `.github/scripts`, never the PR head or
merge ref. The new helper imports the established `pr_standards.py` beside it.
No PR-controlled script, action, dependency, executable or artifact is run.

The single unconditional job runs on a disposable GitHub-hosted runner, with a
five-minute timeout, `contents: read` and `pull-requests: read`. Credentials are
not persisted in Git configuration. Its API client sends GET requests only to
fixed `api.github.com` routes for this repository and refuses redirects. No
write token, environment secret, custom check publication, installation or
external service is needed.

The helper fences the webhook against current PR metadata before and after
validation: repository identities, number, open state, head/base SHAs and refs,
author, title, body, update time and changed-file count must match. Its run SHA
must be the event base SHA and the current `main` tip. A stale event or base
fails; push or reopen the PR to obtain a fresh qualification. The workflow has
no shared concurrency group: ordinary and ruleset-required executions must not
cancel or replace one another. These are conservative read fences, not a
transaction locking GitHub PR metadata until merge. GitHub's
[ruleset concurrency guidance](https://docs.github.com/en/enterprise-cloud@latest/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/troubleshooting-rules#concurrency)
requires avoiding cancellation of required workflows.

Fork heads are accepted as metadata, with the same immutable identity checks;
no access to the fork's contents is needed. A missing or changed head repository
identity fails rather than falling back to the base repository.

## Complete inventory and policy

The files endpoint is paged at 100 entries. The collected entry count must
exactly match `changed_files`, before rename expansion. Empty, duplicate,
malformed, incomplete and over-cap inventories fail closed. Both old and new
rename paths feed the risk floor, so moving a critical file does not remove its
classification. Paths must be representable without newline or control-character
ambiguity. Internal spaces are supported.

GitHub limits this REST endpoint to 3,000 files. Larger PRs are refused; split
them for this validator. The separate manual inventory route in the existing
supply-chain workflow is not a successful run of this trusted workflow and is
not a substitute for its eventual required-workflow qualification. The helper
does not silently accept a partial list or switch to executing head code.

The existing base policy's title/body checks, T0 floor and exemptions remain.
The new workflow, helper and regression file additionally request the existing
T0 attestation policy when they change. This extends the floor without modifying
the policy owned by other changes. Indirect stored release-note bodies are
currently refused: a complete, separately qualified metadata route would be
needed before supporting them. Ordinary inline PR bodies continue through the
base policy.

Attestation validation checks declared shape/evidence references, not whether a
claim is true or a named test actually passed. Exact-head Actions, independent
review and release approval remain separate gates. Diagnostics are constant
messages and SHA/count receipts; raw PR text and API response bodies are not
printed as workflow commands.

## Required-workflow rollout contract

A normal `pull_request_target` job result is not a custom PR-head status. Its
name, or its GitHub Actions App identity, alone does not establish enforcement.
The intended additive binding is GitHub's native organization ruleset
**Require workflows to pass before merging**, selecting the reviewed repository,
workflow path and trusted source ref. Preserve every existing required check.

Before requesting settings changes, Engineering and Security must accept the
exact source/ref and settings, and the settings owner must establish that the
repository's plan and permissions support this rule. Do not substitute a status
name or a new write credential if the capability is unavailable. Retain the
current gate and record the prerequisite instead.

GitHub documents `pull_request_target` support for required workflows. Ruleset
runs use default `opened`, `synchronize` and `reopened` activities and ignore
activity, path and branch filters. The ordinary target workflow also listens for
`edited` and `ready_for_review`, but an edit-only run must not be assumed to
refresh the ruleset qualification. Push or reopen after metadata corrections.
Merge queues are not enabled here; enabling one needs separately reviewed
`merge_group` validation, not an unconditional successful result.

Required live rollout evidence includes:

1. The accepted source ref/SHA, workflow path, repository identity and effective
   ruleset readback targeting `main`, with no existing required gate removed.
2. A valid PR passing and an invalid attestation refusing, on their exact heads,
   including a fork PR and a renamed critical path.
3. A changed/skipped merge-ref job and a same-name head context not satisfying
   the trusted workflow requirement. Offline fixtures prove only source behavior;
   they do not prove GitHub enforced this setting.
4. A changed head, metadata or base not inheriting a stale qualification; record
   the actual push/reopen behavior for existing open PRs.
5. Complete/truncated inventory outcomes and the explicit over-cap refusal.

Do not claim enforcement before that readback and qualification evidence exist.
The workflow first becomes available from trusted base source after its own
reviewed merge; a draft PR adding it cannot validate itself through a target
workflow that is not yet on the base. The synthetic regression suite instead
runs in the existing `check.yml` Python suite on the exact draft head.

## Verification and integration boundaries

`scripts/ci/test_base_attestation_gate.py` supplies synthetic metadata and REST
responses to the actual helper and established base policy. GitHub Actions is
CI of record. The existing workflow Python discovery runs the new tests; do not
run a local full suite or CI mirror.

The implementation is additive. It does not alter `supply-chain.yml`, its
transport, its inventory fixtures, or existing required contexts. The shared
workflow-policy suite has explicit per-job permission and runner expectations;
registering this job there is a narrow dependent integration after coordination
with that suite's current owners. Preserve the existing inventory and snapshot
assertions. Do not widen a general permission or runner allowlist to accommodate
this one job.

Sources:

- [Available rules: required workflows](https://docs.github.com/en/enterprise-cloud@latest/repositories/configuring-branches-and-merges-in-your-repository/managing-rulesets/available-rules-for-rulesets#require-workflows-to-pass-before-merging)
- [Events: pull_request_target](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#pull_request_target)
- [REST pull request files limit](https://docs.github.com/en/rest/pulls/pulls#list-pull-requests-files)

REA angle: reuse the shipped base policy and GitHub required-workflow mechanism,
not a service, watcher or custom status publisher.
