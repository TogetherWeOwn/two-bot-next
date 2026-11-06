//! Schema-aware internal audit migration regressions (0471 identity, 0472 order).
//!
//! Real-database support for `website_contract.rs`. The shipped chain runs in
//! isolated scratch schemas there; this module also applies the raw, unshipped
//! 0471/0472 files with hostile installer state. `raw_sql` sends each file as
//! one simple-query batch, which Postgres executes as one implicit
//! transaction, so a refusal must leave the catalog byte-for-byte unchanged.

use sqlx::{migrate::Migrator, AssertSqlSafe, PgPool};
use two_bot_testsupport::TestDatabase;

const M471: &str = include_str!("../../../cutover/migrations/0471_internal_audit_identity.sql");
const M472: &str = include_str!("../../../cutover/migrations/0472_internal_audit_order.sql");

pub const HELPERS: [&str; 3] = [
    "internal_audit_identity_immutable",
    "internal_audit_allocate_order",
    "internal_audit_order_guard",
];
const TABLES: [&str; 3] = [
    "internal_action_log",
    "internal_idempotency",
    "internal_audit_order",
];

const GUILD: &str = "123456789012345678";
const TARGET: &str = "234567890123456789";

/// Always-quoted identifier, safe for any schema name used here.
pub fn qi(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn hex64(c: char) -> String {
    c.to_string().repeat(64)
}

/// The shipped chain strictly below `version`.
pub fn chain_below(version: i64) -> Migrator {
    let full = sqlx::migrate!("../cutover/migrations");
    Migrator::with_migrations(
        full.iter()
            .filter(|m| m.version < version)
            .cloned()
            .collect(),
    )
}

const CATALOG_SQL: &str = "SELECT jsonb_build_object(
  'functions', (SELECT coalesce(jsonb_agg(jsonb_build_object(
      'oid', p.oid::bigint, 'name', p.proname,
      'args', pg_get_function_identity_arguments(p.oid),
      'owner', pg_get_userbyid(p.proowner), 'acl', p.proacl::text,
      'config', p.proconfig, 'secdef', p.prosecdef,
      'def', pg_get_functiondef(p.oid)) ORDER BY p.proname, p.oid), '[]'::jsonb)
    FROM pg_proc p
    WHERE p.pronamespace = $1::text::regnamespace AND p.proname = ANY($2::text[])),
  'triggers', (SELECT coalesce(jsonb_agg(jsonb_build_object(
      'oid', t.oid::bigint, 'table', c.relname, 'name', t.tgname,
      'fn', t.tgfoid::bigint, 'fn_schema', p.pronamespace::regnamespace::text,
      'enabled', t.tgenabled::text, 'def', pg_get_triggerdef(t.oid))
      ORDER BY c.relname, t.tgname), '[]'::jsonb)
    FROM pg_trigger t
    JOIN pg_class c ON c.oid = t.tgrelid
    JOIN pg_proc p ON p.oid = t.tgfoid
    WHERE c.relnamespace = $1::text::regnamespace AND NOT t.tgisinternal
      AND p.proname = ANY($2::text[])),
  'tables', (SELECT coalesce(jsonb_agg(jsonb_build_object(
      'oid', c.oid::bigint, 'name', c.relname,
      'owner', pg_get_userbyid(c.relowner), 'acl', c.relacl::text,
      'rls', c.relrowsecurity) ORDER BY c.relname), '[]'::jsonb)
    FROM pg_class c
    WHERE c.relnamespace = $1::text::regnamespace AND c.relname = ANY($3::text[])),
  'columns', (SELECT coalesce(jsonb_agg(jsonb_build_object(
      'table', c.relname, 'name', a.attname, 'type', a.atttypid::bigint,
      'not_null', a.attnotnull, 'dropped', a.attisdropped) ORDER BY c.relname, a.attnum), '[]'::jsonb)
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
    WHERE c.relnamespace = $1::text::regnamespace AND c.relname = ANY($3::text[]) AND a.attnum > 0),
  'constraints', (SELECT coalesce(jsonb_agg(jsonb_build_object(
      'table', c.relname, 'name', k.conname, 'validated', k.convalidated,
      'def', pg_get_constraintdef(k.oid)) ORDER BY c.relname, k.conname), '[]'::jsonb)
    FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid
    WHERE c.relnamespace = $1::text::regnamespace AND c.relname = ANY($3::text[]))
)::text";

