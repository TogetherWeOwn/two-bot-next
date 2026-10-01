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
Actions artifact for 14 days, including on a vulnerability failure. The existing
required `check` depends on this scan job and explicitly rejects failed, skipped
or cancelled results, so scan failure cannot leave that merge check green.

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

### Conditional-decision tuple preflight (no suppression)

`scripts/vulnerability-preflight.py` binds the raw pinned-Trivy v0.69.3 image
report to the source SHA, immutable image ID, Debian 12.15/amd64 metadata and a
successful, lossless installed-package probe. It checks each HIGH/CRITICAL
finding against both scan and dpkg inventory, including PURL package/version,
architecture, distro and epoch. Duplicate qualifiers/rows, missing/null PURLs,
inventory mismatches and failed/partial probes fail closed. Trivy's normalized
`Version` and `SrcVersion` are not full Debian binary/source versions; the check
uses installed versions and package IDs, not inferred downstream revisions.

The frozen [CISO revision 3 decision](/TOG/issues/TOG-11261#document-vulnerability-disposition)
lists 17 conditional tuples expiring at **2026-10-08T00:00:00Z**. Matching tuples
are reported as `conditions-and-selector-proof-required`, never approved or
suppressed. At or after expiry they are `expired`; any other tuple is
`not-conditionally-accepted`. Every row retains `suppressed: false` and the
report always has `suppressed_count: 0`. It does not distinguish previously
rejected tuples from newly discovered tuples: neither is accepted.

CI retains `vulnerability-preflight.json` and its checksum after diagnostics,
including failed image gates. Successful preflight means only that observations
were bound and classified; **it is not a vulnerability PASS**. Both existing
Trivy gates, their exit codes and the empty ignore file remain unchanged. This
preflight does not implement affected-code conditions, source/package
authentication, extra/injected implementation exclusion or scanner suppression.
Those checks, actual pinned-scanner positive/negative selector/expiry tests and
independent final-head approval remain required before any activation. Changes
to modules, interpreters, payloads or startup environment cannot be waived by a
matching tuple. Stock-image observations are not deployed privilege controls.

### Actual pinned-scanner selector diagnostics (no activation)

`scripts/probe-trivy-selectors.py` runs the installed **Trivy v0.69.3** binary's
`convert` command against isolated copies of the retained raw image report.
Upstream `convert` calls the same `result.Filter` implementation used during
scans. This tests native filtering, not a Python approximation, but **does not
rescan an image or refresh a vulnerability database**. It never edits the raw
reports or `.trivyignore.yaml` and never feeds filtered copies to either gate.
The four-minute CI step retains `trivy-selector-probes.json` and its checksum.

For each observed conditional tuple, the matrix measures exact selection,
different packages/versions/architectures, unknown CVEs, missing/null PURLs and
identifiers, epoch changes where applicable, source/image/distro binding changes
and expiry. Consistent alternative tuples update both inventories; malformed
fixtures intentionally do not. Receipts distinguish native exit/finding counts
from prerequisite preflight rejection or an unaccepted classification. Binary,
input, synthetic ignore and output hashes identify the experiment. Zero observed
candidates means zero selector coverage, not exception safety.

Each subprocess executes one private, hashed snapshot of the installed binary,
so a concurrent setup action cannot replace its executable during the matrix.
It has an explicit empty config and isolated HOME/cache, with only PATH/HOME
inherited and a 20-second timeout. Synthetic YAML expiry controls use
RFC3339 timestamps in the distant past/future; JSON-quoted date-only strings do
not decode into Trivy's timestamp field. These test dates do **not** renew the
real decision. The preflight's exclusive expiry boundary is independently
checked at the original **2026-10-08T00:00:00Z**; Trivy's wall clock is unchanged.

Native conversion shows why YAML alone is insufficient: missing/null target
PURLs can match a selected CVE, and source/image/distro provenance is not a YAML
PURL condition. Missing identifiers may also be repaired from scan inventory.
A null whole `PkgIdentifier` triggers a v0.69.3 JSON decoder panic; its nonzero
exit/no output is recorded separately, never as suppression or successful
conversion. All these malformed prerequisites are rejected by the preflight.
Unexpected conversion failures or failed controls fail the diagnostic step.
This does not implement the remaining affected-code, authenticated payload or
injection conditions, does not clear the rejected tuples and does not authorize
any exception. Independent exact-head review and successful real gates remain
mandatory.

To reproduce with an already verified raw artifact and verified pinned binary:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/probe-trivy-selectors.py <artifact-directory> --trivy <verified-trivy-binary>
```

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
Completed and timed-out output is decoded as UTF-8; invalid bytes are replaced and
marked per stream in `lossy_decoding` for both the probe and cleanup result. Such
output is incomplete evidence, not a valid filename or absence determination.
A valid UTF-8 replacement character alone does not set the loss marker.
Each probe retains its exact UUID `container_name` and original result separately
from cleanup, so a cleanup timeout or daemon transport failure still leaves an
ownership-scoped follow-up target in the partial report. Only the daemon's
exact missing-container response for that probe's UUID confirms absence after a
startup failure; other cleanup errors/timeouts stop further probes and fail the
job. The report is saved incrementally and checksummed even on cleanup failure,
with `complete: false` and an explicit collection error. `complete: true` means
all probes were attempted, not that their commands or vulnerability gates passed.
Module absence and linkage observations still need package/CVE-specific analysis
and source/caller evidence. CI kernel, mounts and inspection privileges do not
prove production kernel, namespace restrictions or exploit reachability. Keep
all unresolved gates red; pursue supported Bookworm fixes or compatible removal
of unnecessary packages before requesting a new disposition on the existing
security-decision thread.

The existing mount-configuration probe first records the eight util-linux binary
packages' exact versions, architectures and source package/versions, then resolves
and hashes `/usr/bin/mount`, `/usr/bin/umount`, `/usr/bin/nsenter` and the amd64
`libmount.so.1` target. Path resolution, package-query or hashing failures remain
nonzero observations; later help/configuration commands cannot hide them. This
keeps the same 16-probe and timeout bounds. These are hashes observed inside the
scanned image, not authenticated Debian package comparisons or automatic evidence
of absent vulnerable code. Compare them with independently authenticated exact
published payloads before relying on a source/build applicability decision. No
ignore selector or vulnerability gate is changed by recording these hashes.

## Release and dry-run

On release-please publication, `release.yml` resolves the explicit Git tag ref
`refs/tags/vX.Y.Z` through the read-only Git API, peels annotated tags, and
checks out the resulting commit SHA, never an ambiguous same-named branch. It
rebuilds/scans that commit and attaches `rust-workspace.cdx.json`,
`container-image.cdx.json`, `SHA256SUMS`, `source-sha.txt` and `image-id.txt` to
that GitHub Release **only if both gates and inventory validation succeed**.
Before upload, checksums and source SHA must match the selected commit, and the
release tag is resolved again: a moved tag fails rather than receiving stale BOMs.
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
The separate container smoke job also uses a run/attempt-owned tag and binds
both runtime probes and deliberate budget failures to the build action's immutable
image output. This prevents another job's tag from changing the tested image;
it does not change either size budget or establish why a prior size check failed.
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
select an unpinned builder. Apt fetches signed current Bookworm certificate
updates in the builder, not the runtime. Digest-pinned bases are not a promise
of byte-for-byte repeatable apt results.

Bookworm's [`ca-certificates` package](https://packages.debian.org/bookworm/ca-certificates)
depends on `openssl`. The runtime instead copies the updated certificate bundle,
hashed certificate links, their Mozilla certificate targets and licensing from
the pinned Bookworm builder. It does not copy OpenSSL executables/libraries or
change the runtime base's package metadata. This avoids introducing certificate
maintenance helpers into a rustls runtime; it does not assert that every package
already in the base is fixed or unnecessary. CI must verify the final inventory,
linkage and runtime smoke test before treating this as successful remediation.
The smoke gate checks that the non-root runtime can read PEM certificate data;
this is not an external TLS handshake or a proof of application input reachability.
Operator-configured upload hooks can invoke external wrappers, so source-only
absence of direct utility calls is not a blanket compatibility or CVE waiver.

Distroless is a separate follow-up evaluation: assess TLS roots, non-root user,
healthcheck executable, debug/incident workflow and binary compatibility before
changing image family. This change deliberately keeps Bookworm.

Offline regressions (no Cargo compile, Docker daemon or database access):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-supply-chain.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-runtime-image-evidence.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-vulnerability-preflight.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-trivy-selector-probes.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_container_smoke.py
python3 scripts/test-docker-deps.py
python3 scripts/test-release-retry.py
```
