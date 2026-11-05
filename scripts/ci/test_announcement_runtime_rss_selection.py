"""Inert RSS grammar/selection adversaries, not authenticated source observations."""

import copy
import json
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from announcement_provenance_fixtures import evidence_index
from announcement_source_input_fixtures import (
    replace_source_body, repin_source_inputs, sha, source_input_fixture,
)
from announcement_runtime_evidence import canonical_digest
from announcement_runtime_policy import runtime_preflight
from announcement_runtime_schema import Refusal
from announcement_runtime_source_inputs import check_source_input_claims


class RssSelection(unittest.TestCase):
    def check(self, receipt, records, now):
        return check_source_input_claims(receipt, evidence_index(records), now)

    def reject_body(self, change, code="unsupported_rss_fixture"):
        receipt, records, now = source_input_fixture()
        body = records["source-body"]["data"]["body_utf8"]
        changed = change(body)
        self.assertNotEqual(body, changed, "adversary must change actual source bytes")
        replace_source_body(receipt, records, changed)
        with self.assertRaisesRegex(Refusal, code + "$"):
            self.check(receipt, records, now)

    def test_same_corpus_is_consumed_by_shipped_rust_parser_and_offline_checker(self):
        corpus = json.loads((Path(__file__).parent / "fixtures/announcement_rss_profile.json").read_text())
        self.assertEqual(len(corpus["cases"]), 3)
        for case in corpus["cases"]:
            with self.subTest(case=case["name"]):
                receipt, records, now = source_input_fixture()
                selected = records["selected-item"]["data"]
                selected.update(item=copy.deepcopy(case["item"]), nonce=case["nonce"],
                                post_content_sha256=sha(case["post_content"]))
                digest = canonical_digest(case["item"])
                receipt["fixtures"]["item_content_sha256"] = digest
                records["item-ref"]["data"]["item_content_sha256"] = digest
                records["runtime-input-plan"]["data"]["feed"]["item_key"] = case["item_key"]
                records["key-delivery"]["data"]["item_key"] = case["item_key"]
                delivery = next(x for x in receipt["resources"]["entries"] if x["ref"] == "delivery")
                delivery["identity"]["key_sha256"] = canonical_digest(records["key-delivery"]["data"])
                binding = next(x for x in records["runtime-identity-map"]["data"]["bindings"]
                               if x["resource_ref"] == "delivery")
                binding["identity"] = copy.deepcopy(delivery["identity"])
                for ref in ("before-delivery", "after-delivery", "isolation-delivery"):
                    records[ref]["data"]["identity_sha256"] = canonical_digest(delivery["identity"])
                replace_source_body(receipt, records, case["body"])
                self.assertIsNone(self.check(receipt, records, now))
                with self.assertRaisesRegex(Refusal, "unsupported$"):
                    runtime_preflight()

    def test_selected_guid_must_be_present_even_after_every_body_hash_is_rebound(self):
        self.reject_body(lambda x: x.replace("<guid>fixture-item-one</guid>",
                                           "<guid>different-item</guid>"), "source_parser_selection")

    def test_selected_title_url_and_publication_must_equal_body_fields(self):
        for old, new in (("<title>Fixture item</title>", "<title>Other title</title>"),
                         ("/item</link>", "/other</link>"),
                         ("2026-01-01T00:00:00Z", "2026-01-01T00:00:01Z")):
            with self.subTest(field=old):
                self.reject_body(lambda x: x.replace(old, new), "source_parser_selection")

    def test_rehashed_selected_title_cannot_disagree_with_unchanged_body(self):
        receipt, records, now = source_input_fixture()
        selected = records["selected-item"]["data"]
        selected["item"]["title"] = "Fabricated title"
        digest = canonical_digest(selected["item"])
        receipt["fixtures"]["item_content_sha256"] = digest
        records["item-ref"]["data"]["item_content_sha256"] = digest
        selected["post_content_sha256"] = sha("New feed item: **Fabricated title**\n" +
                                             selected["item"]["url"])
        repin_source_inputs(receipt, records)
        with self.assertRaisesRegex(Refusal, "source_parser_selection$"):
            self.check(receipt, records, now)

    def test_missing_fields_cannot_use_candidate_fallbacks(self):
        for field, value in (("guid", "fixture-item-one"), ("title", "Fixture item"),
                             ("link", "https://fixture.example.test/item"),
                             ("pubDate", "2026-01-01T00:00:00Z")):
            with self.subTest(field=field):
                self.reject_body(lambda x: x.replace(f"<{field}>{value}</{field}>", ""))

    def test_duplicate_fields_or_fallback_identifiers_are_outside_profile(self):
        for extra in ("<guid>second-guid</guid>", "<title>Second title</title>",
                      "<link>https://fixture.example.test/other</link>",
                      "<pubDate>2026-01-01T00:00:01Z</pubDate>", "<id>other</id>",
                      "<published>2026-01-01T00:00:00Z</published>",
                      "<updated>2026-01-01T00:00:00Z</updated>"):
            with self.subTest(extra=extra):
                self.reject_body(lambda x: x.replace("</item>", extra + "</item>"))

    def test_second_item_or_channel_refuses_instead_of_silently_filtering(self):
        self.reject_body(lambda x: x.replace("</channel>",
            "<item><guid>other</guid><title>Other</title><link>https://fixture.example.test/other"
            "</link><pubDate>2026-01-01T00:00:00Z</pubDate></item></channel>"))
        self.reject_body(lambda x: x.replace("</rss>", "<channel></channel></rss>"))
        self.reject_body(lambda x: x.replace("</channel>", "<item/></channel>"))

    def test_attribute_shadowing_and_link_attributes_refuse(self):
        for old, new in (("<rss version=\"2.0\">", "<rss version=\"2.0\" channel=\"x\">"),
                         ("<channel>", "<channel item=\"x\">"),
                         ("<item>", "<item guid=\"other\">"),
                         ("<item>", "<item title=\"Other\">"),
                         ("<item>", "<item link=\"https://fixture.example.test/other\">"),
                         ("<item>", "<item pubDate=\"2026-01-01T00:00:01Z\">"),
                         ("<link>", "<link rel=\"alternate\">")):
            with self.subTest(new=new):
                self.reject_body(lambda x: x.replace(old, new))

    def test_namespace_prefixes_and_default_namespaces_refuse(self):
        for change in (lambda x: x.replace('<rss version="2.0">',
                                          '<rss version="2.0" xmlns="urn:fixture">'),
                       lambda x: x.replace("<item>", '<v:item xmlns:v="urn:fixture">').replace(
                                          "</item>", "</v:item>"),
                       lambda x: x.replace("<guid>", '<v:guid xmlns:v="urn:fixture">').replace(
                                          "</guid>", "</v:guid>")):
            self.reject_body(change)

    def test_dtd_entity_and_external_references_are_never_parsed(self):
        for payload in ('<!DOCTYPE rss [<!ENTITY ext SYSTEM "file:///private">]>',
                        '<!DOCTYPE rss [<!ENTITY ext SYSTEM "https://fixture.example.test/private">]>',
                        '<!DOCTYPE rss [<!ENTITY a "expanded"><!ENTITY b "&a;&a;">]>'):
            self.reject_body(lambda x: x.replace('<rss version="2.0">', payload +
                                                 '<rss version="2.0">'))
        for value in ("fixture&#45;item-one", "fixture&#x2d;item-one", "fixture&amp;item-one"):
            self.reject_body(lambda x: x.replace("fixture-item-one", value))

    def test_comments_cdata_and_processing_instructions_are_not_identity_shortcuts(self):
        for value in ("<![CDATA[fixture-item-one]]>", "fixture<!--hidden-->-item-one",
                      "fixture<?hidden value?>-item-one", "fixture<key>hidden</key>-item-one"):
            self.reject_body(lambda x: x.replace("fixture-item-one", value))
        self.reject_body(lambda x: x.replace("</rss>", "</rss><!--tail-->"))
        self.reject_body(lambda x: x.replace("<channel>", "<channel><?hidden value?>"))

    def test_noncanonical_xml_markup_and_empty_feed_refuse(self):
        for change in (lambda x: x + "\n", lambda x: "﻿" + x,
                       lambda x: x.replace('version="1.0"', "version='1.0'"),
                       lambda x: x.replace('<item>', '<item>\r\n'),
                       lambda x: x.replace("</rss>", ""),
                       lambda x: x.replace("<rss", "<feed").replace("</rss>", "</feed>"),
                       lambda x: x.replace("<item>", "<entry>").replace("</item>", "</entry>"),
                       lambda x: x[:x.index("<item>")] + "</channel></rss>"):
            self.reject_body(change)

    def test_unescaped_unicode_or_whitespace_key_cannot_match_claim(self):
        for value in ("fixture-item-one ", " fixture-item-one", "fixture-☃-one"):
            self.reject_body(lambda x: x.replace("fixture-item-one", value))

    def test_channel_metadata_urls_are_canonical_too(self):
        for url in ("https://xn--0.test/", "https://fixture.example.test/a/../b",
                    "https://fixture.example.test//b", "https://fixture.example.test"):
            with self.subTest(url=url):
                self.reject_body(lambda x: x.replace("https://fixture.example.test/</link>",
                                                     url + "</link>"), "source_target")

    def test_channel_metadata_is_not_hardcoded_to_a_single_test_fixture(self):
        receipt, records, now = source_input_fixture()
        body = records["source-body"]["data"]["body_utf8"].replace(
            "<title>Fixture</title>", "<title>Different channel 2</title>").replace(
            "<description>Inert fixture</description>", "<description>Another fixture</description>")
        replace_source_body(receipt, records, body)
        self.assertIsNone(self.check(receipt, records, now))

    def test_parser_refusal_never_echoes_restricted_source_content(self):
        receipt, records, now = source_input_fixture()
        body = records["source-body"]["data"]["body_utf8"].replace(
            "fixture-item-one", "private-value&secret;")
        replace_source_body(receipt, records, body)
        with self.assertRaises(Refusal) as caught:
            self.check(receipt, records, now)
        self.assertEqual(str(caught.exception), "announcement evidence refused: unsupported_rss_fixture")


if __name__ == "__main__":
    unittest.main()