async fn table_rows(pool: &PgPool, schema: &str, table: &str) -> String {
    let qualified = format!("{}.{}", qi(schema), table);
    let present: bool = sqlx::query_scalar("SELECT to_regclass($1::text) IS NOT NULL")
        .bind(&qualified)
        .fetch_one(pool)
        .await
        .expect("probe audit table");
    if !present {
        return "absent".to_owned();
    }
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)), '[]'::jsonb)::text FROM {qualified} t"
    )))
    .fetch_one(pool)
    .await
    .expect("snapshot audit rows")
}

/// Helper catalog, trigger OIDs and bindings, ACLs, owners, definitions,
/// constraints, allocator state and audit log for one schema.
pub async fn snapshot(pool: &PgPool, schema: &str) -> String {
    let catalog: String = sqlx::query_scalar(CATALOG_SQL)
        .bind(qi(schema))
        .bind(HELPERS.map(String::from).to_vec())
        .bind(TABLES.map(String::from).to_vec())
        .fetch_one(pool)
        .await
        .expect("snapshot audit catalog");
    format!(
        "{catalog}\nlog={}\nidempotency={}\norder={}",
        table_rows(pool, schema, "internal_action_log").await,
        table_rows(pool, schema, "internal_idempotency").await,
        table_rows(pool, schema, "internal_audit_order").await,
    )
}

pub async fn last_audit_id(pool: &PgPool, schema: &str) -> i64 {
    sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT last_audit_id FROM {}.internal_audit_order",
        qi(schema)
    )))
    .fetch_one(pool)
    .await
    .expect("read allocator state")
}

/// Insert one audit row (and its intent) into `schema`; returns the allocated
/// audit id. `tag` must differ per call within a schema.
pub async fn insert_audit(pool: &PgPool, schema: &str, tag: char) -> i64 {
    let s = qi(schema);
    let intent: i64 = sqlx::query_scalar(AssertSqlSafe(format!(
        "INSERT INTO {s}.internal_idempotency
            (caller_hash, key_hash, action, payload_hash, state, guild_id, target_id)
         VALUES ($1, $2, 'guild.add_member', $3, 'in_flight', '{GUILD}', '{TARGET}')
         RETURNING intent_id"
    )))
    .bind(hex64('a'))
    .bind(hex64(tag))
    .bind(hex64('c'))
    .fetch_one(pool)
    .await
    .expect("insert audit intent");
    sqlx::query_scalar(AssertSqlSafe(format!(
        "INSERT INTO {s}.internal_action_log
            (audit_id, created_at, intent_id, phase, caller_hash, action, guild_id,
             target_id, response_code, http_status, evidence_code, affected)
         VALUES (1, '2000-01-01Z', $1, 'terminal', $2, 'guild.add_member',
                 '{GUILD}', '{TARGET}', 'success', 200, 'executor', 1)
         RETURNING audit_id"
    )))
    .bind(intent)
    .bind(hex64('a'))
    .fetch_one(pool)
    .await
    .expect("insert audit row")
}

