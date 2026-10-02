# Container scan and release SBOMs

`check.yml` calls the same read-only `sbom.yml` used by releases. (The
reusable SBOM workflow was renamed from `supply-chain.yml` because main's
TOG-11810 fold uses that filename for the native pr-lint + gitleaks gate,
which has no `workflow_call` trigger.) It
builds the Dockerfile on `[self-hosted, two-selfhosted]`, without registry push or deployment,
then inventories the exact image and all packages in the workspace Cargo.lock
(including workspace, optional and development dependencies). The Rust BOM is
not a claim that every locked package is linked into the runtime executable.
The image BOM inventories OS/image packages; Rust binaries are not reliably
recoverable by image analysis, so the separate lockfile scan is intentional.

Both gates use SHA-pinned Trivy Action 0.35.0 with Trivy v0.69.3. They fail on
HIGH/CRITICAL in OS and library dependencies, **including unfixed findings**.
A fresh vulnerability DB is downloaded by Trivy on every run. No shared cache
is written or read: the BuildKit GitHub-Actions cache is disabled and every
Trivy step sets `cache: false`, so a poisoned cross-run DB or layer cache
cannot make the gate pass. Scanner errors also fail; no `continue-on-error` or blanket
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

### Exact-tuple preflight (no acceptance, no suppression)

`scripts/vulnerability-preflight.py` binds the raw pinned-Trivy v0.69.3 image
report to the source SHA, immutable image ID, Debian 13.7/amd64 metadata (the
measured release of the digest-pinned distroless runtime) and complete, lossless
filesystem evidence (below). It checks each HIGH/CRITICAL finding against both
the scan inventory and the image's `status.d` package inventory, including PURL
package/version, architecture, distro and epoch. Duplicate qualifiers/rows,
missing/null PURLs, inventory mismatches and failed/partial evidence fail closed.
Trivy's normalized `Version` and `SrcVersion` are not full Debian binary/source
versions; the check uses installed versions and package IDs.

No tuple is accepted. The Bookworm conditional decision in
[CISO revision 3](/TOG/issues/TOG-11261#document-vulnerability-disposition)
(17 tuples expiring 2026-10-08) retired with the Bookworm runtime and does not
carry over to Debian 13 tuples. Every row is `not-accepted` with
`suppressed: false`, and `suppressed_count` is always 0. A new HIGH/CRITICAL
tuple keeps the gate red until a base-digest update fixes it or the CISO
re-dispositions it on that thread; any exception still follows the rules above.

CI retains `vulnerability-preflight.json` and its checksum, including after
failed image gates. Successful preflight means only that observations were
bound and classified; **it is not a vulnerability PASS**. Both Trivy gates,
their exit codes and the empty ignore file are unchanged.

## Exact-image applicability evidence

After the vulnerability gates, including when either fails, CI runs
`scripts/runtime-image-evidence.py` against the immutable image ID recorded with
the BOMs. The `supply-chain` artifact retains `runtime-image-evidence.json` and
its separate SHA-256 checksum. These diagnostic files are not release assets and
do not affect ignore selectors or waive findings.

Nothing executes inside the image: the distroless runtime has no shell, package
manager or `getcap`. The script creates one named, never-started container
(`--network none --no-healthcheck`), streams `docker export` of its merged
filesystem (120-second client timeout) and removes only that container
(30-second timeout), even after a failure. Only the daemon's exact
missing-container response for that name confirms absence after a failed
create; other cleanup errors or timeouts fail the job. Docker inspection does
not record image environment values.

From the exported tar stream, schema version 2 records:

- installed packages from `/var/lib/dpkg/status.d/<package>`: exactly one deb822
  paragraph per regular UTF-8 file, with `Package`, `Version` and `Architecture`,
  a file name equal to `Package` (or `Package:Arch`) and a `Status` that is absent
  or exactly `install ok installed`. A classic `/var/lib/dpkg/status`, duplicate,
  multi-paragraph, malformed, non-installed or empty inventories fail closed;
- SUID/SGID files, file capabilities (`security.capability` pax xattrs) and
  shells, package managers or privilege tools present on standard paths, with
  links resolved inside the image;
- the runtime binary's size, mode, owner, SHA-256, ELF interpreter and
  `DT_NEEDED` libraries, each resolved through image symlinks to a file and to
  its owning packages via `status.d/*.md5sums`. An unresolved library fails closed.

The report is written before export and again at the end, and is checksummed
even on failure. Failed export, cleanup or parsing leaves `complete: false` with
an explicit `collection_error` and a nonzero step. `complete: true` means the
filesystem was fully read, not that the vulnerability gates passed. Observations
of the scanned image do not prove production kernel, privilege or namespace
controls, or exploit reachability.

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
Image tags include the Actions run ID and attempt. No shared cache exists:
the image build uses no `cache-from`/`cache-to`, and Trivy caching is off
(`cache: false`), so per-run state lives only under `runner.temp`.
Cleanup removes only that run's image tag.
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

The builder is `rust:1.94-trixie` and the runtime is
`gcr.io/distroless/cc-debian13:nonroot`, both pinned by multi-platform index
SHA-256. Both are Debian 13, so the binary links against the glibc it runs on.
Dependabot's weekly `docker` updates follow both references. The distroless
`nonroot` tag is unversioned: if no digest bump arrives after a Debian point
release, refresh the pin by hand. There is no builder argument override, so a
build arg cannot select an unpinned base.

On 2026-10-02 the runtime moved off Bookworm under the CEO's TOG-11974 scope
decision. Step A, `debian:trixie-slim` (13.7), still measured 48 HIGH rows over
11 CVEs with no CISO disposition. Step B, the distroless image, measured 0
HIGH/CRITICAL with Trivy v0.69.3. CI re-measures every head; that note is not a
standing PASS.

The runtime contains 14 Debian packages (base-files, ca-certificates,
gcc-14-base, libc6, libgcc-s1, libgomp1, libssl3t64, libstdc++6, libzstd1,
media-types, netbase, tzdata, tzdata-legacy and zlib1g). It has no shell, apt,
dpkg or OpenSSL command. Trust data is the base's own `ca-certificates` bundle
at `/etc/ssl/certs/ca-certificates.crt`; nothing is installed or copied in
except the release binary. The bot runs as uid/gid 65532 (`nonroot`) from
`/home/nonroot/two-bot`, and the exec-form `HEALTHCHECK` calls the binary's own
`--healthcheck`, so no shell is needed.

The container smoke job reads the binary and trust bundle with `docker cp` from
a never-started container and requires PEM data readable by any account. It
requires non-zero real, effective, saved and filesystem uids for PID 1 (via
`docker top`), and its only exec is the binary's `--healthcheck`. It checks only
that the non-root runtime can read the certificate data; it does not perform an
external TLS handshake. Incident triage has no in-image shell. Use `docker cp` or
`docker export` from the runtime image, or a distroless `debug-nonroot` variant
locally only; never deploy a debug variant. Operator-configured upload hooks
that need shell utilities are not supported in this image.

The Bookworm-only mount-package purge and the Trivy selector diagnostics
retired with the Bookworm runtime.

Offline regressions (no Cargo compile, Docker daemon or database access):

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-supply-chain.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-runtime-image-evidence.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test-vulnerability-preflight.py
PYTHONDONTWRITEBYTECODE=1 python3 scripts/test_container_smoke.py
python3 scripts/test-docker-deps.py
python3 scripts/test-release-retry.py
```
