"""Exercise private materialization with synthetic, verified ZIPs."""

from hashlib import sha256
import ntpath
from pathlib import Path
from shutil import copyfileobj
import stat
import sys
from tempfile import TemporaryDirectory
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from zipfile import ZIP_DEFLATED, ZipFile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from materialize_predecessor import materialize_predecessor_bundle
from predecessor_bundle import BUILD, IMAGE
from staging_rollout import GateError
from test_predecessor_bundle import fixture, metadata, pack


class MaterializePredecessorTests(unittest.TestCase):
    def setUp(self):
        self.scratch = Path(self.enterContext(TemporaryDirectory()))
        self.output = self.scratch / "output"
        self.output.mkdir()
        self.members, self.receipt = fixture(self.scratch, 123, b"export default {};\r\n")

    def materialize(self):
        archive = self.enterContext(pack(self.members, self.receipt, compression=ZIP_DEFLATED))
        return archive, materialize_predecessor_bundle(
            archive, scratch_parent=self.output, **metadata(archive, 123, 457),
        )

    def add_module(self, path, data):
        self.members[f"worker/{path}"] = data
        self.receipt["worker_modules_sha256"][path] = sha256(data).hexdigest()

    def test_exact_bytes_and_private_lifetime(self):
        self.add_module("assets/Nested-01/Data.bin", b"\xff\x00\r\n")
        archive, materialization = self.materialize()
        with ZipFile(archive) as bundle:
            expected_receipt = bundle.read(BUILD)
        with materialization as root:
            self.assertEqual(root.parent, self.output)
            self.assertEqual(stat.S_IMODE(root.stat().st_mode) & 0o077, 0)
            self.assertEqual(
                {path.relative_to(root).as_posix(): path.read_bytes()
                 for path in root.rglob("*") if path.is_file()},
                {BUILD: expected_receipt, **self.members},
            )
        self.assertEqual(list(self.output.iterdir()), [])
        self.assertFalse(archive.closed)

    def test_cleans_output_when_consumer_raises(self):
        _, materialization = self.materialize()
        consumer_error = RuntimeError("synthetic consumer failure")
        with self.assertRaises(RuntimeError) as raised, materialization as root:
            self.assertTrue(root.is_dir())
            raise consumer_error
        self.assertIs(raised.exception, consumer_error)
        self.assertEqual(list(self.output.iterdir()), [])

    def test_rejects_traversal_before_materializing(self):
        self.add_module("../../escape.js", b"escape")
        _, materialization = self.materialize()
        with self.assertRaisesRegex(GateError, "^invalid_predecessor_members$"), materialization:
            self.fail("published a directory for a refused ZIP")
        self.assertEqual(list(self.output.iterdir()), [])
        self.assertFalse((self.scratch / "escape.js").exists())

    def test_cleans_partial_output_after_a_path_collision(self):
        self.add_module("index.js/nested.js", b"collision")
        _, materialization = self.materialize()
        with self.assertRaisesRegex(GateError, "^predecessor_materialization_failed$"), materialization:
            self.fail("published a directory for colliding paths")
        self.assertEqual(list(self.output.iterdir()), [])

    def test_cleans_incomplete_copy(self):
        _, materialization = self.materialize()

        def short_image_copy(source, destination):
            destination.write(source.read(1 if source.name == IMAGE else -1))

        with patch("materialize_predecessor.copyfileobj", short_image_copy):
            with self.assertRaisesRegex(GateError, "^predecessor_materialization_failed$"), materialization:
                self.fail("published a directory after a short copy")
        self.assertEqual(list(self.output.iterdir()), [])

    def test_refuses_extra_file_in_written_inventory(self):
        _, materialization = self.materialize()

        def copy_with_extra_file(source, destination):
            copyfileobj(source, destination)
            if source.name == BUILD:
                Path(destination.name).with_name("extra").touch()

        with patch("materialize_predecessor.copyfileobj", copy_with_extra_file):
            with self.assertRaisesRegex(GateError, "^predecessor_materialization_failed$"), materialization:
                self.fail("published a directory with extra files")
        self.assertEqual(list(self.output.iterdir()), [])

    def test_refuses_windows_reserved_path(self):
        self.add_module("assets/NUL/nested.js", b"reserved")
        _, materialization = self.materialize()

        with patch("materialize_predecessor.os", SimpleNamespace(name="nt", path=ntpath)):
            with self.assertRaisesRegex(GateError, "^predecessor_materialization_failed$"), materialization:
                self.fail("published a directory with a Windows device name")
        self.assertEqual(list(self.output.iterdir()), [])
