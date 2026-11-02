//! Acceptance tests: admin API slice 0 — token registry + container authorize.
//!
//! Protected test file: the build card implements against these tests and
//! must not edit them. If a pinned name below cannot be implemented as
//! written, stop and escalate instead of changing the tests.
//!
//! Spec: the Admin MCP + REST API design, section 8 slice 0 (token registry
//! and container authorize), sections 4.1 (admin_api_tokens), 4.2 (scope map
//! v0), 4.3 (guild allowlist), 5.1 (hash-only storage) and 5.2 (two-layer
//! enforcement). Owner-approved round-1 decisions apply: operator-issued
//! tokens to named holders including named AI-agent clients (one token
//! each); AI holders get read+moderate scope only; admin scope is
//! humans-only; no self-service issuance.
//!
//! Contract pinned here (build card implements exactly this):
//!
//! 1. Migrations in both chains (the store chain alone never reaches
//!    staging/production, so the cutover file is the canonical one):
//!    `crates/cutover/migrations/0456_admin_api_tokens.sql` and
//!    `crates/store/migrations/0457_admin_api_tokens.sql`. Both create the
//!    same `admin_api_tokens` table (`IF NOT EXISTS` throughout, so
//!    cross-running the chains stays safe in either order), document
//!    `DROP TABLE` rollback in the file header, and gain reviewed checksum
//!    entries in `migrations.lock`. Numbers 0458+ stay reserved for the
//!    later audit/confirm slice; the numbering gate rejects a version
//!    shared across chains. Table shape: `token_id` TEXT PRIMARY KEY
//!    (`adm_` + 16 lowercase hex chars), `secret_hash` TEXT (sha256 hex of
//!    the secret; the secret itself is never stored), `scopes` TEXT[]
//!    (every element in read/moderate/admin; empty allowed),
//!    `guild_ids` BIGINT[] NOT NULL with at least one element,
//!    `label`/`issued_by` TEXT, `issued_at` NOT NULL, `expires_at` NOT NULL
//!    with `expires_at > issued_at`, `revoked_at`/`last_used_at` nullable,
//!    and an index covering (`revoked_at`, `expires_at`) in that order.
//!    Timestamps are TIMESTAMPTZ (house direction); the tests accept TEXT
//!    too and pin nullability plus comparisons, never the type.
//! 2. Five pure associated functions on
//!    `two_bot_core::internal_actions::InternalFlags` (no database, no
//!    clock, no network):
//!    `required_admin_scope(action: &str) -> Option<&'static str>`,
//!    `admin_scope_allows(scopes: &[&str], action: &str) -> bool`,
//!    `admin_token_authorizes(scopes: &[&str], guild_ids: &[i64],
//!    expires_at_unix: i64, revoked: bool, action: &str, guild_id: i64,
//!    now_unix: i64) -> bool`, `is_valid_admin_token_id(id: &str) -> bool`
//!    and `hash_admin_token_secret(secret: &[u8]) -> Option<String>`.
//!    The v0 map covers the 19 verbs the endpoint runs today: read verbs
//!    are `settings.get`, `event.read`, `automations.export`; moderate
//!    verbs are `moderation.warn/timeout/kick/purge/slowmode`,
//!    `guild.add_member`, `event.upsert/cancel`; admin verbs are
//!    `moderation.ban/tempban/lockdown/unlock`, `role.assign` (privilege
//!    grant, deliberately admin), `announcement.post`, `settings.set` and
//!    `automations.import`. Higher scope implies lower for reads only;
//!    writes need exactly the listed scope, never an implied tier. Later
//!    slices extend the map with new verbs; only strings that can never be verbs (typos, empty,
//!    case/whitespace variants) are pinned to no scope here.
//! 3. `authorize` and `assert_allowed` keep their signatures and their
//!    HMAC/flag behavior: the scope/token step is additive and runs after
//!    them. A valid service-key request still authorizes with no token.
//! 4. `sql/database_role_matrix.sql` gains the `admin_api_tokens` row and
//!    `docs/cutover-migration-ledger.md` gains the 0456 row, so the table
//!    the tests prove is the table the runtime can read.
//!
//! Review pins (same protected file, no new suite): `secret_hash` must
//! match `^[0-9a-f]{64}$` through a CHECK named
//! `admin_api_tokens_hash_shape` in both chains; `guild_ids` refuses NULL
//! elements (`array_position(guild_ids, NULL) IS NULL`); malformed
//! `token_id` inserts fail 23514; and the store 0457 DDL equals the
//! cutover 0456 DDL once comments are stripped, so a store-only
//! constraint or index edit fails loudly.
//!
//! Secrets: hash-only storage is pinned here (database row scan plus the
//! hashing vector below). Refusal values carry no secret material (bools
//! and static strings only). Tracing-level redaction of the token bearer
//! belongs to the later Worker/audit slices, which extend the
//! `secret_redaction` corpus once a transport exists.
//!
//! Test shapes: offline unit tests (no database, run by default) pin the
//! pure surface through a deny-all baseline adapter; the builder's
//! inherent associated functions shadow the unqualified calls. Filesystem
//! tests pin the migration/matrix/ledger contract without a database.
//! Database tests (disposable test database only, via
//! `TWO_TEST_DATABASE_URL`, skipped without it outside CI, never staging
//! or production) migrate the cutover chain into an isolated database and
//! pin the table shape, constraints, index, rollback and hash-only
//! storage through direct SQL. No live Discord, no live network.

use std::collections::{HashMap, HashSet};

use two_bot_core::clock_guard::ClockGuard;
use two_bot_core::internal_actions::{
    authorize, body_hash, parse_keys, sign, AuthHeaders, InternalFlags, KeyRing, NonceCache,
    TokenBuckets, NONCE_TTL_SECONDS, SKEW_SECONDS,
};

// ---------------------------------------------------------------------------
// Pinned literals (spec sections 4.1 and 4.2)
// ---------------------------------------------------------------------------

const SCOPE_READ: &str = "read";
const SCOPE_MODERATE: &str = "moderate";
const SCOPE_ADMIN: &str = "admin";

/// The v0 scope map as literals: (verb, required scope). Later slices add
/// rows for new verbs; they never change these nineteen.
const V0_SCOPE_MAP: [(&str, &str); 19] = [
    ("settings.get", SCOPE_READ),
    ("event.read", SCOPE_READ),
    ("automations.export", SCOPE_READ),
    ("moderation.warn", SCOPE_MODERATE),
    ("moderation.timeout", SCOPE_MODERATE),
    ("moderation.kick", SCOPE_MODERATE),
    ("moderation.purge", SCOPE_MODERATE),
    ("moderation.slowmode", SCOPE_MODERATE),
    ("guild.add_member", SCOPE_MODERATE),
    ("event.upsert", SCOPE_MODERATE),
    ("event.cancel", SCOPE_MODERATE),
    ("moderation.ban", SCOPE_ADMIN),
    ("moderation.tempban", SCOPE_ADMIN),
    ("moderation.lockdown", SCOPE_ADMIN),
    ("moderation.unlock", SCOPE_ADMIN),
    ("role.assign", SCOPE_ADMIN),
    ("announcement.post", SCOPE_ADMIN),
    ("settings.set", SCOPE_ADMIN),
    ("automations.import", SCOPE_ADMIN),
];

