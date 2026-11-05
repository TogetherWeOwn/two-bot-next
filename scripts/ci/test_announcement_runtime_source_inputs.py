"""Restricted source-input adversaries; never actual stored feed or send proof."""

import copy
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import at, evidence_index
from announcement_source_input_fixtures import (
    source_input_fixture, repin_source_inputs, replace_source_body, sha,
)
from announcement_runtime_authority import check_authority_graph_claims
from announcement_runtime_evidence import SOURCE_RECORDS, canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_provenance import check_provenance_claims
from announcement_runtime_source_inputs import check_source_input_claims
from announcement_runtime_schema import Refusal


def data(records, ref="runtime-source-plan"):
    return records[ref]["data"]


class SourceInputs(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_source_input_claims(receipt, evidence_index(records), now)

    def reject(self, mutate, code=None, *, repin=True):
        receipt, records, now = source_input_fixture()
        mutate(receipt, records)
        if repin:
            repin_source_inputs(receipt, records)
        with self.assertRaisesRegex(Refusal, code or "announcement evidence refused"):
            self.check(receipt, records, now)

    def test_consistent_source_claims_compose_but_cannot_admit(self):
        receipt, records, now = source_input_fixture()
        index = evidence_index(records)
        self.assertIsNone(check_source_input_claims(receipt, index, now))
        self.assertIsNone(check_authority_graph_claims(receipt, index, now))
        self.assertIsNone(check_provenance_claims(receipt, index, now))
        index.require_all_used()
        with self.assertRaisesRegex(Refusal, "unsupported"):
            runtime_preflight()

    def test_source_kinds_are_closed_and_required(self):
        _, records, _ = source_input_fixture()
        examples = {r["kind"]: ref for ref, r in records.items() if r["kind"] in SOURCE_RECORDS}
        self.assertEqual(set(examples), set(SOURCE_RECORDS))
        for ref in examples.values():
            for value in (None, {}, {**data(records, ref), "extra": True}):
                with self.subTest(ref=ref, value=type(value)):
                    self.reject(lambda r, s: s[ref].update(data=value), repin=False)

    def test_source_objects_cannot_be_missing_mistyped_or_mixed_attempt(self):
        for ref in ("runtime-source-plan", "stored-source", "source-body", "selected-item"):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: s.pop(ref), "unresolved_reference", repin=False)
                self.reject(lambda r, s: s[ref].update(kind="document"), repin=False)
                self.reject(lambda r, s: s[ref]["run"].update(attempt=2),
                            "evidence_attempt", repin=False)

    def test_source_plan_and_every_inner_record_are_content_bound(self):
        for ref, changes in (("runtime-source-plan", {"maximum_response_bytes": 4000}),
                             ("stored-source", {"channel_id": "999"}),
                             ("source-body", {"body_utf8": "replaced"}),
                             ("selected-item", {"nonce": "a" * 24})):
            with self.subTest(ref=ref):
                self.reject(lambda r, s: data(s, ref).update(changes), "source_content", repin=False)

    def test_candidate_and_source_fixture_must_match(self):
        for changes in ({"candidate": {"commit": "a" * 40, "tree": "b" * 40}},
                        {"feed_ref": "rsvp"}, {"source_ref": "other-source"},
                        {"item_ref": "other-item"}):
            self.reject(lambda r, s: data(s).update(changes), "source_fixture")

    def test_noncanonical_or_ambiguous_targets_refuse(self):
        for origin in ("http://fixture.example.test", "https://user:password@fixture.example.test",
                       "https://fixture.example.test:443", "https://fixture.example.test.",
                       "https://127.0.0.1", "https://localhost", "https://fixture..test",
                       "https://-fixture.test", "https://fixture-.test", "https://UPPER.test",
                       "https://fixture.test/alternate", "https://xn--.test",
                       "https://" + "a" * 64 + ".test"):
            with self.subTest(origin=origin):
                self.reject(lambda r, s: data(s)["target"].update(origin=origin))
        for path in ("//other.test/path", "/../feed", "/a/./feed", "/a//feed",
                     "/feed?token=secret", "/feed#fragment", "/%2e/feed", "/a\\b"):
            with self.subTest(path=path):
                self.reject(lambda r, s: data(s)["target"].update(path=path))

    def test_invalid_idna_item_url_refuses_after_full_repin(self):
        receipt, records, now = source_input_fixture()
        selected = data(records, "selected-item")
        selected["item"]["url"] = "https://xn--0.test/item"
        digest = canonical_digest(selected["item"])
        receipt["fixtures"]["item_content_sha256"] = digest
        data(records, "item-ref")["item_content_sha256"] = digest
        selected["post_content_sha256"] = sha("New feed item: **Fixture item**\n" +
                                             selected["item"]["url"])
        repin_source_inputs(receipt, records)
        with self.assertRaisesRegex(Refusal, "source_target$"):
            self.check(receipt, records, now)

    def test_idna_source_hosts_are_not_in_the_supported_profile(self):
        for host in ("xn--0.test", "xn--bcher-kva.test", "fixture.xn--0"):
            self.reject(lambda r, s: data(s)["target"].update(origin="https://" + host),
                        "source_target$")

    def test_target_digest_is_not_arbitrary_equal_hashes(self):
        for field in ("origin_identity_sha256", "path_sha256"):
            def mutate(r, s):
                entry = next(x for x in r["resources"]["entries"] if x["ref"] == "source")
                entry["identity"][field] = "f" * 64
                binding = next(x for x in data(s, "runtime-identity-map")["bindings"]
                               if x["resource_ref"] == "source")
                binding["identity"] = copy.deepcopy(entry["identity"])
                for ref in ("before-source", "after-source", "isolation-source"):
                    data(s, ref)["identity_sha256"] = canonical_digest(entry["identity"])
            with self.subTest(field=field):
                self.reject(mutate, "source_target_binding")

    def test_stored_row_keys_destinations_and_source_are_exact(self):
        for changes in ({"feed_ref": "other-feed"}, {"key_ref": "key-rsvp"},
                        {"guild_id": "999"}, {"channel_id": "999"},
                        {"source_url": "https://fixture.example.test/other.xml"}):
            self.reject(lambda r, s: data(s, "stored-source").update(changes), "stored_source")
        self.reject(lambda r, s: data(s, "stored-source").update(enabled=False))
        for kind in ("twitch", "youtube"):
            self.reject(lambda r, s: data(s, "stored-source").update(kind=kind),
                        "unsupported_source_profile")

    def test_row_observation_is_before_rollout_and_uses_served_before(self):
        for clock in ("2025-12-31T23:59:59Z", at("00:03:01")):
            self.reject(lambda r, s: data(s, "stored-source").update(observed_at=clock),
                        "stored_source_time")
        self.reject(lambda r, s: data(s, "stored-source").update(served_ref="served-one"),
                    "stored_source_served")

    def test_row_observer_uses_independent_same_attempt_job_and_producer(self):
        self.reject(lambda r, s: data(s, "stored-source").update(observer_ref="observer-ref"),
                    "source_observer")
        # Each changed job/producer is source-only, so resource baselines remain
        # coherent and this adversary reaches the new source observer predicate.
        for changes in ({"source": "candidate"}, {"conclusion": "failure"},
                        {"head": "f" * 40}, {"started_at": at("00:02:01")},
                        {"ended_at": at("00:01:59")}, {"producer_ref": "producer-ref"}):
            def mutate(r, s):
                s["source-observer"] = copy.deepcopy(s["resource-observer"])
                s["source-observer"]["ref"] = "source-observer"
                data(s, "stored-source")["observer_ref"] = "source-observer"
                data(s, "source-observer")["job_ref"] = "source-job"
                s["source-job"] = copy.deepcopy(s["job-resource-observer"])
                s["source-job"]["ref"] = "source-job"
                data(s, "source-job").update(changes)
                r["attempt"]["job_refs"].append("source-job")
            with self.subTest(changes=changes):
                self.reject(mutate, "source_observer")

    def test_body_hash_size_and_target_are_compared_to_actual_utf8(self):
        for changes in ({"body_utf8": "replaced"}, {"sha256": "a" * 64},
                        {"byte_count": 1}, {"source_ref": "other-source"},
                        {"target": {"origin": "https://other.example.test", "path": "/feed.xml"}}):
            self.reject(lambda r, s: data(s, "source-body").update(changes), "source_body")
        self.reject(lambda r, s: data(s).update(maximum_response_bytes=1), "source_body")
        self.reject(lambda r, s: data(s, "source-body").update(body_utf8="é" * 2049))
        self.reject(lambda r, s: data(s).update(maximum_response_bytes=4097))
        self.reject(lambda r, s: data(s).update(redirects="allow"))
        self.reject(lambda r, s: data(s).update(content_encoding="gzip"))

    def test_utf8_byte_count_is_not_character_count(self):
        def mutate(r, s):
            body = data(s, "source-body")
            body.update(body_utf8="é", byte_count=1, sha256=sha("é"))
            r["fixtures"]["source_content_sha256"] = sha("é")
            data(s, "item-ref")["source_content_sha256"] = sha("é")
            entry = next(x for x in r["resources"]["entries"] if x["ref"] == "source")
            entry["identity"]["content_sha256"] = sha("é")
            binding = next(x for x in data(s, "runtime-identity-map")["bindings"]
                           if x["resource_ref"] == "source")
            binding["identity"] = copy.deepcopy(entry["identity"])
            for ref in ("before-source", "after-source", "isolation-source"):
                data(s, ref)["identity_sha256"] = canonical_digest(entry["identity"])
        self.reject(mutate, "source_body")

    def test_selection_and_nonce_reference_bind_exact_item(self):
        for changes in ({"source_ref": "other-source"}, {"item_ref": "other-item"},
                        {"body_sha256": "f" * 64}, {"nonce_ref": "one-shot-nonce"}):
            self.reject(lambda r, s: data(s, "selected-item").update(changes), "source_selection")
        self.reject(lambda r, s: data(s, "selected-item")["item"].update(key="other-key"),
                    "source_selection")

    def test_nonce_uses_derived_item_hash_not_raw_key(self):
        self.reject(lambda r, s: data(s, "selected-item").update(
            nonce=sha("fixture-relay\0" + data(s, "selected-item")["item"]["key"])[:24]),
            "source_nonce")
        self.reject(lambda r, s: data(s, "selected-item").update(nonce="a" * 24), "source_nonce")
        self.reject(lambda r, s: data(s, "selected-item").update(post_content_sha256="a" * 64),
                    "source_post")

    def test_unicode_mentions_markdown_and_empty_key_are_unsupported(self):
        for field, value in (("key", ""), ("key", " key "), ("key", "☃"),
                             ("title", "@everyone"), ("title", "*hidden*"),
                             ("url", "https://user:secret@fixture.test/item"),
                             ("url", "https://fixture.test/item?secret=value")):
            with self.subTest(field=field):
                self.reject(lambda r, s: data(s, "selected-item")["item"].update({field: value}))

    def test_title_hyphen_is_escaped_like_candidate_not_plain_markdown(self):
        receipt, records, now = source_input_fixture()
        selected = data(records, "selected-item")
        selected["item"]["title"] = "Fixture-item"
        digest = canonical_digest(selected["item"])
        receipt["fixtures"]["item_content_sha256"] = digest
        data(records, "item-ref")["item_content_sha256"] = digest
        selected["post_content_sha256"] = sha("New feed item: **Fixture\\-item**\n" +
                                             selected["item"]["url"])
        replace_source_body(receipt, records, data(records, "source-body")["body_utf8"].replace(
            "<title>Fixture item</title>", "<title>Fixture-item</title>"))
        self.assertIsNone(self.check(receipt, records, now))
        selected["post_content_sha256"] = sha("New feed item: **Fixture-item**\n" +
                                             selected["item"]["url"])
        repin_source_inputs(receipt, records)
        with self.assertRaisesRegex(Refusal, "source_post"):
            self.check(receipt, records, now)

    def test_source_wire_budget_is_finite_and_exact(self):
        self.reject(lambda r, s: data(s).update(maximum_wire_attempts=2), "source_budget")
        for value in (0, 513, True):
            self.reject(lambda r, s: data(s).update(maximum_wire_attempts=value))

    def test_private_urls_and_bodies_are_not_echoed_in_refusals(self):
        receipt, records, now = source_input_fixture()
        data(records, "stored-source")["source_url"] = "https://secret.example.test/private"
        repin_source_inputs(receipt, records)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertNotIn("secret", str(caught.exception))
        self.assertNotIn("private", str(caught.exception))


if __name__ == "__main__":
    unittest.main()
