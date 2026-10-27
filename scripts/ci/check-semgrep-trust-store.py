#!/usr/bin/env python3
"""Reject the compromised emSign G3 anchor in the installed scanner's certifi.

Read-only/offline; defaults to certifi.where(), not the OS trust store. JSON
schema semgrep-trust-store.v1; exits 0 clean, 1 compromised, 2 unreadable/empty.
Use --bundle PATH to replay a PEM fixture without installing dependencies.
"""

import argparse
import hashlib
import json
import ssl

# SHA-256 of the DER certificate, not its mutable PEM comments or subject.
# GHSA-6q48-6c7w-9gw2: emSign ECC Root CA - G3, removed in certifi 2026.10.19.
COMPROMISED_SHA256 = "86a1ecba089c4a8d3bbe2734c612ba341d813e043cf9e8a862cd5c57a36bbe6b"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bundle", help="PEM bundle to check instead of installed certifi")
    args = parser.parse_args()
    report = {"schema": "semgrep-trust-store.v1", "retryable": False,
              "certificate_count": 0, "blocked_sha256": []}
    try:
        bundle = args.bundle
        if bundle is None:
            import certifi
            bundle = certifi.where()
        # An explicit context loads only this bundle, never default OS roots
        # or SSL_CERT_FILE/SSL_CERT_DIR. OpenSSL validates certificate encoding.
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.load_verify_locations(cafile=bundle)
        certificates = context.get_ca_certs(binary_form=True)
        if not certificates:
            raise ValueError("empty CA store")
    except (ImportError, OSError, ValueError):
        report.update(code="trust_store_unavailable",
                      next_action="Reinstall the hash-pinned scanner dependencies and check its nonempty certifi PEM bundle.")
        print(json.dumps(report, sort_keys=True))
        return 2
    report["certificate_count"] = len(certificates)
    report["blocked_sha256"] = sorted({hashlib.sha256(der).hexdigest() for der in certificates}
                                      & {COMPROMISED_SHA256})
    if report["blocked_sha256"]:
        report.update(code="compromised_trust_anchor",
                      next_action="Upgrade the hash-pinned certifi to 2026.10.19 or later; do not waive this check.")
        status = 1
    else:
        report.update(code="trust_store_clean", next_action="Continue the scanner gate.")
        status = 0
    print(json.dumps(report, sort_keys=True))
    return status


if __name__ == "__main__":
    raise SystemExit(main())
