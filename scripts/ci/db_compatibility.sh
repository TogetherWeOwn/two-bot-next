#!/usr/bin/env bash
# Runs only in the check.yml container job with its unpublished Postgres service.
set -euo pipefail
export LC_ALL=C
[[ ${GITHUB_ACTIONS:-} == true && ${GITHUB_EVENT_NAME:-} == pull_request ]]
[[ $PR_HEAD_SHA =~ ^[0-9a-f]{40}$ && $PR_BASE_SHA =~ ^[0-9a-f]{40}$ ]]
head_tree=$PWD
[[ $(git rev-parse HEAD) == "$PR_HEAD_SHA" ]]
prior_sha=$(git merge-base --all "$PR_BASE_SHA" "$PR_HEAD_SHA")
[[ $prior_sha =~ ^[0-9a-f]{40}$ ]]
scratch=$(mktemp -d "${RUNNER_TEMP:?}/db-compat.XXXXXXXX")
prior_tree=$scratch/prior
git worktree add --detach "$prior_tree" "$prior_sha"
printf 'PR head %s, event base %s, merge base %s\n' "$PR_HEAD_SHA" "$PR_BASE_SHA" "$prior_sha"
[[ $(git -C "$prior_tree" rev-parse HEAD) == "$prior_sha" ]]

build() {
  local tree=$1 label=$2 example=crates/cutover/examples/db_compatibility.rs
  (
    cd "$tree"
    [[ ! -e $example && ! -L $example ]]
    mkdir -p "$(dirname "$example")"
    cp "$head_tree/scripts/ci/fixtures/db_compatibility.rs" "$example"
    export CARGO_TARGET_DIR="$scratch/build-$label"
    cargo build --locked -p two-bot-cutover --example db_compatibility
    if [[ $label == prior ]]; then cargo build --locked -p two-bot --bin two-bot; fi
    git diff --exit-code HEAD
  )
}
build "$prior_tree" prior
build "$head_tree" head
prior="$scratch/build-prior/debug"
head="$scratch/build-head/debug"

source_db=two_bot_test_compat_source
target_db=two_bot_test_compat_target
url() { printf 'postgres://%s:@agent-testdb:5432/%s' "$1" "$2"; }
# Strip ambient credentials and connection overrides. The service is job-local.
sql() {
  local login=$1 database=$2
  shift 2
  env -i PATH="$PATH" PGCONNECT_TIMEOUT=5 PGPASSFILE=/dev/null PGSSLMODE=disable \
    psql "$(url "$login" "$database")" -XqnwAt -v ON_ERROR_STOP=1 "$@"
}
admin() { sql agent_test "$@"; }
with_db() {
  local setting=$1 login=$2 database=$3
  shift 3
  env -i PATH="$PATH" GITHUB_ACTIONS=true TWO_DATABASE_TLS=local-only \
    "$setting=$(url "$login" "$database")" "$@"
}

admin postgres <<'SQL'
CREATE ROLE two_bot_migrator NOLOGIN;
CREATE ROLE two_bot_runtime NOLOGIN;
CREATE ROLE two_bot_migrator_ro NOLOGIN;
CREATE ROLE two_web_reader NOLOGIN;
CREATE ROLE compat_migrate LOGIN IN ROLE two_bot_migrator;
ALTER ROLE compat_migrate SET role TO two_bot_migrator;
CREATE ROLE compat_runtime LOGIN IN ROLE two_bot_runtime;
ALTER ROLE compat_runtime SET role TO two_bot_runtime;
CREATE ROLE compat_audit LOGIN IN ROLE two_bot_migrator_ro;
ALTER ROLE compat_audit SET role TO two_bot_migrator_ro;
CREATE ROLE compat_web LOGIN IN ROLE two_web_reader;
ALTER ROLE compat_web SET role TO two_web_reader;
CREATE DATABASE two_bot_test_compat_source;
GRANT CONNECT, CREATE ON DATABASE two_bot_test_compat_source TO two_bot_migrator;
SQL
admin "$source_db" -c 'ALTER SCHEMA public OWNER TO two_bot_migrator'
with_db TWO_DATABASE_URL compat_migrate "$source_db" "$prior/examples/db_compatibility" source
"$prior/two-bot" db roles plan | admin "$source_db"
with_db TWO_DATABASE_URL compat_audit "$source_db" "$prior/two-bot" db roles verify

