"""Supplemental fixture on an isolated GitHub Actions PostgreSQL service."""

from pathlib import Path
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from staging_recovery_capture import decode_inventory

ROOT = Path(__file__).resolve().parents[2]
URL = "postgresql://agent_test:@agent-testdb:5432/two_bot_test_local"
COMMAND = ["psql", "-X", "-w", "-q", "-t", "--csv", "-v", "ON_ERROR_STOP=1", "-f", "-", URL]


def psql(query, *, success=True):
    result = subprocess.run(COMMAND, input=query, text=True, capture_output=True, timeout=40)
    if success and result.returncode:
        raise RuntimeError(result.stderr)
    if not success:
        assert result.returncode and not result.stdout, "misbound login returned an inventory"
    return result.stdout


psql("""
CREATE ROLE two_bot_migrator NOLOGIN;
CREATE ROLE two_bot_migrator_ro NOLOGIN;
CREATE ROLE two_bot_runtime NOLOGIN;
CREATE ROLE two_web_reader NOLOGIN;
CREATE ROLE fixture_parent NOLOGIN;
CREATE ROLE fixture_grandparent NOLOGIN;
CREATE ROLE fixture_outsider NOLOGIN;
CREATE ROLE fixture_unrelated NOLOGIN;
CREATE ROLE fixture_reader LOGIN IN ROLE two_bot_migrator_ro;
CREATE ROLE fixture_misbound LOGIN IN ROLE two_bot_migrator_ro, two_bot_migrator;
GRANT fixture_parent TO two_bot_runtime;
GRANT fixture_grandparent TO fixture_parent;
CREATE SCHEMA web_v1 AUTHORIZATION two_bot_migrator;
CREATE SCHEMA fixture_other AUTHORIZATION fixture_outsider;
ALTER DEFAULT PRIVILEGES FOR ROLE fixture_outsider IN SCHEMA public
    GRANT SELECT ON TABLES TO fixture_grandparent;
ALTER DEFAULT PRIVILEGES FOR ROLE fixture_outsider IN SCHEMA web_v1
    GRANT SELECT ON TABLES TO PUBLIC;
ALTER DEFAULT PRIVILEGES FOR ROLE fixture_outsider IN SCHEMA fixture_other
    GRANT SELECT ON TABLES TO fixture_grandparent;
ALTER DEFAULT PRIVILEGES FOR ROLE fixture_unrelated IN SCHEMA public
    GRANT SELECT ON TABLES TO fixture_unrelated;
CREATE EXTENSION citext;
CREATE FUNCTION public.fixture_identity(integer) RETURNS integer LANGUAGE SQL AS 'SELECT $1';
ALTER FUNCTION public.fixture_identity(integer) OWNER TO two_bot_migrator;
CREATE TABLE public.fixture_row (identifier bigint);
CREATE TABLE public.fixture_main (row_value public.fixture_row, label citext);
ALTER TABLE public.fixture_row OWNER TO two_bot_migrator;
ALTER TABLE public.fixture_main OWNER TO two_bot_migrator;
CREATE TYPE public.fixture_foreign AS (identifier integer);
ALTER TYPE public.fixture_foreign OWNER TO fixture_outsider;
CREATE DOMAIN public.fixture_nested AS public.fixture_foreign[];
ALTER DOMAIN public.fixture_nested OWNER TO fixture_outsider;
CREATE TYPE public.fixture_enum AS ENUM ('ready');
ALTER TYPE public.fixture_enum OWNER TO fixture_outsider;
CREATE DOMAIN public.fixture_scalar_domain AS integer;
ALTER DOMAIN public.fixture_scalar_domain OWNER TO fixture_outsider;
CREATE TYPE public.fixture_range AS RANGE (subtype = integer);
ALTER TYPE public.fixture_range OWNER TO fixture_outsider;
CREATE TABLE public.fixture_outside_row (identifier bigint);
ALTER TABLE public.fixture_outside_row OWNER TO fixture_outsider;
CREATE TABLE public._sqlx_migrations (
    version bigint, description text, success boolean, checksum bytea,
    installed_on timestamptz DEFAULT now(), execution_time bigint
);
CREATE TABLE public._two_bot_migrations (LIKE public._sqlx_migrations INCLUDING ALL);
INSERT INTO public._sqlx_migrations (version, description, success, checksum, execution_time)
    VALUES (480, 'cutover fixture', true, decode('abcd', 'hex'), 1);
INSERT INTO public._two_bot_migrations (version, description, success, checksum, execution_time)
    VALUES (478, 'store fixture', true, decode('abcd', 'hex'), 1);
ALTER TABLE public._sqlx_migrations OWNER TO two_bot_migrator;
ALTER TABLE public._two_bot_migrations OWNER TO two_bot_migrator;
GRANT USAGE ON SCHEMA web_v1 TO two_bot_migrator_ro;
GRANT SELECT ON public._sqlx_migrations, public._two_bot_migrations TO two_bot_migrator_ro;
""")

matrix = """
SELECT * FROM (VALUES ('public', 'fixture_main', 'table'),
    ('public', 'fixture_row', 'table'), ('public', 'fixture_identity(integer)', 'function'),
    ('public', '_sqlx_migrations', 'ledger'),
    ('public', '_two_bot_migrations', 'ledger')) objects(schema_name, name, kind)
"""
query = (ROOT / "scripts/staging_recovery_inventory.sql").read_text().replace("-- @matrix", matrix)


def inventory():
    return decode_inventory(psql("SET SESSION AUTHORIZATION fixture_reader;\n" + query))


baseline = inventory()
assert baseline["role"] == "two_bot_migrator_ro"
assert not baseline["unsupported"] and not baseline["missing_expected"]
assert baseline["cutover_ledger"][0]["version"] == 480
assert baseline["store_ledger"][0]["version"] == 478
assert {row["schema"] for row in baseline["default_acls"] if row["owner"] == "fixture_outsider"} == {
    "public", "web_v1",
}
assert all(row["owner"] != "fixture_unrelated" for row in baseline["default_acls"])
assert {"fixture_outsider", "fixture_parent", "fixture_grandparent"} <= {r["name"] for r in baseline["roles"]}
for type_name in ("fixture_foreign", "fixture_nested", "fixture_enum", "fixture_scalar_domain",
                  "fixture_range", "fixture_outside_row"):
    psql(f"ALTER TABLE public.fixture_main ADD COLUMN bad public.{type_name};")
    assert inventory()["unsupported"], f"uncaptured dependency {type_name} was accepted"
    psql("ALTER TABLE public.fixture_main DROP COLUMN bad;")
psql("""
CREATE FUNCTION public.fixture_enum_result() RETURNS public.fixture_enum LANGUAGE SQL
    AS 'SELECT NULL::public.fixture_enum';
ALTER FUNCTION public.fixture_enum_result() OWNER TO two_bot_migrator;
""")
assert inventory()["unsupported"], "uncaptured routine return type was accepted"
psql("DROP FUNCTION public.fixture_enum_result();")
psql("SET SESSION AUTHORIZATION fixture_misbound;\n" + query, success=False)
print("PASS: real read-only catalog query, ordinary in-scope row and extension types, direct and nested composites, enum, domain, range, outside row and routine refusal, inherited default ACLs and misbound refusal")