/// Exactly three non-overloaded helpers, owned by the installer, catalog-first
/// `search_path`, triggers bound to this schema's own helpers, and an allocator
/// body that names only this schema's state table.
pub async fn assert_schema_bound(pool: &PgPool, schema: &str) {
    let quoted: String = sqlx::query_scalar("SELECT quote_ident($1::text)")
        .bind(schema)
        .fetch_one(pool)
        .await
        .expect("quote_ident");
    let functions: Vec<(String, Option<Vec<String>>, String, bool)> = sqlx::query_as(
        "SELECT proname::text, proconfig, prosrc,
                pg_get_userbyid(proowner) = current_user
         FROM pg_proc
         WHERE pronamespace = $1::text::regnamespace AND proname = ANY($2::text[])
         ORDER BY proname",
    )
    .bind(qi(schema))
    .bind(HELPERS.map(String::from).to_vec())
    .fetch_all(pool)
    .await
    .expect("read helpers");
    let mut expected_names = HELPERS.map(String::from).to_vec();
    expected_names.sort();
    assert_eq!(
        functions.iter().map(|f| f.0.clone()).collect::<Vec<_>>(),
        expected_names,
        "{schema}: one non-overloaded helper per name"
    );
    for (name, config, source, owned) in &functions {
        assert_eq!(
            config.as_deref(),
            Some(&[format!("search_path=pg_catalog, {quoted}")][..]),
            "{schema}.{name}: catalog-first search_path"
        );
        assert!(owned, "{schema}.{name}: installer-owned");
        assert!(!source.contains("@audit_schema@"), "{schema}.{name}");
        if schema != "public" {
            assert!(
                !source.contains("public."),
                "{schema}.{name}: body leaks public"
            );
        }
        if name == "internal_audit_allocate_order" {
            assert!(
                source.contains(&format!("{quoted}.internal_audit_order")),
                "{schema}: allocator must name its own state table"
            );
        }
    }
    let triggers: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT t.tgname::text, p.proname::text, n.nspname::text
         FROM pg_trigger t
         JOIN pg_class c ON c.oid = t.tgrelid
         JOIN pg_proc p ON p.oid = t.tgfoid
         JOIN pg_namespace n ON n.oid = p.pronamespace
         WHERE c.relnamespace = $1::text::regnamespace AND NOT t.tgisinternal
           AND p.proname = ANY($2::text[])
         ORDER BY t.tgname",
    )
    .bind(qi(schema))
    .bind(HELPERS.map(String::from).to_vec())
    .fetch_all(pool)
    .await
    .expect("read triggers");
    let mut expected: Vec<(String, String)> = [
        (
            "internal_audit_allocate_order",
            "internal_audit_allocate_order",
        ),
        ("internal_audit_order_guard", "internal_audit_order_guard"),
        (
            "internal_audit_order_truncate_guard",
            "internal_audit_order_guard",
        ),
        (
            "internal_intent_identity_immutable",
            "internal_audit_identity_immutable",
        ),
        (
            "internal_log_identity_immutable",
            "internal_audit_identity_immutable",
        ),
    ]
    .map(|(t, f)| (t.to_owned(), f.to_owned()))
    .to_vec();
    expected.sort();
    assert_eq!(
        triggers
            .iter()
            .map(|t| (t.0.clone(), t.1.clone()))
            .collect::<Vec<_>>(),
        expected,
        "{schema}: trigger set"
    );
    for (trigger, _, function_schema) in &triggers {
        assert_eq!(
            function_schema, schema,
            "{schema}: trigger {trigger} tgfoid must be this schema's helper"
        );
    }
}

/// Insert advances only the target schema's allocator; others stay put.
pub async fn assert_insert_advances_only(
    via: &PgPool,
    target: &str,
    tag: char,
    all: &[(&str, &PgPool)],
) {
    let mut before = Vec::new();
    for (schema, pool) in all {
        before.push(last_audit_id(pool, schema).await);
    }
    let allocated = insert_audit(via, target, tag).await;
    for ((schema, pool), prior) in all.iter().zip(before) {
        let now = last_audit_id(pool, schema).await;
        if *schema == target {
            assert_eq!(now, prior + 1, "{schema}: allocator advances once");
            assert_eq!(allocated, now, "{schema}: row takes the allocated id");
        } else {
            assert_eq!(now, prior, "{schema}: foreign insert must not advance");
        }
    }
}

struct Case {
    name: &'static str,
    base: String,
    extra: Vec<String>,
    path: String,
    setup: String,
    through_471: bool,
    targets: Vec<(&'static str, &'static str)>,
}

