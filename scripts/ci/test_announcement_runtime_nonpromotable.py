"""Candidate bytes cannot become canonical promotion evidence by relabeling.

Reuse the existing immutable-promotion fixture's read-only service boundary and
real canonical admission command. No production tool or workflow is modified.
"""

import io
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_immutable_bot_promotion as canonical
from announcement_runtime_fixtures import receipt
from announcement_runtime_source import CANDIDATE


def archive(members):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as packet:
        for name, data in members.items():
            packet.writestr(name, data)
    return buffer.getvalue()


class CandidateNeverPromotes(canonical.PromotionFixture):
    def test_canonical_name_and_run_metadata_cannot_authenticate_candidate_bytes(self):
        self.replace_package(record=receipt())
        self.assertEqual(self.github.artifact["name"], canonical.NAME)
        self.assertEqual(self.github.run, canonical.run_record())
        self.refused()

    def test_candidate_namespace_and_filename_refuse_canonical_admission(self):
        blob = archive({"candidate-receipt.json": json.dumps(receipt())})
        self.replace_package(blob=blob)
        name = f"bot-candidate-validation-1639-{CANDIDATE}-{canonical.STAGE_RUN}-{canonical.ATTEMPT}"
        self.github.artifact["name"] = name
        self.github.listing["artifacts"][0]["name"] = name
        self.refused()

    def test_mixed_packets_refuse_even_with_a_valid_canonical_receipt(self):
        blob = archive({
            "promotion-receipt.json": json.dumps(canonical.receipt()),
            "candidate-receipt.json": json.dumps(receipt()),
            **{"modules/" + name: data for name, data in canonical.MODULES.items()},
        })
        self.replace_package(blob=blob)
        self.refused()

    def test_promotable_type_or_purpose_edits_do_not_convert_candidate_schema(self):
        for key, value in (("promotable", True), ("promotable", 1),
                           ("purpose", "production"), ("schema", "promotion"),
                           ("schema_version", 1)):
            with self.subTest(field=key, value=value):
                candidate = receipt()
                candidate[key] = value
                self.replace_package(record=candidate)
                self.refused()

    def test_wrong_control_image_run_attempt_or_producer_never_reaches_image_copy(self):
        for case in ("control", "image", "run", "attempt", "producer", "repository"):
            with self.subTest(case=case):
                candidate = receipt()
                if case == "control":
                    candidate["control"]["version"]["commit"] = "f" * 40
                elif case == "image":
                    candidate["build"]["image_digest"] = "sha256:" + "f" * 64
                elif case == "run":
                    candidate["attempt"]["run"]["run_id"] = "9999"
                elif case == "attempt":
                    candidate["attempt"]["run"]["attempt"] = 99
                self.replace_package(record=candidate)
                if case == "producer":
                    self.github.run["path"] = ".github/workflows/validate-staging-candidate.yml"
                elif case == "repository":
                    self.github.artifact["workflow_run"]["repository_id"] = 2
                self.refused()

    def test_candidate_after_source_merge_is_still_not_canonical_history(self):
        # Matching main-branch/current-run service claims after a hypothetical
        # merge do not change the meaning of candidate bytes. No merge is run.
        with patch.object(canonical, "SHA", CANDIDATE):
            candidate = receipt()
            candidate["scope"]["candidate"]["commit"] = CANDIDATE
            self.replace_package(record=candidate)
            self.assertEqual(self.github.run["head_branch"], "main")
            self.assertEqual(self.github.run["head_sha"], CANDIDATE)
            self.refused()


if __name__ == "__main__":
    unittest.main()
