# Load-sensitive test fixtures

## HTTP deadlines

Announcement transport tests use paused Tokio time, a blocking clock hold during
real socket I/O, and request/header barriers. Only deadline tests advance time,
and they check the pending outcome immediately before the existing deadline.
The production deadline, response classification and retry policy are unchanged.

## Disposable database lifecycle

Fixture connections retain `statement_timeout=5000ms`. A process-local gate queues
CREATE/DROP statements before their SQL deadline starts; migration and test bodies
remain concurrent. Teardown closes all fixture-owned pools and propagates DROP
errors. The lifecycle regression checks catalog absence, independent-pool closure,
eight-way create/close, migration failure and cancellation-resilient cleanup.

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