# Clone before seeding so the target starts with an installed prior schema,
# real ledgers and grants, and no destination moderation history.
admin postgres -c "CREATE DATABASE $target_db TEMPLATE $source_db"
admin postgres -c "GRANT CONNECT, CREATE ON DATABASE $target_db TO two_bot_migrator"
with_db TWO_DATABASE_URL compat_migrate "$target_db" "$head/examples/db_compatibility" target
env -i PATH="$PATH" GITHUB_ACTIONS=true "$head/examples/db_compatibility" plan | admin "$target_db"
[[ $(sql compat_audit "$target_db" -c 'SELECT current_user') == two_bot_migrator_ro ]]
[[ $(sql compat_web "$target_db" -c 'SELECT current_user') == two_web_reader ]]

sql compat_runtime "$source_db" <<'SQL'
INSERT INTO events (event_type, guild_id, occurred_at, source, idempotency_key)
VALUES ('member_join', '100000000000000001', '2026-01-01', 'compat', 'compat-event');
INSERT INTO join_risk_flags
    (event_id, guild_id, member_id, account_created_at, joined_at, source, score, reasons_json,
     bulk_join_window, flagged, created_at)
VALUES ('compat-raid', '100000000000000001', '100000000000000002', '2025-01-01T00:00:00Z',
        '2026-01-01T00:00:00Z', 'compat', 1, '[]', false, true, '2026-01-01T00:00:00Z');
INSERT INTO scheduled_messages
    (id, guild_id, channel_id, body, next_run_at, created_by, created_at, updated_by, updated_at)
VALUES ('compat-schedule', '100000000000000001', '100000000000000003', 'synthetic',
        '2026-01-01T00:00:00.000Z', 'compat', '2026-01-01T00:00:00.000Z',
        'compat', '2026-01-01T00:00:00.000Z');
SQL
mkdir "$scratch/backups"
with_db TWO_DATABASE_URL compat_migrate "$source_db" \
  env TWO_BACKUP_DIR="$scratch/backups" "$prior/two-bot" backup