fn cases(role: &str) -> Vec<Case> {
    let both = vec![("0471", M471), ("0472", M472)];
    let only_471 = vec![("0471", M471)];
    let only_472 = vec![("0472", M472)];
    let plain = |name: &'static str,
                 base: &str,
                 setup: String,
                 through_471: bool,
                 targets: Vec<(&'static str, &'static str)>| Case {
        name,
        base: base.to_owned(),
        extra: Vec::new(),
        path: qi(base),
        setup,
        through_471,
        targets,
    };
    let function = |base: &str, helper: &str| {
        format!(
            "CREATE FUNCTION {}.{helper}() RETURNS trigger LANGUAGE plpgsql
             AS $f$ BEGIN RETURN NEW; END; $f$;",
            qi(base)
        )
    };
    let overload = |base: &str, helper: &str| {
        format!(
            "CREATE FUNCTION {}.{helper}(integer) RETURNS integer LANGUAGE sql
             AS 'SELECT 1';",
            qi(base)
        )
    };
    let foreign = |base: &str, helper: &str| {
        format!(
            "{} ALTER FUNCTION {}.{helper}() OWNER TO {};",
            function(base, helper),
            qi(base),
            qi(role)
        )
    };
    let mut out = vec![
        // The first existing schema on the path is empty, while unqualified
        // names still find the real tables later on the path.
        Case {
            name: "empty current schema ahead of the real one",
            base: "tog10090_h_real".to_owned(),
            extra: vec!["tog10090_h_empty".to_owned()],
            path: "tog10090_h_empty, tog10090_h_real".to_owned(),
            setup: String::new(),
            through_471: false,
            targets: both.clone(),
        },
    ];
    for table in ["internal_action_log", "internal_idempotency"] {
        let base = format!("tog10090_h_temp_{}", &table[9..12]);
        let setup = format!(
            "CREATE TEMP TABLE {table} (LIKE {}.{table} INCLUDING ALL);",
            qi(&base)
        );
        out.push(Case {
            name: "temp shadow of an audit table",
            path: qi(&base),
            base,
            extra: Vec::new(),
            setup,
            through_471: false,
            targets: both.clone(),
        });
    }
    for (name, unsafe_schema) in [
        ("hyphenated schema", "tog10090-unsafe_audit".to_owned()),
        ("uppercase schema", "TOG10090_Upper".to_owned()),
        ("overlong schema", format!("t{}", "a".repeat(59))),
    ] {
        out.push(plain(
            name,
            &unsafe_schema,
            String::new(),
            false,
            both.clone(),
        ));
    }
    out.push(plain(
        "preexisting audit order table",
        "tog10090_h_state",
        "CREATE TABLE \"tog10090_h_state\".internal_audit_order (singleton boolean);".to_owned(),
        true,
        only_472.clone(),
    ));
    out.push(plain(
        "overloaded identity helper",
        "tog10090_h_over_a",
        overload("tog10090_h_over_a", HELPERS[0]),
        false,
        only_471.clone(),
    ));
    out.push(plain(
        "overloaded allocator helper",
        "tog10090_h_over_b",
        overload("tog10090_h_over_b", HELPERS[1]),
        true,
        only_472.clone(),
    ));
    out.push(plain(
        "overloaded guard helper",
        "tog10090_h_over_c",
        overload("tog10090_h_over_c", HELPERS[2]),
        true,
        only_472.clone(),
    ));
    out.push(plain(
        "foreign-owned identity helper",
        "tog10090_h_own_a",
        foreign("tog10090_h_own_a", HELPERS[0]),
        false,
        only_471,
    ));
    out.push(plain(
        "foreign-owned allocator helper",
        "tog10090_h_own_b",
        foreign("tog10090_h_own_b", HELPERS[1]),
        true,
        only_472.clone(),
    ));
    out.push(plain(
        "foreign-owned guard helper",
        "tog10090_h_own_c",
        foreign("tog10090_h_own_c", HELPERS[2]),
        true,
        only_472,
    ));
    out
}

async fn prepare_schemas(admin: &PgPool, schemas: &[&str]) {
    for schema in schemas {
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {s} CASCADE; CREATE SCHEMA {s};",
            s = qi(schema)
        )))
        .execute(admin)
        .await
        .expect("create hostile scratch schema");
    }
}

