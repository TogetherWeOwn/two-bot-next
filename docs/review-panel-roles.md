# Review panel roles

Status: **NOT QUALIFIED**. These files declare the intended roles; offline tests
check their declarations, not whether a running adapter enforces them. A role
file is not a sandbox. Do not use these roles as qualified panel seats until the
runtime checks below pass for the exact reviewed commit.

## Roles and authority

| Role | Full model id | Code-review focus |
| --- | --- | --- |
| `panel-opus` | `claude-opus-5-5` | Design, scope and simplicity of the code |
| `cross-reviewer` | `gpt-6.1-sol` | Correctness, concurrency and security |

Both request `xhigh` effort and only `Read`, `Grep` and `Glob`. Neither exposes
Bash: a prompt asking a general shell to behave read-only is not enforcement.
Mutation and delegation tools are explicitly denied as a second declaration.
Tests, Git inspection and evidence preparation belong to the lead, not a seat.
The allowlist does not itself restrict filesystem reads to one directory or
protect secrets, hidden cases or the evaluator from a process sharing its uid.

Threat model: material under review can contain hostile instructions, and model
aliases, inherited environment pins or a stale loaded role can change authority
or routing. Runtime tool filtering, native workspace controls and served-model
receipts must enforce the intended boundary. Prompts and SHA comparisons only
detect some mistakes; they do not supply isolation. These roles grant no
credentials, network calls, installs, production actions or governance authority.

## Qualification checklist

Record dated evidence outside the repository, without credentials or raw logs.
For each role record the role-file commit, role-load receipt, effective tools,
requested and served model ids, request correlation id, workspace id and pinned
SHA. Record unavailable evidence as `not run`, never as a passing result.

1. Start a fresh seat through the named repository role in the installed
   adapter. Do not use a model alias, an Agent model override, a generic seat or
   a governance agent. Verify the loaded role matches the committed file;
   editing a role does not prove an existing session reloaded it.
2. Verify the **served model** from a correlated routing receipt, not the role's
   frontmatter, request alone or the model's claim. Confirm the requested effort
   is supported; report an unsupported setting instead of silently weakening it.
3. Verify the effective tool set and **no-delegation** boundary. In an authorized
   disposable fixture, establish that out-of-scope mutation, shell and nested
   delegation requests are refused by the runtime, not merely declined in text.
   Never probe this against production files or credentials.
4. Use a **native SHA-pinned** workspace. Confirm the runtime binding and actual
   commit before dispatch; do not manually clone or provision detached worktrees.
   Confirm required evaluator/held-out access exclusions independently: the
   same uid, a detached checkout or an evaluator hash is not access isolation.
5. For image-bearing reviews, qualify actual image input on each route with a
   harmless known-answer image. A text-only reply is not image evidence.
6. Confirm existing subscription eligibility and quota before dispatch. A zero
   monthly budget is not a zero-spend enforcement boundary. No paid model CI or
   new credential is authorized by these roles.

Until those checks pass, stop panel dispatch and retain `NOT QUALIFIED`. A
successful static test or a completed review cannot repair missing controls.
Changes to these instructions require maintainer review; normal CI, review and
release gates still apply.

## Review protocol

Fix known defects, open findings and failing checks before requesting a round.
The lead supplies a frozen `standing.md` (request, focus, rubric, references,
protected calls and output contract) and `r<N>.md` (round, full commit SHA,
native workspace, evidence and the disposition of prior findings). Do not put a
pass threshold, target, peer scores or peer output in either brief. Keep the
roster fixed; each seat is fresh and independent, without the author's context.

Each panel reply contains:

1. `REVIEWED: <sha>` for the supplied pinned commit.
2. `SCORE: x/10`, or category scores if the standing brief supplies a rubric.
   Use `no score` if evidence is insufficient; never invent a score.
3. `VERDICT: ship | ship-with-changes | rethink`. An invalid run instead reports
   `not run: <reason>` with no approval verdict.
4. Verified findings, most severe first (`blocker | should | nit`), each with
   file:line, failure scenario, smallest fix and concrete evidence. Use measured
   values, reproduction steps or supplied evidence item ids, not speculation.
5. Whether the round made anything worse, and what must not change.
6. A closing JSON block with `sha`, `scores`, `verdict` and `findings`. Use null
   scores/verdict for an invalid run and include its `not_run_reason`.

The lead discards wrong-SHA replies, verifies findings before accepting them and
records a reason for each declined finding. Unverifiable taste calls need two
independent seats to agree. Never average scores into approval. Later code
rounds cover changed code and open findings, with every seat confirming the
exact final commit. Default cap: three rounds; timeout, missing seats and budget
exhaustion are not approval. Cut scope after two rounds without progress and
route unresolved disputes through the normal review process.

Edit the PR description's `## Review panel` table, rather than posting score
comments. Show each seat's served id, all rounds, `not run` / `no score` cells,
accepted/declined/deferred findings and final dissent. With no PR, use the task's
review ledger instead. Panel scores never replace required CI or maintainer
review.

## Offline checks

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts/ci -p 'test_panel_roles.py' -v
python3 scripts/check_docs_index.py --json
```

The existing CI discovery of `scripts/ci/test_*.py` runs these configuration pins
without invoking a model. They are regression checks for declarations and the
qualification warning, not runtime tool-denial or served-model evidence.
