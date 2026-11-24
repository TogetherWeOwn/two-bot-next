#!/usr/bin/env python3
"""Encrypt a read-only staging schema, ACL and migration-ledger inventory."""

import base64
import csv
import hashlib
import json
import os
from pathlib import Path
import ssl
import subprocess
import sys
import tempfile

from staging_retention_census import (
    BRANCH_ENV,
    DB_ROLE,
    ReadFailed,
    Refused,
    csv_rows,
    database_target,
    psql_environment,
    run_psql,
)

ROOT = Path(__file__).resolve().parents[1]
CAPTURE = Path("staging-recovery.cms")
MAX_CAPTURE_BYTES = 4 * 1024 * 1024
RECIPIENT_SHA256 = "278940b22ac30d798774250303002b4f3e79dd75ff171efa02b738babcf7c959"


def encrypt(payload, certificate):
    with tempfile.TemporaryDirectory() as directory:
        recipient = Path(directory) / "recipient.pem"
        recipient.write_text(certificate, encoding="ascii")
        return subprocess.run(
            ["openssl", "cms", "-encrypt", "-binary", "-outform", "DER",
             "-aes-256-gcm", "-recip", str(recipient),
             "-keyopt", "rsa_padding_mode:oaep", "-keyopt", "rsa_oaep_md:sha256"],
            input=payload, capture_output=True, timeout=10, check=True, env={"PATH": os.defpath},
        ).stdout


def main():
    try:
        public_certificate = os.environ.get("RECOVERY_RECIPIENT", "")
        if not 0 < len(public_certificate) <= 4096:
            raise Refused("a bounded public recipient certificate is required")
        der_certificate = base64.b64decode(public_certificate, validate=True)
        if hashlib.sha256(der_certificate).hexdigest() != RECIPIENT_SHA256:
            raise Refused("the recipient does not match the reviewed certificate")
        pg, host, database = database_target(os.environ)
        query = (ROOT / "scripts/staging_recovery_inventory.sql").read_text(encoding="utf-8")
        matrix = (ROOT / "sql/database_role_matrix.sql").read_text(encoding="utf-8")
        output = run_psql("psql", psql_environment(pg, os.environ),
                          query.replace("-- @matrix", matrix))
        report = decode_inventory(output)
        if (report["database"]["name"] != database or report["role"] != DB_ROLE
                or report["missing_expected"] or report["unsupported"]
                or not (report["cutover_ledger"] and report["store_ledger"])):
            raise ReadFailed("catalog inventory could not cover the reviewed scope")
        report["target"] = {"host": host, "database": database,
                            "branch_id": os.environ.get(BRANCH_ENV, "").strip()}
        report["workflow_sha"] = os.environ["GITHUB_SHA"]
        payload = (json.dumps(report, sort_keys=True, indent=2) + "\n").encode()
        if len(payload) > MAX_CAPTURE_BYTES:
            raise ReadFailed("catalog inventory exceeded the output limit")

        encrypted = encrypt(payload, ssl.DER_cert_to_PEM_cert(der_certificate))
        with CAPTURE.open("xb") as artifact:
            artifact.write(encrypted)
    except (Refused, ReadFailed) as error:
        # Both classes only contain fixed messages, never psql diagnostics.
        print(str(error), file=sys.stderr)
        return 1
    except (KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError):
        print("staging recovery capture failed", file=sys.stderr)
        return 1

    print(f"Plaintext SHA-256: {hashlib.sha256(payload).hexdigest()}")
    return 0


def decode_inventory(output):
    if len(output.encode()) > MAX_CAPTURE_BYTES:
        raise ReadFailed("catalog inventory exceeded the output limit")
    csv.field_size_limit(MAX_CAPTURE_BYTES)
    [[record]] = csv_rows(output)
    return json.loads(record)


if __name__ == "__main__":
    sys.exit(main())
