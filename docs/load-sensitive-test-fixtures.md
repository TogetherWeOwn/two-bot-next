# Load-sensitive test fixtures

## HTTP deadlines

Announcement transport tests use paused Tokio time, a blocking clock hold during
real socket I/O, and request/header barriers. Only deadline tests advance time,
and they check the pending outcome immediately before the existing deadline.
The production deadline, response classification and retry policy are unchanged.
An independent standard-thread watchdog bounds setup, notification barriers and
final completion without advancing or unpausing Tokio. Its cancellation drops
the pending future and releases the blocking clock hold. Negative regressions
exercise both a never-ready notification and a hidden virtual sleep, verify zero
virtual advancement, and acknowledge the released hold.

## Shared audit pacing

The executor's lane timestamps and waits use the same Tokio monotonic clock.
Unpaused production keeps the existing 110 ms shared and 350 ms kick intervals;
checked posts retain their reservation through late authorization and HTTP send.
A refused authorization commits neither a lane stamp nor a request.

Three former receipt-gap integration cases now run in the adapter's always-built
unit suite, against the real adapter/transport and the same loopback REST double.
This keeps the committed-admission probe `cfg(test)` without a release API or a
feature that could silently omit the tests. The probe reports the exact stamp
synchronously under the real lane lock, only after authorization succeeds.

Paused-clock cases check pending state at 109 ms, release at 111 ms, assert each
committed shared gap is at least 110 ms, and verify request counts, methods and
body order. A barrier deliberately delays the first mock receipt by 100 ms;
correct 111 ms admission spacing then yields an 11 ms receipt gap. That controlled
counterexample distinguishes observer lag from admission under-pacing; it does
not reproduce or establish the historical failing schedule. Further cases prove
late refusal sends nothing and consumes no slot, and held authorization cannot
be overtaken by another post or a shared-lane read. All use the independent
wall-clock watchdog and non-spinning clock hold.

## Single-core harness concurrency

Before applying native-binary CPU affinity, the workflow preserves explicit
`RUST_TEST_THREADS` or queries unpinned Rust `available_parallelism()` (which
accounts for CPU quotas, not just the affinity mask). It exports that count,
records its source and original affinity, and leaves explicit `--test-threads`
overrides intact. Compilation is unpinned and test bodies are not serialized.
Old single-core runs that did not preserve libtest's original parallelism do not
establish this concurrency requirement. A corrected SHA starts at **0/3**.

## Disposable database lifecycle

Fixture connections retain `statement_timeout=5000ms`. A process-local gate queues
CREATE/DROP statements before their SQL deadline starts; migration and test bodies
remain concurrent. Teardown closes all fixture-owned pools and propagates DROP
errors. The lifecycle regression checks catalog absence, independent-pool closure,
eight-way create/close, migration failure and cancellation-resilient cleanup.
The close wave prepares all eight fixtures and independent peers before a
nine-participant barrier releases all eight teardown callers together; SQL DDL
alone remains queued by the existing gate.

These measures do not remove PostgreSQL's internal waits. PostgreSQL 18.6 DROP
unconditionally requests a forced immediate checkpoint and waits for it after
forgetting target sync requests. That request waits for a new checkpoint; an
already-running checkpoint can delay it. DROP also waits for backend exit,
cluster-wide storage-manager barrier acknowledgement, WAL synchronization and
filesystem removal. Closing a client pool is not a server-process exit receipt.

The loaded check reproduced a five-second DROP cancellation alongside a checkpoint
with over ten seconds of sync time. This does **not** identify the cancelled
statement's wait phase or prove that checkpoint caused it.

## CI-only storage isolation

Only the main `check` job's disposable `agent-testdb` service places its PostgreSQL
volume on a **1 GiB tmpfs**, with a **2 GiB total service memory limit** and equal
memory/swap limits to prevent swap spill. PostgreSQL 18's default data directory is
`/var/lib/postgresql/18/docker`; the image volume is `/var/lib/postgresql`, not the
pre-18 `/var/lib/postgresql/data`. The health check verifies the actual `PGDATA`
filesystem is tmpfs. A read-only preflight rejects changed data paths or disabled
`fsync`, `full_page_writes` and `synchronous_commit`, using a five-second statement
deadline and an explicit passwordless test-service connection without ambient
libpq configuration or password files.

This isolates resident data/WAL checkpoint traffic from shared-host block storage.
It changes neither PostgreSQL durability settings nor fixture assertions,
activation gates, parallelism, SQL deadlines or image/binary budgets. No controller
cache, production/staging database, other CI service or host configuration changes.

This is a correction candidate, not proven root-cause attribution. The unchanged
full suite must validate capacity and memory headroom: data, WAL and temporary
files share the tmpfs cap, and tmpfs plus backend/shared-buffer memory share the
service cap. ENOSPC, OOM, unhealthy service or teardown failure is a failed run,
not a reason to retry unchanged code or raise limits silently. Tmpfs is disposable
storage, not evidence of persistence across service recreation or host failure.

Three consecutive exact-head single-core check passes, all required checks,
independent review, non-author merge and current-main green remain the acceptance
gates. Offline workflow checks alone do not establish loaded-runner acceptance.

## References

- [PostgreSQL 18.6 DROP implementation](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/commands/dbcommands.c)
- [PostgreSQL 18.6 checkpoint requests](https://github.com/postgres/postgres/blob/REL_18_6/src/backend/postmaster/checkpointer.c)
- [Official 18.6 image source](https://github.com/docker-library/postgres/blob/e00e1bd34ec5c8a8e7ad89b273b3d42efaf6d5bc/18/trixie/Dockerfile)
- [Docker tmpfs semantics](https://docs.docker.com/engine/storage/tmpfs/)

Offline regression command:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_ci_postgres_storage.py' -v
```