/// Raw 0471/0472 refuse hostile installer state transactionally. Needs a
/// role-creating test identity, as the disposable CI service provides.
pub async fn refuses_hostile_installs_without_catalog_change(fixture: &TestDatabase) {
    let admin = fixture.pool();
    let role = format!("{}_foreign", fixture.name());
    for path in ["pg_catalog", "information_schema", "tog10090_absent_only"] {
        let hostile = fixture
            .pool_with_search_path(path)
            .await
            .expect("reserved schema pool");
        let before = snapshot(admin, "public").await;
        for migration in [M471, M472] {
            let error = sqlx::raw_sql(migration)
                .execute(&hostile)
                .await
                .expect_err("reserved or missing schema must refuse");
            assert!(error.as_database_error().is_some());
            assert_eq!(snapshot(admin, "public").await, before);
        }
        hostile.close().await;
    }
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE ROLE {} NOLOGIN;", qi(&role))))
        .execute(admin)
        .await
        .expect("create foreign owner role");
    for case in cases(&role) {
        let mut schemas = vec![case.base.as_str()];
        schemas.extend(case.extra.iter().map(String::as_str));
        prepare_schemas(admin, &schemas).await;
        let base_pool = fixture
            .pool_with_search_path(&qi(&case.base))
            .await
            .expect("base pool");
        chain_below(471)
            .run(&base_pool)
            .await
            .unwrap_or_else(|e| panic!("{}: pre-471 chain: {e}", case.name));
        let hostile = fixture
            .pool_with_search_path(&case.path)
            .await
            .expect("hostile pool");
        // One connection: a TEMP shadow exists only for its own session.
        let mut conn = hostile.acquire().await.expect("hostile connection");
        if !case.setup.is_empty() {
            sqlx::raw_sql(AssertSqlSafe(case.setup.clone()))
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("{}: setup: {e}", case.name));
        }
        if case.through_471 {
            sqlx::raw_sql(M471)
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("{}: 0471 prerequisite: {e}", case.name));
        }
        let mut before = Vec::new();
        for schema in &schemas {
            before.push(snapshot(admin, schema).await);
        }
        for (label, sql) in &case.targets {
            let error = sqlx::raw_sql(*sql)
                .execute(&mut *conn)
                .await
                .expect_err(&format!("{}: {label} must refuse", case.name));
            assert!(
                error.as_database_error().is_some(),
                "{}: {label} must refuse in the database, got {error}",
                case.name
            );
            for (schema, prior) in schemas.iter().zip(&before) {
                assert_eq!(
                    &snapshot(admin, schema).await,
                    prior,
                    "{}: {label} refusal must leave the catalog unchanged ({schema})",
                    case.name
                );
            }
        }
        drop(conn);
        hostile.close().await;
        base_pool.close().await;
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE;",
            qi(&case.base)
        )))
        .execute(admin)
        .await
        .expect("drop hostile scratch schema");
        for extra in &case.extra {
            sqlx::raw_sql(AssertSqlSafe(format!("DROP SCHEMA {} CASCADE;", qi(extra))))
                .execute(admin)
                .await
                .expect("drop hostile extra schema");
        }
    }
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP OWNED BY {r}; DROP ROLE {r};",
        r = qi(&role)
    )))
    .execute(admin)
    .await
    .expect("drop foreign owner role");
}

/// A caller schema that shadows `clock_timestamp` and `isfinite`, listed ahead
/// of `pg_catalog`, with a missing schema first, must bind neither at install
/// nor at insert time.
pub async fn install_and_insert_bind_catalog_builtins(fixture: &TestDatabase) {
    let admin = fixture.pool();
    let schema = "tog10090_shadow";
    prepare_schemas(admin, &[schema]).await;
    let base_pool = fixture
        .pool_with_search_path(&qi(schema))
        .await
        .expect("base pool");
    chain_below(471)
        .run(&base_pool)
        .await
        .expect("pre-471 chain");
    sqlx::raw_sql(
        "CREATE FUNCTION tog10090_shadow.clock_timestamp() RETURNS timestamptz
             LANGUAGE plpgsql AS $f$ BEGIN RAISE EXCEPTION 'shadowed clock_timestamp'; END; $f$;
         CREATE FUNCTION tog10090_shadow.isfinite(timestamptz) RETURNS boolean
             LANGUAGE plpgsql AS $f$ BEGIN RAISE EXCEPTION 'shadowed isfinite'; END; $f$;",
    )
    .execute(admin)
    .await
    .expect("create shadows");
    let hostile = fixture
        .pool_with_search_path("tog10090_absent_first, tog10090_shadow, pg_catalog")
        .await
        .expect("hostile pool");
    for (label, sql) in [("0471", M471), ("0472", M472)] {
        sqlx::raw_sql(sql)
            .execute(&hostile)
            .await
            .unwrap_or_else(|e| panic!("{label} must install despite shadows: {e}"));
    }
    assert_schema_bound(admin, schema).await;
    let dependents: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_depend d
         WHERE d.refobjid IN ('tog10090_shadow.clock_timestamp()'::regprocedure,
                              'tog10090_shadow.isfinite(timestamptz)'::regprocedure)",
    )
    .fetch_one(admin)
    .await
    .expect("shadow dependents");
    assert_eq!(dependents, 0, "no installed object may bind a shadow");
    let first = insert_audit(&hostile, schema, 'd').await;
    let second = insert_audit(admin, schema, 'e').await;
    assert_eq!(second, first + 1, "insert binds catalog clock and isfinite");
    // Evaluates the order CHECK and the guard trigger under the same path.
    sqlx::query(AssertSqlSafe(format!(
        "UPDATE {}.internal_audit_order SET last_audit_id = last_audit_id",
        qi(schema)
    )))
    .execute(&hostile)
    .await
    .expect("guarded no-op update binds catalog isfinite");
    hostile.close().await;
    base_pool.close().await;
    sqlx::raw_sql("DROP SCHEMA tog10090_shadow CASCADE;")
        .execute(admin)
        .await
        .expect("drop shadow schema");
}
