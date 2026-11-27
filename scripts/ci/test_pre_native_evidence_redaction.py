"""Offline checks for the default-off PRE artifact redaction contract."""

from dataclasses import FrozenInstanceError, replace
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
from uuid import UUID

with patch.object(sys, "path", [str(Path(__file__).resolve().parents[1]), *sys.path]):
    from pre_native_evidence_redaction import (
        MAX_BYTES, MAX_DEPTH, MAX_ITEMS, ArtifactMetadata, EvidencePin, PromotionReadback,
        RedactionVerdict, verify_native_pre_evidence_redaction as verify,
    )
    from pre_target import Target


TARGET = Target("pre-fixture-redaction", 203, 1_800_000_000, "0473/0465")
PIN = EvidencePin(TARGET, UUID(int=1), UUID(int=2), "a" * 40)
NOW = TARGET.started_at + 120
READBACK = PromotionReadback(NOW, True)
CANARY = "synthetic-value-keep-out-of-diagnostics"


class Unreadable:
    def __getattribute__(self, name):
        raise AssertionError("input was accessed")

    __bool__ = __repr__ = lambda self: self.unreadable


class PreNativeEvidenceRedactionTests(unittest.TestCase):
    def check_artifact(self, payload=b"{}", **changes):
        artifact = ArtifactMetadata(PIN.reservation_id, TARGET.run_id, PIN.deployment_id, PIN.sha, len(payload))
        options = dict(pin=PIN, artifact=artifact, payload=payload, promotion_readback=READBACK,
                       now=NOW, enabled=True)
        return verify(**(options | changes))

    def check_value(self, value, **changes):
        return self.check_artifact(json.dumps(value).encode("ascii"), **changes)

    def test_default_off_returns_one_frozen_verdict_without_touching_inputs(self):
        disabled = verify()
        self.assertEqual(disabled, RedactionVerdict("pre_native_redaction_disabled"))
        unreadable = Unreadable()
        for enabled in (False, None, 1, "true", unreadable):
            self.assertIs(verify(unreadable, artifact=unreadable, payload=unreadable,
                                 promotion_readback=unreadable, now=unreadable, max_bytes=unreadable,
                                 max_depth=unreadable, max_items=unreadable, enabled=enabled), disabled)
        with self.assertRaises(FrozenInstanceError):
            disabled.code = "changed"

    def test_clean_artifact_at_size_depth_and_item_limits(self):
        payload = b'{"endpoint":["http://127.0.0.2:8080","http://[::1]:8080","::ffff:127.0.0.1",true]}'
        self.assertEqual(self.check_artifact(payload, max_bytes=len(payload), max_depth=2, max_items=7),
                         RedactionVerdict("pre_native_evidence_redacted", "$", 7))
        self.assertEqual(self.check_artifact(b' {\n "time": "12:34:56" \n}').code,
                         "pre_native_evidence_redacted")

    def test_accepts_the_pinned_source_sha_and_rust_path(self):
        self.assertEqual(self.check_value({"source_sha": PIN.sha, "source": "two_bot::gateway"}).code,
                         "pre_native_evidence_redacted")
        self.assertEqual(self.check_value({"source_sha": "b" * 40}).code,
                         "pre_native_evidence_provider_token")

    def test_pin_binds_each_artifact_identity_and_suppresses_repr(self):
        artifact = ArtifactMetadata(PIN.reservation_id, TARGET.run_id, PIN.deployment_id, PIN.sha, 2)
        for changes in (dict(reservation_id=UUID(int=0)), dict(reservation_id=UUID(int=3)),
                        dict(run_id="203"), dict(run_id=204), dict(deployment_id=UUID(int=3)),
                        dict(sha=CANARY)):
            wrong = replace(artifact, **changes)
            self.assertEqual(self.check_artifact(artifact=wrong).code, "wrong_pre_native_evidence_target")
            self.assertNotIn(CANARY, repr(wrong))
        for pin in (None, TARGET, object.__new__(EvidencePin), replace(PIN, deployment_id=UUID(int=0)),
                    replace(PIN, sha=CANARY)):
            self.assertEqual(self.check_artifact(pin=pin).code, "invalid_pre_native_evidence_pin")
            self.assertNotIn(CANARY, repr(pin))

    def test_requires_literal_true_fresh_readback_on_the_pinned_clock(self):
        for readback in (None, object.__new__(PromotionReadback), replace(READBACK, production_auto_promote_disabled="true"),
                         replace(READBACK, production_auto_promote_disabled=False),
                         replace(READBACK, production_auto_promote_disabled=1),
                         PromotionReadback(NOW - 61, True), PromotionReadback(NOW + 1, True),
                         PromotionReadback(str(NOW), True)):
            self.assertEqual(self.check_artifact(promotion_readback=readback).code,
                             "pre_native_promotion_readback_unconfirmed")
        self.assertEqual(self.check_artifact(promotion_readback=PromotionReadback(NOW - 60, True)).code,
                         "pre_native_evidence_redacted")
        self.assertEqual(self.check_artifact(now=TARGET.started_at,
                         promotion_readback=PromotionReadback(TARGET.started_at - 1, True)).code,
                         "pre_native_promotion_readback_unconfirmed")
        post_cleanup = TARGET.expires_at + 1
        self.assertEqual(self.check_artifact(now=post_cleanup,
                         promotion_readback=PromotionReadback(post_cleanup, True)).code,
                         "pre_native_evidence_redacted")
        for now in (True, TARGET.started_at - 1, 10_000_000_000):
            self.assertEqual(self.check_artifact(now=now).code, "invalid_pre_clock")

    def test_limits_refuse_oversized_mismatched_or_overcomplex_inputs(self):
        for options in (dict(max_bytes=True), dict(max_bytes=MAX_BYTES + 1),
                        dict(max_depth=MAX_DEPTH + 1), dict(max_items=MAX_ITEMS + 1),
                        dict(max_items=0)):
            self.assertEqual(self.check_artifact(**options).code, "invalid_pre_native_evidence_limits")
        for payload, options in ((b"", {}), (b"{}", dict(max_bytes=1)), (bytearray(b"{}"), {}),
                                 (b"{}", dict(artifact=ArtifactMetadata(
                                     PIN.reservation_id, TARGET.run_id, PIN.deployment_id, PIN.sha, 1)))):
            self.assertEqual(self.check_artifact(payload, **options).code, "invalid_pre_native_evidence_size")
        self.assertEqual(self.check_value([[[]]], max_depth=2).code, "pre_native_evidence_depth_limit")
        self.assertEqual(self.check_value({"safe": [1, 2]}, max_items=4),
                         RedactionVerdict("pre_native_evidence_item_limit", "$.value[0][1]", 4))
        self.assertEqual(self.check_value({"safe": 1}, max_items=1),
                         RedactionVerdict("pre_native_evidence_item_limit", "$.key[0]", 1))

    def test_forbidden_keys_are_checked_recursively_and_never_echoed(self):
        for key in ("authorization", "Token", "SECRET", "password", "api_key", "DSN", "cookie",
                    "privateKey", "bearer", "webhook-url", "ａｐｉ＿ｋｅｙ", "client_secret", "DISCORD_TOKEN",
                    "database_password", "access_token", "Set-Cookie", "x-api-key", "database_url",
                    "discordToken", "passwd", "webhook", "clientCredential"):
            result = self.check_value({CANARY: [{"safe": True, key: CANARY}]})
            self.assertEqual(result, RedactionVerdict("pre_native_evidence_secret_key", "$.value[0][0].key[1]", 7))
            self.assertNotIn(CANARY, str(result))
            self.assertNotIn(CANARY, repr(result))

    def test_secret_and_address_patterns_are_checked_in_names_and_values(self):
        samples = {
            "url_credentials": [f"https://fixture:{CANARY}@example.invalid/path",
                                f"https://{CANARY}@example.invalid/path",
                                "https://fixture%2F:opaque@example.invalid",
                                "https://example.invalid/?format=json&sig=opaque",
                                "https://example.invalid/?%74oken=opaque"],
            "private_key": ["-----BEGIN OPENSSH " + "PRIVATE KEY-----"],
            "provider_token": ["ghp_" + "A" * 36, "github_pat_" + "b" * 64, "mfa." + "C" * 84,
                               ".".join(("D" * 24, "E" * 6, "F" * 27)), "cfut_" + "G" * 40,
                               "Q" * 39 + "_", "b" * 37, "v1.0-" + "b" * 64,
                               "Bearer sk-abcdef", "sk-ant-api03-" + "H" * 12],
            "nonloopback_ip": ["ip=192.0.2.8:8080", "172.016.0.2", "https://[2001:db8::1]:443/",
                               "fd00::8%device", "::ffff:192.0.2.8", "2001:0db8:0:0:0:0:0:8",
                               "0.0.0.0", "::", "300.192.0.2.8", "127.10.0.0.5",
                               "http://3232235521/", "http://0xc0a80001/", "http://[%3A%3Af00d]/"],
            "control_byte": ["\x00", "\n", "\t", "\x1f", "\x7f", "\x85", "\u202e", "\ud800"],
        }
        for code, values in samples.items():
            for value in values:
                name = f"{CANARY} {value}"
                for payload, path in (({name: True}, "$.key[0]"), ([name], "$[0]")):
                    result = self.check_value(payload)
                    self.assertEqual(result, RedactionVerdict(f"pre_native_evidence_{code}", path, 2))
                    self.assertNotIn(CANARY, repr(result))

    def test_malformed_json_and_unsafe_shapes_fail_closed_without_echo(self):
        for payload in (b"\xff", b'{"x":"\x00"}', b'{"x":1,"x":2}', b"[NaN]", b"[1e999]",
                        f'["{CANARY}",]'.encode("ascii")):
            result = self.check_artifact(payload)
            self.assertEqual(result, RedactionVerdict("invalid_pre_native_evidence_json"))
            self.assertNotIn(CANARY, repr(result))
