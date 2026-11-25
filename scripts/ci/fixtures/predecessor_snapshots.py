"""Disposable PRE schema seeds from the pinned migration prefixes."""

import hashlib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
# Same SQLx projections and row counts as the landed predecessor_plan.rs.
PROJECTIONS = {
    473: (115, "186ebda9ebbc1beab29be4c9e7298fb05e83c9f84460c514707ddaa2f1ad4724"),
    475: (116, "bd463cd55505f968149f2dd65a845fbba3ebd2efbaecabf9aa4d4b9ea2b34754"),
    465: (20, "746e2a68cf83ccd51945aa8b3e1d7b06d96bf423ba027113a23db2e8888fb7b7"),
}
GUILD = "222200000000000001"
SEED = """INSERT INTO event_instance_windows
(guild_id,event_id,instance_starts_at,starts_at,status,observed_at)
VALUES ('222200000000000001','200000000000000001','2000-01-01','2000-01-01',
        'completed','2000-01-01T00:00:00Z');
"""



def _require(condition, message):
    if not condition:
        raise ValueError(message)


def chain_sql(chain, table, tip):
    count, projection = PROJECTIONS[tip]
    names = sorted((ROOT / "crates" / chain / "migrations").glob("*.sql"))
    rows, commands = [], []
    for path in names:
        version, name = path.stem.split("_", 1)
        if int(version) > tip:
            continue
        sql = path.read_bytes()
        checksum = hashlib.sha384(sql).hexdigest()
        description = name.replace("_", " ")
        rows.append(f"{int(version)}:{description}:{checksum}\n")
        if int(version) == 475:
            commands.append(SEED)
        commands.append(f"BEGIN;\n{sql.decode()}\n"
                        f"INSERT INTO {table} VALUES ({int(version)}, '{description}', "
                        f"now(), true, decode('{checksum}', 'hex'), 0);\nCOMMIT;\n")
    _require(len(rows) == count and rows[-1].startswith(f"{tip}:")
            and hashlib.sha256(("synthetic-pre-inventory\n" + "".join(rows)).encode()).hexdigest()
            == projection, f"PRE migration projection mismatch at {tip}")
    return (f"CREATE TABLE {table} (version bigint PRIMARY KEY, description text NOT NULL, "
            "installed_on timestamptz NOT NULL, success bool NOT NULL, checksum bytea NOT NULL, "
            "execution_time bigint NOT NULL);\n" + "".join(commands))


def snapshot_sql(tip):
    contract, marker, _ = (ROOT / "sql/web_v1.sql").read_text().partition("-- Voice heat contract 1.1:")
    _require(marker, "missing Web contract 1.0 boundary")
    return (chain_sql("cutover", "_sqlx_migrations", tip)
            + (SEED if tip == 473 else "")
            + chain_sql("store", "_two_bot_migrations", 465) + contract + """
CREATE ROLE offline_bot LOGIN PASSWORD 'offline-fixture';
GRANT USAGE ON SCHEMA public, web_v1 TO offline_bot;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO offline_bot;
GRANT SELECT ON ALL TABLES IN SCHEMA web_v1 TO offline_bot;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO offline_bot;
REVOKE INSERT, UPDATE, DELETE ON _sqlx_migrations, _two_bot_migrations FROM offline_bot;
""")


def schema_check(tip):
    fence = ("attendance_recovered_at = (SELECT installed_on FROM _sqlx_migrations "
             "WHERE version = 475)" if tip == 475 else "NOT EXISTS (SELECT FROM "
             "information_schema.columns WHERE table_schema = 'public' "
             "AND table_name = 'event_instance_windows' AND column_name = 'attendance_recovered_at')")
    return f"""SELECT
        (SELECT count(*) = {PROJECTIONS[tip][0]} AND max(version) = {tip} AND bool_and(success)
           FROM _sqlx_migrations)
        AND (SELECT count(*) = 20 AND max(version) = 465 AND bool_and(success)
           FROM _two_bot_migrations)
        AND (SELECT contract_version = '1.0' FROM web_v1.contract_meta)
        AND (SELECT {fence} FROM event_instance_windows WHERE guild_id = '{GUILD}');"""