archives=("$scratch"/backups/two-funnel-*.ndjson.gz)
[[ ${#archives[@]} == 1 && -s ${archives[0]} ]]

prior_matrix="$prior_tree/sql/database_role_matrix.sql"
head_matrix="$head_tree/sql/database_role_matrix.sql"
head_matrix_additions() {
  {
    echo 'BEGIN READ ONLY; SET LOCAL search_path = pg_catalog, pg_temp; WITH prior AS ('
    cat "$prior_matrix"
    echo '), candidate AS ('
    cat "$1"
    cat <<'SQL'
), added AS (SELECT * FROM candidate EXCEPT SELECT * FROM prior)
SELECT CASE WHEN NOT EXISTS (SELECT FROM prior)
    OR EXISTS (SELECT * FROM prior EXCEPT SELECT * FROM candidate)
    OR EXISTS (SELECT FROM added a JOIN prior p USING (schema_name, name))
    OR EXISTS (SELECT FROM added GROUP BY schema_name, name HAVING count(*) > 1)
    OR EXISTS (SELECT FROM added WHERE schema_name IS NULL OR name IS NULL OR kind IS NULL
        OR schema_name NOT IN ('public', 'web_v1')
        OR kind NOT IN ('table', 'admission', 'ledger', 'migrator', 'retained_aggregate', 'view', 'sequence', 'function')
        OR CASE WHEN kind = 'function' THEN to_regprocedure(format('%I.%s', schema_name, name)) IS NOT NULL
                ELSE to_regclass(format('%I.%I', schema_name, name)) IS NOT NULL END)
  THEN 'REFUSE'
  WHEN count(*) = 0 THEN 'UNCHANGED'
  ELSE 'UNION ALL SELECT * FROM (VALUES ' ||
       string_agg(format('(%L,%L,%L)', schema_name, name, kind), ', ' ORDER BY schema_name, name) || ') added'
  END FROM added;
ROLLBACK;
SQL
  } | sql compat_audit "$source_db"
}

set_additions() {
  additions=$(head_matrix_additions "$1")
  if [[ $additions == UNCHANGED ]]; then return; fi
  [[ $additions == 'UNION ALL SELECT * FROM (VALUES '* ]] || {
    echo 'Role matrix must preserve prior rows and add only new objects' >&2
    return 1
  }
  # The verifier follows a table's owned sequences implicitly. An added table
  # must not bring a pre-existing sequence into its new privilege expectations.
  for database in "$source_db" "$target_db"; do
    sql compat_audit "$database" -c "SELECT json_build_array(n.nspname, s.relname, d.refobjid::regclass::text, d.refobjsubid)
      FROM pg_class s JOIN pg_namespace n ON n.oid = s.relnamespace
      LEFT JOIN pg_depend d ON d.objid = s.oid AND d.classid = 'pg_class'::regclass
        AND d.refclassid = 'pg_class'::regclass AND d.deptype IN ('a', 'i')
      WHERE s.relkind = 'S' AND n.nspname NOT LIKE 'pg_%'" | sort > "$scratch/$database.sequences"
  done
  [[ -z $(comm -23 "$scratch/$source_db.sequences" "$scratch/$target_db.sequences") ]] || {
    echo 'An existing sequence disappeared or changed ownership dependency' >&2
    return 1
  }
}
set_additions "$head_matrix"

verify_prior() {
  local rc=0 findings
  with_db TWO_DATABASE_URL compat_audit "$target_db" "$prior/two-bot" db roles verify >"$scratch/strict.log" 2>&1 || rc=$?
  cat "$scratch/strict.log"
  if [[ $additions == UNCHANGED ]]; then
    [[ $rc == 0 ]] && grep -Fxq 'DATABASE ROLES VERIFIED (groups only; login bindings require operator audit)' "$scratch/strict.log"
    return
  fi
  # Keep every prior catalog check. Only extend its expected objects; a
  # connection/inspection error never counts as expected additive drift.
  if [[ $rc == 0 ]]; then
    grep -Fxq 'DATABASE ROLES VERIFIED (groups only; login bindings require operator audit)' "$scratch/strict.log" || return 1
  else
    [[ $rc == 1 ]] && grep -Eq '^DATABASE ROLE DRIFT: [1-9][0-9]* finding\(s\)$' "$scratch/strict.log" || return 1
  fi
  findings=$({
    echo 'BEGIN READ ONLY; SET LOCAL search_path = pg_catalog, pg_temp;'
    ADDITIONS="$additions" awk -v matrix="$prior_matrix" '
      $0 == "-- @matrix" {
        while ((getline row < matrix) > 0) print row
        print ENVIRON["ADDITIONS"]
        found++
        next
      }
      { print }
      END { if (found != 1) exit 1 }
    ' "$prior_tree/sql/verify_database_roles.sql" || return 1
    echo 'ROLLBACK;'
  } | sql compat_audit "$target_db") || return 1
  [[ -z $findings ]] || { printf '%s\n' "$findings"; return 1; }
}

round_trip() {
  local label=$1
  verify_prior
  with_db TWO_RESTORE_URL compat_migrate "$target_db" \
    "$prior/two-bot" restore "${archives[0]}" --force | tee "$scratch/restore-$label"
  [[ $(tail -n 1 "$scratch/restore-$label") == 'RESTORE VERIFIED' ]]
  [[ $(sql compat_runtime "$target_db" -c "SELECT count(*) FROM events WHERE idempotency_key = 'compat-event'") == 1 ]]
  [[ $(sql compat_web "$target_db" -c 'SELECT count(*) FROM web_v1.contract_meta') == 1 ]]
  with_db TWO_DATABASE_URL compat_runtime "$target_db" "$prior/examples/db_compatibility" smoke |
    tee "$scratch/smoke-$label"
  [[ $(tail -n 1 "$scratch/smoke-$label") == 'PRIOR DB SMOKE VERIFIED' ]]
}

round_trip exact-head
sql compat_migrate "$target_db" <<'SQL'
ALTER TABLE scheduled_messages ADD COLUMN compat_optional_note text;
CREATE TABLE public.compat_additive (id integer PRIMARY KEY);
GRANT SELECT, INSERT, UPDATE, DELETE ON public.compat_additive TO two_bot_runtime;
GRANT SELECT ON public.compat_additive TO two_bot_migrator_ro;
SQL
{
  echo 'SELECT * FROM ('
  cat "$head_matrix"
  echo ") candidate UNION ALL SELECT 'public', 'compat_additive', 'table'"
} > "$scratch/additive.sql"
set_additions "$scratch/additive.sql"
round_trip additive-schema

sql compat_migrate "$target_db" -c 'REVOKE UPDATE ON scheduled_messages FROM two_bot_runtime'
if verify_prior >"$scratch/revoked.log" 2>&1; then
  echo 'The prior verifier accepted a missing runtime grant' >&2
  exit 1
fi
cat "$scratch/revoked.log"
grep -Fx 'drift: missing table privilege: two_bot_runtime/public.scheduled_messages/UPDATE' "$scratch/revoked.log"
{
  printf '### Database rollback compatibility\n\n'
  printf 'Candidate %s upgraded from merge base %s (event base %s).\n\n' "$PR_HEAD_SHA" "$prior_sha" "$PR_BASE_SHA"
  printf 'Prior verifier, restore and DB smoke passed on the exact schema and with an added nullable column and explicitly granted table.\n'
  printf 'Revoking runtime UPDATE on scheduled_messages produced the expected prior verifier finding.\n'
} >> "${GITHUB_STEP_SUMMARY:?}"