/// Strings that can never become verbs: the only inputs pinned to no scope.
/// Future real verbs (message/channel/role/passthrough/audit) are
/// deliberately absent: later slices map them without touching this file.
const NEVER_VERBS: [&str; 8] = [
    "",
    "bogus",
    "moderation.bann",
    "settings",
    "ADMIN",
    " moderation.warn",
    "moderation.warn ",
    "moderation.Warn",
];

const CUTOVER_MIGRATION: &str = "0456_admin_api_tokens.sql";
const STORE_MIGRATION: &str = "0457_admin_api_tokens.sql";

const TABLE_COLUMNS: [&str; 10] = [
    "token_id",
    "secret_hash",
    "scopes",
    "guild_ids",
    "label",
    "issued_by",
    "issued_at",
    "expires_at",
    "revoked_at",
    "last_used_at",
];

// ---------------------------------------------------------------------------
// Pure surface: baseline adapter (offline, no database)
// ---------------------------------------------------------------------------

/// Baseline shape of the slice-0 pure surface. At baseline there is no
/// token registry, so the adapter denies everything; every allow-case test
/// below fails until the builder adds the inherent associated functions on
/// `InternalFlags`, which shadow these unqualified calls. The refusal-case
/// tests pass on baseline and constrain green (the builder cannot
/// implement allow-all). Call shapes are fixed: the builder's inherent
/// functions take exactly these argument types.
trait BaselineAdminScope {
    fn required_admin_scope(action: &str) -> Option<&'static str>;
    fn admin_scope_allows(scopes: &[&str], action: &str) -> bool;
    fn admin_token_authorizes(
        scopes: &[&str],
        guild_ids: &[i64],
        expires_at_unix: i64,
        revoked: bool,
        action: &str,
        guild_id: i64,
        now_unix: i64,
    ) -> bool;
    fn is_valid_admin_token_id(id: &str) -> bool;
    fn hash_admin_token_secret(secret: &[u8]) -> Option<String>;
}

impl BaselineAdminScope for InternalFlags {
    fn required_admin_scope(_action: &str) -> Option<&'static str> {
        None
    }

    fn admin_scope_allows(_scopes: &[&str], _action: &str) -> bool {
        false
    }

    fn admin_token_authorizes(
        _scopes: &[&str],
        _guild_ids: &[i64],
        _expires_at_unix: i64,
        _revoked: bool,
        _action: &str,
        _guild_id: i64,
        _now_unix: i64,
    ) -> bool {
        false
    }

    fn is_valid_admin_token_id(_id: &str) -> bool {
        false
    }

    fn hash_admin_token_secret(_secret: &[u8]) -> Option<String> {
        None
    }
}

// ---------------------------------------------------------------------------
// Scope map v0 over the existing verbs
// ---------------------------------------------------------------------------

#[test]
fn scope_map_has_nineteen_rows_without_duplicates() {
    // Guards this table against silent edits; the endpoint may grow new
    // verbs later, which gain rows of their own.
    assert_eq!(V0_SCOPE_MAP.len(), 19);
    let mut verbs = HashSet::new();
    for (verb, scope) in V0_SCOPE_MAP {
        assert!(verbs.insert(verb), "duplicate scope row for {verb}");
        assert!(
            [SCOPE_READ, SCOPE_MODERATE, SCOPE_ADMIN].contains(&scope),
            "{verb} maps to an unknown scope {scope}"
        );
    }
}

#[test]
fn read_verbs_require_read() {
    for verb in ["settings.get", "event.read", "automations.export"] {
        assert_eq!(
            InternalFlags::required_admin_scope(verb),
            Some(SCOPE_READ),
            "{verb} is a read verb"
        );
    }
}

#[test]
fn moderate_verbs_require_moderate() {
    for verb in [
        "moderation.warn",
        "moderation.timeout",
        "moderation.kick",
        "moderation.purge",
        "moderation.slowmode",
        "guild.add_member",
        "event.upsert",
        "event.cancel",
    ] {
        assert_eq!(
            InternalFlags::required_admin_scope(verb),
            Some(SCOPE_MODERATE),
            "{verb} is a moderate verb"
        );
    }
}

#[test]
fn admin_verbs_require_admin() {
    for verb in [
        "moderation.ban",
        "moderation.tempban",
        "moderation.lockdown",
        "moderation.unlock",
        "role.assign",
        "announcement.post",
        "settings.set",
        "automations.import",
    ] {
        assert_eq!(
            InternalFlags::required_admin_scope(verb),
            Some(SCOPE_ADMIN),
            "{verb} is an admin verb"
        );
    }
}

#[test]
fn role_assign_is_admin_not_moderate() {
    // Privilege grants are admin (round-1 decision A2), even though the
    // verb reads like membership tooling next to guild.add_member.
    assert_eq!(
        InternalFlags::required_admin_scope("role.assign"),
        Some(SCOPE_ADMIN)
    );
    assert!(
        !InternalFlags::admin_scope_allows(&[SCOPE_MODERATE], "role.assign"),
        "moderate must not grant role.assign"
    );
}

