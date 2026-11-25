"""Exercise the OCI handoff with the verified ZIP and fake registry responses."""

from hashlib import sha256
from pathlib import Path
import sys
import tarfile
from tempfile import TemporaryDirectory
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from materialize_predecessor import materialize_predecessor_bundle
from predecessor_bundle import IMAGE
from predecessor_image import MEDIA_TYPE, plan_predecessor_image, verify_registry_manifest
from staging_rollout import GateError
from test_predecessor_bundle import fixture, metadata, pack

REPOSITORY = "registry.example.invalid/staging/two-bot-next-attendance-predecessor-staging"


class PredecessorImageTests(unittest.TestCase):
    def setUp(self):
        scratch = Path(self.enterContext(TemporaryDirectory()))
        self.members, receipt = fixture(scratch, 123, b"export default {};")
        archive = self.enterContext(pack(self.members, receipt))
        self.root = self.enterContext(materialize_predecessor_bundle(archive, **metadata(archive, 123, 457)))
        self.digest = receipt["image_manifest_digest"]
        self.reference = f"{REPOSITORY}@{self.digest}"
        with tarfile.open(self.root / IMAGE) as image:
            self.body = image.extractfile(f"blobs/sha256/{self.digest.removeprefix('sha256:')}").read()
        self.descriptor = {"mediaType": MEDIA_TYPE, "size": len(self.body), "digest": self.digest}

    def test_same_digest_readback_returns_the_scoped_immutable_reference(self):
        plan = plan_predecessor_image(self.root, staging_repository=REPOSITORY)
        self.assertEqual(plan.copy_argv, ("skopeo", "copy", "--preserve-digests", "--dest-tls-verify=true",
                                         f"oci-archive:{self.root / IMAGE}", f"docker://{self.reference}"))
        self.assertEqual(verify_registry_manifest(plan, reference=self.reference,
                                                 descriptor=self.descriptor, body=self.body), self.reference)
        self.assertEqual((self.root / IMAGE).read_bytes(), self.members[IMAGE])

    def test_changed_manifest_fails_even_with_the_expected_descriptor_digest(self):
        plan = plan_predecessor_image(self.root, staging_repository=REPOSITORY)
        changed = self.body + b"\n"
        descriptor = {**self.descriptor, "size": len(changed)}
        with self.assertRaisesRegex(GateError, "^predecessor_registry_manifest_mismatch$"):
            verify_registry_manifest(plan, reference=self.reference, descriptor=descriptor, body=changed)
        descriptor["digest"] = "sha256:" + sha256(changed).hexdigest()
        with self.assertRaisesRegex(GateError, "^predecessor_registry_descriptor_mismatch$"):
            verify_registry_manifest(plan, reference=self.reference, descriptor=descriptor, body=changed)

    def test_optional_digest_header_never_replaces_the_raw_body_hash(self):
        plan = plan_predecessor_image(self.root, staging_repository=REPOSITORY)
        descriptor = {"mediaType": MEDIA_TYPE, "size": len(self.body)}
        self.assertEqual(verify_registry_manifest(plan, reference=self.reference,
                                                 descriptor=descriptor, body=self.body), self.reference)
        changed = b" " + self.body[1:]
        with self.assertRaisesRegex(GateError, "^predecessor_registry_manifest_mismatch$"):
            verify_registry_manifest(plan, reference=self.reference, descriptor=descriptor, body=changed)
        with self.assertRaisesRegex(GateError, "^predecessor_registry_descriptor_mismatch$"):
            verify_registry_manifest(plan, reference=self.reference,
                                     descriptor={**descriptor, "digest": "sha256:" + "f" * 64}, body=self.body)

    def test_image_index_media_type_fails_even_with_the_expected_bytes(self):
        plan = plan_predecessor_image(self.root, staging_repository=REPOSITORY)
        index_type = "application/vnd.oci.image.index.v1+json"
        index = b'{"schemaVersion":2,"manifests":[]}'
        for body in (index, self.body):
            with self.subTest(size=len(body)):
                descriptor = {**self.descriptor, "mediaType": index_type, "size": len(body)}
                with self.assertRaisesRegex(GateError, "^predecessor_registry_descriptor_mismatch$"):
                    verify_registry_manifest(plan, reference=self.reference, descriptor=descriptor, body=body)

    def test_incomplete_descriptor_fails_even_with_the_expected_bytes(self):
        plan = plan_predecessor_image(self.root, staging_repository=REPOSITORY)
        for descriptor in ({**self.descriptor, "digest": None}, {**self.descriptor, "size": len(self.body) + 1}):
            with self.assertRaisesRegex(GateError, "^predecessor_registry_descriptor_mismatch$"):
                verify_registry_manifest(plan, reference=self.reference, descriptor=descriptor, body=self.body)

    def test_wrong_registry_scope_or_tag_refuses_the_same_digest(self):
        plan = plan_predecessor_image(self.root, staging_repository=REPOSITORY)
        for reference in (self.reference.replace("registry.", "other."),
                          self.reference.replace("/staging/", "/production/"),
                          self.reference.replace("-predecessor-staging@", "-production@"),
                          REPOSITORY + ":latest"):
            with self.subTest(reference=reference):
                with self.assertRaisesRegex(GateError, "^predecessor_registry_scope_mismatch$"):
                    verify_registry_manifest(plan, reference=reference, descriptor=self.descriptor, body=self.body)
        for repository in (REPOSITORY.replace("-predecessor-staging", "-production"),
                           "https://" + REPOSITORY, REPOSITORY + ":latest", self.reference):
            with self.subTest(repository=repository):
                with self.assertRaisesRegex(GateError, "^predecessor_registry_scope_refused$"):
                    plan_predecessor_image(self.root, staging_repository=repository)

    def test_changed_materialized_archive_cannot_be_planned(self):
        image = self.root / IMAGE
        with image.open("ab") as stream:
            stream.write(b"unexpected trailing bytes")
        with self.assertRaisesRegex(GateError, "^predecessor_image_archive_digest_mismatch$"):
            plan_predecessor_image(self.root, staging_repository=REPOSITORY)
