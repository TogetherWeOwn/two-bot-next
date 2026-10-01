# Container scan and release SBOMs

`check.yml` calls the same read-only `supply-chain.yml` used by releases. It
builds the Dockerfile on `[self-hosted, two-selfhosted]`, without registry push or deployment,
then inventories the exact image and all packages in the workspace Cargo.lock
(including workspace, optional and development dependencies). The Rust BOM is
not a claim that every locked package is linked into the runtime executable.
The image BOM inventories OS/image packages; Rust binaries are not reliably
recoverable by image analysis, so the separate lockfile scan is intentional.

Both gates use SHA-pinned Trivy Action 0.35.0 with Trivy v0.69.3. They fail on
HIGH/CRITICAL in OS and library dependencies, **including unfixed findings**.
A fresh vulnerability DB is downloaded/updated by Trivy (the daily cache does
not skip updates). Scanner errors also fail; no `continue-on-error` or blanket
`ignore-unfixed` is allowed. SBOMs and JSON findings remain in the `supply-chain`
Actions artifact for 14 days, including on a vulnerability failure.

## Exceptions

`.trivyignore.yaml` starts empty. Fix dependencies or upgrade base-image digests
before proposing an exception. Each narrowly scoped YAML vulnerability entry
must have `id`, an effective scope selector supported by the pinned scanner,
`statement` with reason and review evidence, and `expired_at` (at most 30 days).
A package/version/architecture in a statement is not selector enforcement;
OS findings without a package path cannot be scoped by inventing a file path.
Verify the approved scope and expiry with positive and negative selector tests
before activating an exception. A Code Reviewer must approve the exact SHA;
security exceptions also need CISO agreement. Do not ignore a whole severity,
a whole ecosystem or every unfixed finding. An expired exception restores the
gate. New findings must be triaged on the same PR, never bypassed to get green.

## Exact-image applicability evidence

After the vulnerability gates, including when either fails, CI runs
`scripts/runtime-image-evidence.py` against the immutable image ID recorded with
the BOMs. The `supply-chain` artifact additionally retains
`runtime-image-evidence.json` and its separate SHA-256 checksum. These diagnostic
files are not release assets and do not affect ignore selectors or waive findings.

The report records image architecture, declared runtime user and entrypoint,
installed package versions/architectures and file lists, selected utility/module
presence, Perl build width, ELF/linkage probes, SUID/SGID files, capability-tool
output and mount configuration. Each fixed probe runs in its own named container
with a read-only filesystem, no network, no mounts or passed secrets, all
capabilities dropped and no-new-privileges. Inspection uses container root for
file visibility, not to exercise privileged operations or the bot entrypoint.
Each probe has a 20-second client timeout, CPU/memory/PID limits and removal of
only its own container even on timeout. The enclosing job retains its 40-minute
bound. Docker inspection does not record image environment values.

Nonzero exits, unavailable tools and timeouts are retained explicitly; they are
not absence proof. For example, missing `getcap` leaves capabilities unresolved.
Module absence and linkage observations still need package/CVE-specific analysis
and source/caller evidence. CI kernel, mounts and inspection privileges do not
prove production kernel, namespace restrictions or exploit reachability. Keep
all unresolved gates red; pursue supported Bookworm fixes or compatible removal
of unnecessary packages before requesting a new disposition on the existing
security-decision thread.

## Release and dry-run

On release-please publication, `release.yml` checks out the published `vX.Y.Z`
tag, rebuilds/scans it and attaches `rust-workspace.cdx.json`,
`container-image.cdx.json`, `SHA256SUMS`, `source-sha.txt` and `image-id.txt` to
that GitHub Release **only if both gates and inventory validation succeed**.
The source SHA and local image ID bind the files to this build. They do not
claim this image is the deployment's digest (there is no image push here).
An asset failure does not erase the published release; inspect the failed run
and use the explicit repair input below. Publication isn't atomic with
release-please; require the assets before treating the release as fully delivered.

```sh
# Opening/updating a PR automatically runs check.yml's PR SBOM dry-run.
# The 40-minute job has contents: read, no deploy secrets and no publishing path.
# No release workflow dispatch or pull_request_target is required.
# Inspect the PR run's supply-chain artifact: two nonempty CycloneDX BOMs,
# SHA256SUMS, source SHA/image ID, and both vulnerability reports.
gh run download <run-id> -n supply-chain -D <run-owned-scratch-directory>
# Repair assets on an existing stable release; rebuilds its tag, not current main.
gh workflow run release.yml --ref main -f release_tag=vX.Y.Z
```

The PR dry-run inventories the exact PR head SHA, validates lockfile coverage
and Debian image components, and records checksums/provenance before scanning.
Image tags include the Actions run ID and attempt; Trivy's cache is run-local.
Cleanup removes only that run's image tag, never shared Docker caches.
The optional release-workflow dry-run input also has no publication tag, even
if a release tag input is supplied; PR verification does not use that dispatch.
The repair path skips
release-please and replaces assets only after the full scan passes. Release
upload uses the short-lived Actions GITHUB_TOKEN with only `contents: write`;
all builder/scanner jobs have only `contents: read` and no repository credential
persisted in their checkout.

## Pins and maintenance

Docker base tags remain Rust 1.94 Bookworm and Debian Bookworm slim, with
multi-platform manifest SHA-256 pins resolved from Docker Hub. Dependabot's
weekly `docker` updates maintain those digests alongside Cargo/Actions updates.
The builder argument override was removed so a build arg cannot silently
select an unpinned builder. Apt still fetches signed current Bookworm packages;
digest-pinned bases are not a promise of byte-for-byte repeatable apt results.

Distroless is a separate follow-up evaluation: assess TLS roots, non-root user,
healthcheck executable, debug/incident workflow and binary compatibility before
changing image family. This change deliberately keeps Bookworm.

Offline regressions (no Cargo compile, Docker daemon or database access):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-supply-chain.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-runtime-image-evidence.py
python3 scripts/test-docker-deps.py
python3 scripts/test-release-retry.py
```