#[test]
fn never_verbs_have_no_scope() {
    for verb in NEVER_VERBS {
        assert_eq!(
            InternalFlags::required_admin_scope(verb),
            None,
            "{verb:?} must never map to a scope"
        );
        assert!(
            !InternalFlags::admin_scope_allows(&[SCOPE_ADMIN], verb),
            "even admin must not allow {verb:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scope allowance: higher scope implies lower for reads only
// ---------------------------------------------------------------------------

#[test]
fn every_tier_allows_read_verbs() {
    for scope in [SCOPE_READ, SCOPE_MODERATE, SCOPE_ADMIN] {
        for verb in ["settings.get", "event.read", "automations.export"] {
            assert!(
                InternalFlags::admin_scope_allows(&[scope], verb),
                "{scope} must allow the read verb {verb}"
            );
        }
    }
}

#[test]
fn only_explicit_moderate_allows_moderate_verbs() {
    for verb in [
        "moderation.warn",
        "moderation.kick",
        "guild.add_member",
        "event.cancel",
    ] {
        assert!(
            InternalFlags::admin_scope_allows(&[SCOPE_MODERATE], verb),
            "explicit moderate must allow {verb}"
        );
        assert!(
            !InternalFlags::admin_scope_allows(&[SCOPE_ADMIN], verb),
            "A2: admin alone must not imply the moderate write {verb}"
        );
    }
}

#[test]
fn exact_write_scope_matrix_and_read_implication_cover_every_existing_verb() {
    // Literal expectations from A2, not computed using the production map.
    let cases: &[(&[&str], [bool; 3])] = &[
        (&[], [false, false, false]),
        (&[SCOPE_READ], [true, false, false]),
        (&[SCOPE_MODERATE], [true, true, false]),
        (&[SCOPE_ADMIN], [true, false, true]),
        (&[SCOPE_READ, SCOPE_ADMIN], [true, false, true]),
        (&[SCOPE_MODERATE, SCOPE_ADMIN], [true, true, true]),
        (
            &[SCOPE_READ, SCOPE_MODERATE, SCOPE_ADMIN],
            [true, true, true],
        ),
        (&["ADMIN", "moderate "], [false, false, false]),
    ];
    for &(scopes, expected) in cases {
        for (verb, required) in V0_SCOPE_MAP {
            let column = match required {
                SCOPE_READ => 0,
                SCOPE_MODERATE => 1,
                SCOPE_ADMIN => 2,
                _ => panic!("invalid acceptance scope literal"),
            };
            assert_eq!(
                InternalFlags::admin_scope_allows(scopes, verb),
                expected[column],
                "{scopes:?} on {verb}: reads imply, writes require explicit scope"
            );
            assert_eq!(
                live_token_allows(scopes, verb, GUILD),
                expected[column],
                "token predicate must preserve exact scope on {verb}"
            );
        }
    }
}

#[test]
fn only_admin_allows_admin_verbs() {
    for verb in [
        "moderation.ban",
        "moderation.lockdown",
        "role.assign",
        "announcement.post",
        "settings.set",
        "automations.import",
    ] {
        assert!(
            InternalFlags::admin_scope_allows(&[SCOPE_ADMIN], verb),
            "admin must allow {verb}"
        );
    }
}

#[test]
fn read_refuses_all_writes() {
    for verb in [
        "moderation.warn",
        "moderation.kick",
        "moderation.purge",
        "guild.add_member",
        "event.upsert",
        "moderation.ban",
        "role.assign",
        "announcement.post",
        "settings.set",
        "automations.import",
    ] {
        assert!(
            !InternalFlags::admin_scope_allows(&[SCOPE_READ], verb),
            "read must refuse the write verb {verb}"
        );
    }
}

#[test]
fn moderate_refuses_admin_verbs() {
    for verb in [
        "moderation.ban",
        "moderation.tempban",
        "moderation.lockdown",
        "moderation.unlock",
        "role.assign",
        "settings.set",
        "automations.import",
    ] {
        assert!(
            !InternalFlags::admin_scope_allows(&[SCOPE_MODERATE], verb),
            "moderate must refuse the admin verb {verb}"
        );
    }
}

#[test]
fn unknown_scopes_contribute_nothing() {
    assert!(
        !InternalFlags::admin_scope_allows(&["bogus"], "event.read"),
        "an unknown scope must not allow even reads"
    );
    assert!(
        !InternalFlags::admin_scope_allows(&[], "event.read"),
        "no scope must not allow even reads"
    );
    assert!(
        InternalFlags::admin_scope_allows(&["bogus", SCOPE_READ], "event.read"),
        "a held read scope still allows beside an unknown one"
    );
    assert!(
        !InternalFlags::admin_scope_allows(&["bogus", SCOPE_READ], "moderation.warn"),
        "an unknown scope must not widen read into moderate"
    );
}

#[test]
fn scope_names_are_lowercase_literals() {
    for held in ["Read", "READ", " read", "read "] {
        assert!(
            !InternalFlags::admin_scope_allows(&[held], "event.read"),
            "{held:?} is not the read scope"
        );
    }
}

#[test]
fn multi_scope_tokens_allow_the_union() {
    assert!(InternalFlags::admin_scope_allows(
        &[SCOPE_READ, SCOPE_MODERATE],
        "moderation.kick"
    ));
    assert!(!InternalFlags::admin_scope_allows(
        &[SCOPE_READ, SCOPE_MODERATE],
        "moderation.ban"
    ));
}

// ---------------------------------------------------------------------------
// Token authorize predicate: scope + guild + expiry + revocation
// ---------------------------------------------------------------------------

const GUILD: i64 = 326474832151838730;
const OTHER_GUILD: i64 = 999988887777666555;
const NOW: i64 = 1_791_000_000;
const EXPIRES: i64 = NOW + 90 * 86400;

/// Happy-path registry row: live token for GUILD expiring in 90 days (the
/// bounded lifetime from decision A6) at a fixed synthetic clock.
fn live_token_allows(scopes: &[&str], action: &str, guild_id: i64) -> bool {
    InternalFlags::admin_token_authorizes(scopes, &[GUILD], EXPIRES, false, action, guild_id, NOW)
}

#[test]
fn live_tokens_allow_their_scope() {
    assert!(live_token_allows(&[SCOPE_READ], "event.read", GUILD));
    assert!(live_token_allows(
        &[SCOPE_MODERATE],
        "moderation.kick",
        GUILD
    ));
    assert!(live_token_allows(&[SCOPE_ADMIN], "moderation.ban", GUILD));
    assert!(live_token_allows(&[SCOPE_ADMIN], "event.read", GUILD));
}

#[test]
fn revoked_tokens_refuse_everything() {
    for action in ["event.read", "moderation.kick", "moderation.ban"] {
        assert!(
            !InternalFlags::admin_token_authorizes(
                &[SCOPE_ADMIN],
                &[GUILD],
                EXPIRES,
                true,
                action,
                GUILD,
                NOW
            ),
            "revoked admin token must refuse {action}"
        );
    }
}

#[test]
fn expired_tokens_refuse_at_the_boundary() {
    // Valid while now < expires; the expiry instant itself refuses.
    assert!(
        InternalFlags::admin_token_authorizes(
            &[SCOPE_READ],
            &[GUILD],
            EXPIRES,
            false,
            "event.read",
            GUILD,
            EXPIRES - 1
        ),
        "one second before expiry still allows"
    );
    for now in [EXPIRES, EXPIRES + 1, EXPIRES + 86400] {
        assert!(
            !InternalFlags::admin_token_authorizes(
                &[SCOPE_READ],
                &[GUILD],
                EXPIRES,
                false,
                "event.read",
                GUILD,
                now
            ),
            "at or after expiry must refuse (now={now})"
        );
    }
}

#[test]
fn cross_guild_tokens_refuse() {
    assert!(
        !live_token_allows(&[SCOPE_ADMIN], "event.read", OTHER_GUILD),
        "admin token for another guild must refuse"
    );
    assert!(
        !InternalFlags::admin_token_authorizes(
            &[SCOPE_ADMIN],
            &[],
            EXPIRES,
            false,
            "event.read",
            GUILD,
            NOW
        ),
        "an empty guild allowlist refuses everything"
    );
    assert!(
        InternalFlags::admin_token_authorizes(
            &[SCOPE_READ],
            &[OTHER_GUILD, GUILD],
            EXPIRES,
            false,
            "event.read",
            GUILD,
            NOW
        ),
        "membership anywhere in the allowlist allows"
    );
}

#[test]
fn wrong_scope_refuses_even_when_live() {
    assert!(!live_token_allows(&[SCOPE_READ], "moderation.ban", GUILD));
    assert!(!live_token_allows(&[SCOPE_MODERATE], "role.assign", GUILD));
}

#[test]
fn never_verbs_refuse_even_when_live() {
    for verb in NEVER_VERBS {
        assert!(
            !live_token_allows(&[SCOPE_ADMIN], verb, GUILD),
            "live admin token must refuse {verb:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Token id shape: `adm_` + 16 lowercase hex chars
// ---------------------------------------------------------------------------

#[test]
fn well_formed_ids_validate() {
    for id in [
        "adm_0123456789abcdef",
        "adm_ffffffffffffffff",
        "adm_0000000000000000",
    ] {
        assert!(
            InternalFlags::is_valid_admin_token_id(id),
            "{id} must validate"
        );
    }
}

#[test]
fn malformed_ids_refuse() {
    for id in [
        "",
        "adm_",
        "adm",
        "tok_0123456789abcdef",
        "ADM_0123456789abcdef",
        "adm_0123456789abcde",
        "adm_0123456789abcdef0",
        "adm_0123456789ABCDEF",
        "adm_0123456789abcdeg",
        " adm_0123456789abcdef",
        "adm_0123456789abcdef ",
        "adm_0123456789abcdef\n",
    ] {
        assert!(
            !InternalFlags::is_valid_admin_token_id(id),
            "{id:?} must not validate"
        );
    }
}

// ---------------------------------------------------------------------------
// Secret hashing: sha256 hex, 32-byte minimum, hash-only storage
// ---------------------------------------------------------------------------

/// RFC 4231 / FIPS 180-2 long vector (56 bytes, over the minimum): the
/// literal that pins the algorithm even if both sides share one crate.
const SHA256_LONG_INPUT: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
const SHA256_LONG_DIGEST: &str = "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1";

const FIXTURE_SECRET: &[u8] = b"fixture-admin-token-secret-32bytes!";
const FIXTURE_SECRET_SHA256: &str =
    "4bc7cb80ab331af138344f0e0508ea18d0e367b15e785ff40fa529dca6467a56";

#[test]
fn short_secrets_have_no_hash() {
    assert!(FIXTURE_SECRET.len() >= 32, "fixture must clear the minimum");
    for len in [0, 1, 31] {
        let secret = vec![b'x'; len];
        assert_eq!(
            InternalFlags::hash_admin_token_secret(&secret),
            None,
            "{len} bytes must not hash"
        );
    }
}

#[test]
fn thirty_two_bytes_is_the_minimum() {
    let secret = vec![b'y'; 32];
    let hash = InternalFlags::hash_admin_token_secret(&secret);
    assert!(hash.is_some(), "exactly 32 bytes must hash");
    let hash = hash.expect("checked above");
    assert_eq!(hash.len(), 64, "sha256 hex is 64 chars");
    assert!(
        hash.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "lowercase hex only: {hash}"
    );
}

#[test]
fn hash_matches_the_sha256_long_vector() {
    assert_eq!(
        InternalFlags::hash_admin_token_secret(SHA256_LONG_INPUT).as_deref(),
        Some(SHA256_LONG_DIGEST),
        "algorithm is sha256"
    );
}

#[test]
fn hash_matches_an_independent_digest() {
    use sha2::{Digest, Sha256};
    let expected = hex::encode(Sha256::digest(FIXTURE_SECRET));
    assert_eq!(expected, FIXTURE_SECRET_SHA256, "fixture literal drifted");
    assert_eq!(
        InternalFlags::hash_admin_token_secret(FIXTURE_SECRET).as_deref(),
        Some(FIXTURE_SECRET_SHA256)
    );
    // Deterministic: the registry lookup re-hashes the presented secret.
    assert_eq!(
        InternalFlags::hash_admin_token_secret(FIXTURE_SECRET),
        InternalFlags::hash_admin_token_secret(FIXTURE_SECRET)
    );
}

#[test]
fn hash_output_never_contains_the_secret() {
    let rendered = format!(
        "{:?}",
        InternalFlags::hash_admin_token_secret(FIXTURE_SECRET)
    );
    assert!(
        !rendered.contains("fixture-admin-token-secret"),
        "hash output must not echo the secret: {rendered}"
    );
    // The Debug form of None (baseline) trivially passes; post-green it
    // guards a future refactor that prints the input beside the digest.
}

#[test]
fn baseline_adapter_stays_live_after_green() {
    // Explicit trait paths: keeps every BaselineAdminScope method used
    // after the builder's inherent functions shadow the unqualified calls
    // above, so clippy `-D warnings` stays green in both states. The
    // adapter denies/empties everything by construction.
    assert_eq!(
        <InternalFlags as BaselineAdminScope>::required_admin_scope("event.read"),
        None
    );
    assert!(!<InternalFlags as BaselineAdminScope>::admin_scope_allows(
        &[SCOPE_ADMIN],
        "event.read"
    ));
    assert!(
        !<InternalFlags as BaselineAdminScope>::admin_token_authorizes(
            &[SCOPE_ADMIN],
            &[GUILD],
            EXPIRES,
            false,
            "event.read",
            GUILD,
            NOW
        )
    );
    assert!(
        !<InternalFlags as BaselineAdminScope>::is_valid_admin_token_id("adm_0123456789abcdef")
    );
    assert_eq!(
        <InternalFlags as BaselineAdminScope>::hash_admin_token_secret(FIXTURE_SECRET),
        None
    );
}

// ---------------------------------------------------------------------------
// Control: the HMAC/flag gate is unchanged and sufficient for service keys
// ---------------------------------------------------------------------------

#[test]
fn hmac_service_key_path_authorizes_without_a_token() {
    // Slice 0 adds its step AFTER this gate; the gate itself keeps its
    // signature and behavior. Passes before and after the build.
    // Derived (never a hardcoded HMAC-secret literal): body_hash returns 64
    // hex chars, clearing the 32-byte key minimum.
    let secret = body_hash(b"admin-token-registry-acceptance-hmac-secret");
    let keys = KeyRing::new(parse_keys(&format!("test-key:{secret}")).expect("test key parses"));
    let raw = br#"{"action":"event.read"}"#;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_secs();
    let timestamp = now_secs.to_string();
    // 32 hex chars, derived (never a hardcoded nonce literal).
    let nonce = body_hash(b"admin-token-registry-acceptance-control")[..32].to_owned();
    let signature = sign(secret.as_bytes(), &timestamp, &nonce, raw);
    let headers = AuthHeaders {
        key_id: "test-key",
        timestamp: &timestamp,
        nonce: &nonce,
        signature: &signature,
    };
    let mut vars = HashMap::new();
    vars.insert("TWO_INTERNAL_ALLOW_EVENT_READ".to_owned(), "1".to_owned());
    let flags = InternalFlags::from_map(&vars);
    let mut nonces = NonceCache::new(NONCE_TTL_SECONDS);
    let mut buckets = TokenBuckets::new();
    let mut clock = ClockGuard::new();
    let decision = authorize(
        &headers,
        raw,
        &keys,
        &flags,
        true,
        true,
        SKEW_SECONDS,
        now_secs * 1000,
        &mut nonces,
        &mut buckets,
        &mut clock,
    )
    .expect("valid service-key request authorizes");
    assert_eq!(decision.action, "event.read");
    assert_eq!(decision.key_id, "test-key");
}

// ---------------------------------------------------------------------------
// Migration and contract files (offline: filesystem only, no database)
// ---------------------------------------------------------------------------

fn manifest_relative(path: &str) -> String {
    format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path)
}

fn read_manifest_file(path: &str) -> String {
    let full = manifest_relative(path);
    std::fs::read_to_string(&full).unwrap_or_else(|_| panic!("contract file must exist at {full}"))
}

#[test]
fn cutover_migration_creates_the_registry() {
    let sql = read_manifest_file(&format!("../cutover/migrations/{CUTOVER_MIGRATION}"));
    assert!(
        sql.contains("CREATE TABLE") && sql.contains("IF NOT EXISTS"),
        "cutover migration must create the table idempotently"
    );
    assert!(sql.contains("admin_api_tokens"), "table name");
    for column in TABLE_COLUMNS {
        assert!(sql.contains(column), "cutover migration must add {column}");
    }
    assert!(sql.contains("PRIMARY KEY"), "token_id primary key");
    assert!(sql.contains("CHECK"), "scopes/expiry/guild CHECKs");
    assert!(
        sql.contains("cardinality") || sql.contains("array_length"),
        "guild_ids non-empty check needs an array-length predicate"
    );
    assert!(
        sql.contains("CREATE INDEX") && sql.contains("revoked_at") && sql.contains("expires_at"),
        "live-token lookup index"
    );
    let lowered = sql.to_ascii_lowercase();
    assert!(
        lowered.contains("rollback") && sql.contains("DROP TABLE"),
        "migration must document DROP TABLE rollback"
    );
}

#[test]
fn store_migration_mirrors_the_registry() {
    let sql = read_manifest_file(&format!("../store/migrations/{STORE_MIGRATION}"));
    assert!(
        sql.contains("CREATE TABLE") && sql.contains("IF NOT EXISTS"),
        "store migration must create the table idempotently"
    );
    assert!(sql.contains("admin_api_tokens"), "table name");
    for column in TABLE_COLUMNS {
        assert!(sql.contains(column), "store migration must add {column}");
    }
    assert!(sql.contains("PRIMARY KEY"), "token_id primary key");
    assert!(sql.contains("CHECK"), "scopes/expiry/guild CHECKs");
    let lowered = sql.to_ascii_lowercase();
    assert!(
        lowered.contains("rollback") && sql.contains("DROP TABLE"),
        "migration must document DROP TABLE rollback"
    );
}

#[test]
fn migrations_lock_covers_both_chains() {
    let lock = read_manifest_file("../../migrations.lock");
    for file in [CUTOVER_MIGRATION, STORE_MIGRATION] {
        assert!(
            lock.contains(file),
            "migrations.lock needs a reviewed checksum for {file}"
        );
    }
}

#[test]
fn role_matrix_covers_the_registry() {
    let matrix = read_manifest_file("../../sql/database_role_matrix.sql");
    assert!(
        matrix.contains("admin_api_tokens"),
        "the runtime grant matrix must list admin_api_tokens"
    );
}

#[test]
fn cutover_ledger_records_the_migration() {
    let ledger = read_manifest_file("../../docs/cutover-migration-ledger.md");
    assert!(
        ledger.contains("0456_admin_api_tokens"),
        "the cutover ledger must record 0456_admin_api_tokens"
    );
}

// ---------------------------------------------------------------------------
// Review pins: filesystem half (offline, no database)
// ---------------------------------------------------------------------------

/// Strip `--` comment lines and blank lines, then collapse every whitespace
/// run to one space: the two chains document the same table under different
/// file headers, so only the normalized DDL must match.
fn normalized_ddl(sql: &str) -> String {
    sql.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn both_migrations_name_the_hash_shape_check() {
    // Review pin 1 (named CHECK, both chains): the database probes below pin
    // the behavior; this pins the constraint name in each chain.
    for file in [
        format!("../cutover/migrations/{CUTOVER_MIGRATION}"),
        format!("../store/migrations/{STORE_MIGRATION}"),
    ] {
        let ddl = normalized_ddl(&read_manifest_file(&file));
        assert!(
            ddl.contains("admin_api_tokens_hash_shape"),
            "{file} must name the secret_hash shape CHECK"
        );
    }
}

#[test]
fn both_migrations_reject_null_guild_elements() {
    // Review pin 2 (predicate text, both chains): the guild CHECK carries
    // exactly this predicate; the probes below pin the behavior.
    for file in [
        format!("../cutover/migrations/{CUTOVER_MIGRATION}"),
        format!("../store/migrations/{STORE_MIGRATION}"),
    ] {
        let ddl = normalized_ddl(&read_manifest_file(&file));
        assert!(
            ddl.contains("array_position(guild_ids, NULL) IS NULL"),
            "{file} must refuse NULL guild elements"
        );
    }
}

#[test]
fn store_migration_matches_cutover_ddl() {
    // Review pin 3 (store mirror): the store file is an independent copy, so
    // a store-only constraint or index edit must fail loudly here even when
    // every cutover-chain probe stays green.
    let cutover = normalized_ddl(&read_manifest_file(&format!(
        "../cutover/migrations/{CUTOVER_MIGRATION}"
    )));
    let store = normalized_ddl(&read_manifest_file(&format!(
        "../store/migrations/{STORE_MIGRATION}"
    )));
    assert!(
        !cutover.is_empty() && !store.is_empty(),
        "normalized DDL must not be empty"
    );
    for pinned in [
        "CREATE TABLE IF NOT EXISTS admin_api_tokens",
        "admin_api_tokens_id_shape",
        "admin_api_tokens_hash_shape",
        "admin_api_tokens_scopes_closed",
        "admin_api_tokens_guilds_nonempty",
        "admin_api_tokens_expiry_ordered",
        "idx_admin_api_tokens_live",
    ] {
        assert!(cutover.contains(pinned), "cutover DDL must pin {pinned}");
        assert!(store.contains(pinned), "store DDL must pin {pinned}");
    }
    assert_eq!(
        store, cutover,
        "store 0457 must mirror cutover 0456 exactly (comments aside)"
    );
}

// ---------------------------------------------------------------------------
// Disposable-database pins (cutover chain, isolated database per test)
// ---------------------------------------------------------------------------

/// Whole-module gate: without the `db` feature this file is offline-only.
#[cfg(feature = "db")]
mod registry_db {
    use two_bot_testsupport::TestDatabase;

    const TEST_TOKEN: &str = "adm_0123456789abcdef";
    const TEST_HASH: &str = super::FIXTURE_SECRET_SHA256;
    // sqlx binds `&str` as TEXT and Postgres has no text→timestamptz
    // cast, so every use of these below carries `::timestamptz`.
    const ISSUED: &str = "2026-09-01T00:00:00Z";
    const EXPIRES: &str = "2026-11-30T00:00:00Z";

    async fn connect(test: &str) -> Option<TestDatabase> {
        match std::env::var("TWO_TEST_DATABASE_URL") {
            Ok(url) => Some(
                TestDatabase::create(&url, &sqlx::migrate!("../cutover/migrations"))
                    .await
                    .expect("create migrated test database"),
            ),
            Err(_) => {
                assert_ne!(
                    std::env::var("GITHUB_ACTIONS").as_deref(),
                    Ok("true"),
                    "CI must configure admin-token-registry acceptance ({test})"
                );
                eprintln!(
                    "SKIP admin_token_registry_acceptance ({test}): TWO_TEST_DATABASE_URL not set"
                );
                None
            }
        }
    }

    async fn table_exists(pool: &sqlx::PgPool) -> bool {
        sqlx::query_scalar(
            "SELECT EXISTS (
               SELECT 1 FROM information_schema.tables
                WHERE table_name = 'admin_api_tokens'
             )",
        )
        .fetch_one(pool)
        .await
        .expect("probe registry table")
    }

    /// Assert a rejected write failed with the expected SQLSTATE (the
    /// *reason*), not just rejection: a type error (42804) here means the
    /// test's own binds are wrong, not the constraint.
    fn assert_sqlstate(err: &sqlx::Error, expected: &str, context: &str) {
        let db_err = err
            .as_database_error()
            .unwrap_or_else(|| panic!("{context}: expected a Postgres error, got {err:?}"));
        assert_eq!(
            db_err.code().as_deref(),
            Some(expected),
            "{context}: wrong SQLSTATE"
        );
    }

    /// Insert the standard live token (revoked_at/last_used_at default).
    async fn insert_live_token(pool: &sqlx::PgPool, token_id: &str) {
        sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', $1, $2, '{read}', '{326474832151838730}',
                     'fixture holder', 'fixture operator', $3::timestamptz, $4::timestamptz)",
        )
        .bind(token_id)
        .bind(TEST_HASH)
        .bind(ISSUED)
        .bind(EXPIRES)
        .execute(pool)
        .await
        .expect("valid token inserts");
    }

    #[tokio::test]
    async fn registry_table_exists_with_contracted_shape() {
        let Some(db) = connect("table-shape").await else {
            return;
        };
        assert!(
            table_exists(db.pool()).await,
            "admin_api_tokens is missing: migration 0456 was not applied"
        );
        let columns: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name, udt_name, is_nullable
               FROM information_schema.columns
              WHERE table_name = 'admin_api_tokens'
              ORDER BY ordinal_position",
        )
        .fetch_all(db.pool())
        .await
        .expect("read registry columns");
        let mut by_name = std::collections::HashMap::new();
        for (name, udt, nullable) in &columns {
            by_name.insert(name.clone(), (udt.clone(), nullable.clone()));
        }
        for column in super::TABLE_COLUMNS {
            assert!(by_name.contains_key(column), "missing column {column}");
        }
        for column in ["holder_id", "holder_kind"] {
            assert!(
                by_name.contains_key(column),
                "forward repair must add {column}"
            );
            assert_eq!(by_name[column].0, "text", "holder authority is TEXT");
            assert_eq!(by_name[column].1, "NO", "holder authority is NOT NULL");
        }
        assert_eq!(by_name["token_id"].0, "text");
        assert_eq!(by_name["secret_hash"].0, "text");
        assert_eq!(by_name["scopes"].0, "_text", "scopes is TEXT[]");
        assert_eq!(by_name["guild_ids"].0, "_int8", "guild_ids is BIGINT[]");
        assert_eq!(by_name["issued_at"].1, "NO", "issued_at is NOT NULL");
        assert_eq!(by_name["expires_at"].1, "NO", "expires_at is NOT NULL");
        assert_eq!(by_name["guild_ids"].1, "NO", "guild_ids is NOT NULL");
        assert_eq!(by_name["revoked_at"].1, "YES", "revoked_at starts NULL");
        assert_eq!(by_name["last_used_at"].1, "YES", "last_used_at starts NULL");
        let primary_key: Vec<String> = sqlx::query_scalar(
            "SELECT kcu.column_name
               FROM information_schema.table_constraints tc
               JOIN information_schema.key_column_usage kcu
                 ON kcu.constraint_name = tc.constraint_name
                AND kcu.constraint_schema = tc.constraint_schema
              WHERE tc.table_name = 'admin_api_tokens'
                AND tc.constraint_type = 'PRIMARY KEY'
              ORDER BY kcu.ordinal_position",
        )
        .fetch_all(db.pool())
        .await
        .expect("read registry primary key");
        assert_eq!(
            primary_key,
            vec!["token_id".to_owned()],
            "primary key must be exactly (token_id)"
        );
    }

    #[tokio::test]
    async fn live_token_lookup_index_covers_revoked_then_expires() {
        let Some(db) = connect("lookup-index").await else {
            return;
        };
        assert!(table_exists(db.pool()).await, "0456 was not applied");
        let defs: Vec<String> = sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'admin_api_tokens'",
        )
        .fetch_all(db.pool())
        .await
        .expect("read registry indexes");
        let ordered =
            defs.iter().any(
                |def| match (def.find("revoked_at"), def.find("expires_at")) {
                    (Some(revoked), Some(expires)) => revoked < expires,
                    _ => false,
                },
            );
        assert!(
            ordered,
            "need an index on (revoked_at, expires_at) in that order, found {defs:?}"
        );
    }

    #[tokio::test]
    async fn valid_token_round_trips_with_null_lifecycle_columns() {
        let Some(db) = connect("round-trip").await else {
            return;
        };
        insert_live_token(db.pool(), TEST_TOKEN).await;
        let row: (
            String,
            String,
            Vec<String>,
            Vec<i64>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as(
            "SELECT token_id, secret_hash, scopes, guild_ids,
                    revoked_at::text, last_used_at::text
               FROM admin_api_tokens WHERE token_id = $1",
        )
        .bind(TEST_TOKEN)
        .fetch_one(db.pool())
        .await
        .expect("read back the token");
        assert_eq!(row.0, TEST_TOKEN);
        assert_eq!(row.1, TEST_HASH);
        assert_eq!(row.2, vec!["read".to_owned()]);
        assert_eq!(row.3, vec![326474832151838730_i64]);
        assert_eq!(row.4, None, "revoked_at starts NULL (live)");
        assert_eq!(row.5, None, "last_used_at starts NULL");
        // Lifecycle columns stay writable: revoke and touch work.
        sqlx::query(
            "UPDATE admin_api_tokens
                SET revoked_at = $1::timestamptz, last_used_at = $1::timestamptz
              WHERE token_id = $2",
        )
        .bind(EXPIRES)
        .bind(TEST_TOKEN)
        .execute(db.pool())
        .await
        .expect("revoke and touch update");
        let live: bool = sqlx::query_scalar(
            "SELECT revoked_at IS NOT NULL AND last_used_at IS NOT NULL
               FROM admin_api_tokens WHERE token_id = $1",
        )
        .bind(TEST_TOKEN)
        .fetch_one(db.pool())
        .await
        .expect("read lifecycle state");
        assert!(live, "revoke and touch must persist");
    }

    #[tokio::test]
    async fn scope_check_rejects_unknown_scopes_but_allows_empty() {
        let Some(db) = connect("scope-check").await else {
            return;
        };
        let err = sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', 'adm_2000000000000000', $1, '{read,superuser}', '{1}', 'h', 'o', $2::timestamptz, $3::timestamptz)",
        )
        .bind(TEST_HASH)
        .bind(ISSUED)
        .bind(EXPIRES)
        .execute(db.pool())
        .await
        .expect_err("unknown scopes must be rejected");
        assert_sqlstate(&err, "23514", "scope CHECK");
        sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', 'adm_2000000000000001', $1, '{}', '{1}', 'h', 'o', $2::timestamptz, $3::timestamptz)",
        )
        .bind(TEST_HASH)
        .bind(ISSUED)
        .bind(EXPIRES)
        .execute(db.pool())
        .await
        .expect("empty scopes stay allowed: only guild_ids is non-empty");
    }

    #[tokio::test]
    async fn guild_allowlist_must_be_present_and_non_empty() {
        let Some(db) = connect("guild-allowlist").await else {
            return;
        };
        let err = sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', 'adm_2100000000000000', $1, '{read}', '{}', 'h', 'o', $2::timestamptz, $3::timestamptz)",
        )
        .bind(TEST_HASH)
        .bind(ISSUED)
        .bind(EXPIRES)
        .execute(db.pool())
        .await
        .expect_err("empty guild_ids must be rejected");
        assert_sqlstate(&err, "23514", "guild CHECK");
        let err = sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', 'adm_2100000000000001', $1, '{read}', NULL, 'h', 'o', $2::timestamptz, $3::timestamptz)",
        )
        .bind(TEST_HASH)
        .bind(ISSUED)
        .bind(EXPIRES)
        .execute(db.pool())
        .await
        .expect_err("NULL guild_ids must be rejected");
        assert_sqlstate(&err, "23502", "guild NOT NULL");
    }

    #[tokio::test]
    async fn expiry_must_follow_issuance() {
        let Some(db) = connect("expiry-order").await else {
            return;
        };
        for (token_id, issued, expires) in [
            ("adm_2200000000000000", EXPIRES, EXPIRES),
            ("adm_2200000000000001", EXPIRES, ISSUED),
        ] {
            let err = sqlx::query(
                "INSERT INTO admin_api_tokens
                 (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
                 VALUES ('fixture-holder', 'human', $1, $2, '{read}', '{1}', 'h', 'o', $3::timestamptz, $4::timestamptz)",
            )
            .bind(token_id)
            .bind(TEST_HASH)
            .bind(issued)
            .bind(expires)
            .execute(db.pool())
            .await
            .expect_err("{token_id}: expires_at must exceed issued_at");
            assert_sqlstate(&err, "23514", "expiry CHECK");
        }
        let err = sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', 'adm_2200000000000002', $1, '{read}', '{1}', 'h', 'o', $2::timestamptz, NULL)",
        )
        .bind(TEST_HASH)
        .bind(ISSUED)
        .execute(db.pool())
        .await
        .expect_err("NULL expires_at must be rejected");
        assert_sqlstate(&err, "23502", "expiry NOT NULL");
    }

    #[tokio::test]
    async fn token_id_is_unique() {
        let Some(db) = connect("token-unique").await else {
            return;
        };
        insert_live_token(db.pool(), TEST_TOKEN).await;
        let err = sqlx::query(
            "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', $1, $2, '{admin}', '{2}', 'other', 'other', $3::timestamptz, $4::timestamptz)",
        )
        .bind(TEST_TOKEN)
        .bind(TEST_HASH)
        .bind(ISSUED)
        .bind(EXPIRES)
        .execute(db.pool())
        .await
        .expect_err("duplicate token_id must be rejected");
        assert_sqlstate(&err, "23505", "token_id uniqueness");
    }

    #[tokio::test]
    async fn stored_row_never_contains_the_secret() {
        let Some(db) = connect("hash-only").await else {
            return;
        };
        insert_live_token(db.pool(), TEST_TOKEN).await;
        // Every contracted column as text: the raw secret appears nowhere.
        let cells: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT unnest(ARRAY[
                token_id, secret_hash,
                array_to_string(scopes, ','),
                array_to_string(guild_ids, ','),
                label, issued_by, holder_id, holder_kind,
                issued_at::text, expires_at::text,
                revoked_at::text, last_used_at::text
             ])
               FROM admin_api_tokens WHERE token_id = $1",
        )
        .bind(TEST_TOKEN)
        .fetch_all(db.pool())
        .await
        .expect("read every cell as text");
        assert_eq!(
            cells.len(),
            12,
            "all twelve columns including holder authority scanned"
        );
        let secret = std::str::from_utf8(super::FIXTURE_SECRET).expect("printable fixture");
        for cell in &cells {
            assert!(
                cell.as_deref().is_none_or(|text| !text.contains(secret)),
                "raw secret stored in the row: {cells:?}"
            );
        }
        assert!(
            cells.iter().any(|cell| cell.as_deref() == Some(TEST_HASH)),
            "the stored hash must equal sha256(secret)"
        );
    }

    #[tokio::test]
    async fn registry_rolls_back_with_drop_table() {
        let Some(db) = connect("rollback").await else {
            return;
        };
        assert!(table_exists(db.pool()).await, "0456 was not applied");
        insert_live_token(db.pool(), TEST_TOKEN).await;
        sqlx::query("DROP TABLE admin_api_tokens")
            .execute(db.pool())
            .await
            .expect("registry drops cleanly");
        assert!(!table_exists(db.pool()).await, "drop must remove the table");
        // Re-applying the reviewed file restores the registry: the
        // migration is independently re-runnable, not just chain-ordered.
        let path = format!(
            "{}/../cutover/migrations/{}",
            env!("CARGO_MANIFEST_DIR"),
            super::CUTOVER_MIGRATION
        );
        let sql = std::fs::read_to_string(&path).expect("migration file readable");
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(db.pool())
            .await
            .expect("re-apply restores the table");
        assert!(table_exists(db.pool()).await, "re-apply must restore");
        // Historical DDL stays independently usable; it intentionally lacks
        // the holder columns introduced by the separate forward repair.
        sqlx::query(LEGACY_INSERT_PLAIN)
            .bind(TEST_TOKEN)
            .bind(TEST_HASH)
            .bind(ISSUED)
            .bind(EXPIRES)
            .execute(db.pool())
            .await
            .expect("historical registry inserts after rollback");
    }

    // -----------------------------------------------------------------------
    // Review pins: database half (disposable test database only)
    // -----------------------------------------------------------------------

    /// Malformed secret_hash fixtures (synthetic shape probes, never real
    /// secret material): short, non-hex, and uppercase.
    const BAD_HASH_SHORT: &str = "9f86d081";
    const BAD_HASH_NON_HEX: &str =
        "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg";
    const BAD_HASH_UPPER: &str = "4BC7CB80AB331AF138344F0E0508EA18D0E367B15E785FF40FA529DCA6467A56";
    /// Well-formed synthetic control: 64 lowercase hex chars.
    const GOOD_HASH_SYNTHETIC: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Literal INSERT shapes (sqlx 0.9 only accepts `&'static str`, so the two
    /// NULL-guild array shapes are literals, never interpolated): every
    /// timestamp bind carries `::timestamptz` like the rest of this module.
    // Used only when explicitly probing the immutable historical base DDL.
    const LEGACY_INSERT_PLAIN: &str = "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ($1, $2, '{read}', '{1}', 'h', 'o', $3::timestamptz, $4::timestamptz)";
    const INSERT_PLAIN: &str = "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', $1, $2, '{read}', '{1}', 'h', 'o', $3::timestamptz, $4::timestamptz)";
    const INSERT_NULL_GUILD: &str = "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', $1, $2, '{read}', ARRAY[NULL]::bigint[], 'h', 'o', $3::timestamptz, $4::timestamptz)";
    const INSERT_PARTIAL_NULL_GUILD: &str = "INSERT INTO admin_api_tokens
             (holder_id, holder_kind, token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ('fixture-holder', 'human', $1, $2, '{read}', ARRAY[1,NULL]::bigint[], 'h', 'o', $3::timestamptz, $4::timestamptz)";

    #[tokio::test]
    async fn secret_hash_shape_rejects_malformed_hashes() {
        let Some(db) = connect("hash-shape").await else {
            return;
        };
        assert_eq!(BAD_HASH_SHORT.len(), 8, "short fixture shape");
        assert_eq!(BAD_HASH_NON_HEX.len(), 64, "non-hex fixture shape");
        assert_eq!(BAD_HASH_UPPER.len(), 64, "uppercase fixture shape");
        assert!(
            GOOD_HASH_SYNTHETIC.len() == 64
                && GOOD_HASH_SYNTHETIC
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "positive control must be 64 lowercase hex"
        );
        for (token_id, hash) in [
            ("adm_2300000000000000", BAD_HASH_SHORT),
            ("adm_2300000000000001", BAD_HASH_NON_HEX),
            ("adm_2300000000000002", BAD_HASH_UPPER),
        ] {
            let err = sqlx::query(INSERT_PLAIN)
                .bind(token_id)
                .bind(hash)
                .bind(ISSUED)
                .bind(EXPIRES)
                .execute(db.pool())
                .await
                .expect_err("malformed secret_hash must be rejected");
            assert_sqlstate(&err, "23514", "secret_hash CHECK");
        }
        sqlx::query(INSERT_PLAIN)
            .bind("adm_2300000000000003")
            .bind(GOOD_HASH_SYNTHETIC)
            .bind(ISSUED)
            .bind(EXPIRES)
            .execute(db.pool())
            .await
            .expect("well-formed 64-lowercase-hex hash inserts");
    }

    #[tokio::test]
    async fn guild_allowlist_rejects_null_elements() {
        let Some(db) = connect("guild-nulls").await else {
            return;
        };
        for (token_id, stmt) in [
            ("adm_2400000000000000", INSERT_NULL_GUILD),
            ("adm_2400000000000001", INSERT_PARTIAL_NULL_GUILD),
        ] {
            let err = sqlx::query(stmt)
                .bind(token_id)
                .bind(TEST_HASH)
                .bind(ISSUED)
                .bind(EXPIRES)
                .execute(db.pool())
                .await
                .expect_err("NULL guild elements must be rejected");
            assert_sqlstate(&err, "23514", "guild NULL-element CHECK");
        }
        insert_live_token(db.pool(), "adm_2400000000000002").await;
    }

    #[tokio::test]
    async fn token_id_shape_rejects_malformed_ids() {
        let Some(db) = connect("token-id-shape").await else {
            return;
        };
        for token_id in [
            "adm_0123456789ABCDEF",
            "tok_0123456789abcdef",
            "adm_0123456789abcde",
        ] {
            let err = sqlx::query(INSERT_PLAIN)
                .bind(token_id)
                .bind(TEST_HASH)
                .bind(ISSUED)
                .bind(EXPIRES)
                .execute(db.pool())
                .await
                .expect_err("malformed token_id must be rejected");
            assert_sqlstate(&err, "23514", "token_id CHECK");
        }
        insert_live_token(db.pool(), TEST_TOKEN).await;
    }

    #[tokio::test]
    async fn store_migration_applies_standalone_with_same_guards() {
        // These local fixtures target ONLY shipped 0457, not the latest schema.
        const INSERT_PLAIN: &str = LEGACY_INSERT_PLAIN;
        const INSERT_NULL_GUILD: &str = "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ($1, $2, '{read}', ARRAY[NULL]::bigint[], 'h', 'o', $3::timestamptz, $4::timestamptz)";
        const INSERT_PARTIAL_NULL_GUILD: &str = "INSERT INTO admin_api_tokens
             (token_id, secret_hash, scopes, guild_ids, label, issued_by, issued_at, expires_at)
             VALUES ($1, $2, '{read}', ARRAY[1,NULL]::bigint[], 'h', 'o', $3::timestamptz, $4::timestamptz)";
        let Some(db) = connect("store-standalone").await else {
            return;
        };
        // Drop the cutover-built table, then apply ONLY the store file: the
        // store chain must guard the registry on its own.
        sqlx::query("DROP TABLE admin_api_tokens")
            .execute(db.pool())
            .await
            .expect("drop cutover-built table");
        let path = format!(
            "{}/../store/migrations/{}",
            env!("CARGO_MANIFEST_DIR"),
            super::STORE_MIGRATION
        );
        let sql = std::fs::read_to_string(&path).expect("store migration readable");
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(db.pool())
            .await
            .expect("store file applies standalone");
        // Same probes as the cutover chain: every malformed row must fail
        // 23514, each rejected by exactly one guard.
        for (token_id, hash) in [
            ("adm_2500000000000000", BAD_HASH_SHORT),
            ("adm_2500000000000001", BAD_HASH_NON_HEX),
            ("adm_2500000000000002", BAD_HASH_UPPER),
            ("adm_0123456789ABCDEF", TEST_HASH),
        ] {
            let err = sqlx::query(INSERT_PLAIN)
                .bind(token_id)
                .bind(hash)
                .bind(ISSUED)
                .bind(EXPIRES)
                .execute(db.pool())
                .await
                .expect_err("store build must reject the malformed row");
            assert_sqlstate(&err, "23514", "store CHECK");
        }
        for (token_id, stmt) in [
            ("adm_2500000000000003", INSERT_NULL_GUILD),
            ("adm_2500000000000004", INSERT_PARTIAL_NULL_GUILD),
        ] {
            let err = sqlx::query(stmt)
                .bind(token_id)
                .bind(TEST_HASH)
                .bind(ISSUED)
                .bind(EXPIRES)
                .execute(db.pool())
                .await
                .expect_err("store build must reject NULL guild elements");
            assert_sqlstate(&err, "23514", "store guild CHECK");
        }
        sqlx::query(INSERT_PLAIN)
            .bind("adm_2500000000000005")
            .bind(GOOD_HASH_SYNTHETIC)
            .bind(ISSUED)
            .bind(EXPIRES)
            .execute(db.pool())
            .await
            .expect("store build inserts a valid token");
        // The live-lookup index survives the standalone apply.
        let defs: Vec<String> = sqlx::query_scalar(
            "SELECT indexdef FROM pg_indexes WHERE tablename = 'admin_api_tokens'",
        )
        .fetch_all(db.pool())
        .await
        .expect("read store-built indexes");
        let ordered =
            defs.iter().any(
                |def| match (def.find("revoked_at"), def.find("expires_at")) {
                    (Some(revoked), Some(expires)) => revoked < expires,
                    _ => false,
                },
            );
        assert!(
            ordered,
            "store build needs an index on (revoked_at, expires_at) in that order, found {defs:?}"
        );
    }
}
