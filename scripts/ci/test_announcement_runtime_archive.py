"""Memory-only archive regressions; no service inventory or runtime authority."""

import copy
import hashlib
import io
import json
from pathlib import Path
import stat
import struct
import sys
import unittest
import warnings
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import announcement_runtime_archive as archive
import announcement_runtime_fixtures as fixtures
from announcement_runtime_schema import Refusal


def contents():
    return {"candidate-receipt.json": json.dumps(fixtures.receipt()).encode(),
            "evidence/sample.json": b'{"inert":"synthetic-not-authority"}'}


def inventory(files):
    return [{"path": name, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
            for name, data in files.items()]


def package(files=None, *, compression=zipfile.ZIP_DEFLATED, mode=stat.S_IFREG | 0o600,
            comment=b"", extra=b"", duplicate=False):
    files = contents() if files is None else files
    output = io.BytesIO()
    with zipfile.ZipFile(output, "w") as zipped:
        zipped.comment = comment
        for name, data in files.items():
            info = zipfile.ZipInfo(name)
            info.compress_type = compression
            info.external_attr = mode << 16
            info.extra = extra
            zipped.writestr(info, data)
        if duplicate:
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", UserWarning)
                zipped.writestr("candidate-receipt.json", files["candidate-receipt.json"])
    return output.getvalue()


class CandidateArchive(unittest.TestCase):
    def test_exact_closed_inventory_stays_inert(self):
        for compression in (zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED):
            files = contents()
            parsed, inert = archive.inspect_archive(package(files, compression=compression),
                                                    inventory(files))
            self.assertEqual(parsed, fixtures.receipt())
            self.assertEqual(inert, files)
            self.assertIs(parsed["promotable"], False)

    def test_only_receipt_is_allowed_when_inventory_is_exact(self):
        files = {"candidate-receipt.json": contents()["candidate-receipt.json"]}
        parsed, _ = archive.inspect_archive(package(files), inventory(files))
        self.assertEqual(parsed["schema"], "two.bot.announcement-candidate-staging.v1")

    def test_non_json_evidence_remains_bytes_not_accepted_evidence(self):
        files = contents()
        files["evidence/sample.json"] = b"import os; do_not_execute()"
        _, inert = archive.inspect_archive(package(files), inventory(files))
        self.assertEqual(inert["evidence/sample.json"], files["evidence/sample.json"])

    def test_canonical_mixed_and_renamed_members_refuse(self):
        for bad in ("promotion-receipt.json", "admitted.json", "bot-promotion-123.json",
                    "../candidate-receipt.json", "/candidate-receipt.json", "evidence/../x.json",
                    "evidence/a/b.json", "evidence/SAMPLE.json", "evidence/sample.json\x00tail",
                    "evidence\\sample.json", "candidate-receipt.json/", "other.json"):
            with self.subTest(name=bad):
                files = contents()
                files[bad] = b"{}"
                with self.assertRaises(Refusal):
                    archive.inspect_archive(package(files), inventory(files))
        files = {"promotion-receipt.json": contents()["candidate-receipt.json"]}
        with self.assertRaises(Refusal):
            archive.inspect_archive(package(files), inventory(files))

    def test_unlisted_missing_replaced_duplicate_members_refuse(self):
        files = contents()
        mutations = [inventory(files)[:1], inventory(files) + [inventory(files)[1]],
                     [dict(x, sha256="0" * 64) for x in inventory(files)],
                     [dict(x, size=x["size"] + 1) for x in inventory(files)]]
        for expected in mutations:
            with self.subTest(expected=expected), self.assertRaises(Refusal):
                archive.inspect_archive(package(files), expected)
        expected = inventory(files)
        expected.append({"path": "evidence/missing.json", "size": 2,
                         "sha256": hashlib.sha256(b"{}").hexdigest()})
        with self.assertRaises(Refusal):
            archive.inspect_archive(package(files), expected)
        with self.assertRaises(Refusal):
            archive.inspect_archive(package(files, duplicate=True), inventory(files))

    def test_inventory_is_closed_and_bounded(self):
        raw = package()
        for bad in (None, {}, [], inventory(contents()) * 130,
                    [dict(inventory(contents())[0], extension="extra")],
                    [dict(inventory(contents())[0], size=True)],
                    [dict(inventory(contents())[0], size=0)],
                    [dict(inventory(contents())[0], size=archive.ARCHIVE_LIMIT + 1)],
                    [dict(inventory(contents())[0], size=archive.RECEIPT_LIMIT + 1)],
                    [dict(inventory(contents())[0], sha256="SHA256:" + "a" * 64)]):
            with self.subTest(inventory_type=type(bad).__name__), self.assertRaises(Refusal):
                archive.inspect_archive(raw, bad)
        expanded = inventory(contents())
        expanded[1]["size"] = archive.ARCHIVE_LIMIT
        with self.assertRaisesRegex(Refusal, "archive_expanded_size"):
            archive.inspect_archive(raw, expanded)

    def test_expanded_archive_exact_limit_then_one_byte_over(self):
        files = contents()
        receipt_size = len(files["candidate-receipt.json"])
        files["evidence/sample.json"] = b"x" * (archive.ARCHIVE_LIMIT - receipt_size)
        _, inert = archive.inspect_archive(package(files), inventory(files))
        self.assertEqual(sum(map(len, inert.values())), archive.ARCHIVE_LIMIT)
        files["evidence/sample.json"] += b"x"
        with self.assertRaisesRegex(Refusal, "archive_expanded_size"):
            archive.inspect_archive(package(files), inventory(files))

    def test_deflate_member_must_consume_its_complete_declared_payload(self):
        files = contents()
        original = package(files)
        parsed, inert = archive.inspect_archive(original, inventory(files))
        self.assertEqual(parsed, fixtures.receipt())
        self.assertEqual(inert, files)
        with zipfile.ZipFile(io.BytesIO(original)) as zipped:
            members = zipped.infolist()
        for selected in members:
            for suffix in (b"\x00", b"inert-unlisted-tail"):
                with self.subTest(member=selected.filename, suffix=suffix):
                    # Keep every ZIP offset/size consistent while declaring
                    # extra compressed payload. Expanded bytes, CRC and the
                    # complete approved file inventory stay unchanged.
                    raw = bytearray(original)
                    local = selected.header_offset
                    name_size = struct.unpack_from("<H", raw, local + 26)[0]
                    end = local + 30 + name_size + selected.compress_size
                    directory = struct.unpack_from("<I", raw, len(raw) - 6)[0]
                    raw[end:end] = suffix
                    delta = len(suffix)
                    struct.pack_into("<I", raw, local + 18, selected.compress_size + delta)
                    central = directory + delta
                    for member in members:
                        if member is selected:
                            struct.pack_into("<I", raw, central + 20, member.compress_size + delta)
                        if member.header_offset >= end:
                            struct.pack_into("<I", raw, central + 42, member.header_offset + delta)
                        central += 46 + len(member.filename)
                    struct.pack_into("<I", raw, len(raw) - 6, directory + delta)
                    # Show that the packet is a readable ZIP with exact files;
                    # refusal must come from complete stream consumption.
                    with zipfile.ZipFile(io.BytesIO(raw)) as zipped:
                        self.assertEqual({name: zipped.read(name) for name in files}, files)
                    with self.assertRaisesRegex(Refusal, "archive_format") as ctx:
                        archive.inspect_archive(bytes(raw), inventory(files))
                    self.assertIsNone(ctx.exception.__cause__)

    def test_invalid_deflate_stream_is_a_constant_refusal(self):
        raw = bytearray(package())
        name_size = struct.unpack_from("<H", raw, 26)[0]
        payload_start = 30 + name_size
        raw[payload_start:payload_start + 2] = b"\xff\xff"
        with self.assertRaisesRegex(Refusal, "archive_format") as ctx:
            archive.inspect_archive(bytes(raw), inventory(contents()))
        self.assertIsNone(ctx.exception.__cause__)

    def test_archive_byte_limit(self):
        expected = inventory(contents())
        for raw in (b"", b"PK", "not-bytes", bytearray(package()),
                    b"x" * (archive.ARCHIVE_LIMIT + 1)):
            with self.subTest(raw_type=type(raw).__name__), self.assertRaises(Refusal):
                archive.inspect_archive(raw, expected)

    def test_metadata_links_exec_and_comment_refuse(self):
        for mode in (stat.S_IFLNK | 0o600, stat.S_IFDIR | 0o600, stat.S_IFREG | 0o700,
                     stat.S_IFREG | 0o4600, stat.S_IFIFO | 0o600):
            with self.subTest(mode=mode), self.assertRaises(Refusal):
                archive.inspect_archive(package(mode=mode), inventory(contents()))
        for options in ({"comment": b"hidden"}, {"extra": b"\x00\x00\x00\x00"},
                        {"compression": zipfile.ZIP_BZIP2}):
            with self.subTest(options=options), self.assertRaises(Refusal):
                archive.inspect_archive(package(**options), inventory(contents()))

    def test_unlisted_prefix_suffix_and_disk_fields_refuse(self):
        raw = package()
        for changed in (b"hidden" + raw, raw + b"hidden"):
            with self.assertRaises(Refusal):
                archive.inspect_archive(changed, inventory(contents()))
        for offset in (4, 6, 8, 10):
            changed = bytearray(raw)
            struct.pack_into("<H", changed, len(raw) - 22 + offset, 99)
            with self.subTest(offset=offset), self.assertRaises(Refusal):
                archive.inspect_archive(bytes(changed), inventory(contents()))

    def test_local_header_encryption_descriptor_size_and_name_refuse(self):
        raw = package()
        mutations = [(6, "<H", 1), (6, "<H", 8), (18, "<I", 123456),
                     (22, "<I", 123456), (28, "<H", 4)]
        for offset, fmt, value in mutations:
            changed = bytearray(raw)
            struct.pack_into(fmt, changed, offset, value)
            with self.subTest(offset=offset), self.assertRaises(Refusal):
                archive.inspect_archive(bytes(changed), inventory(contents()))
        changed = bytearray(raw)
        changed[30] = ord("x")
        with self.assertRaises(Refusal):
            archive.inspect_archive(bytes(changed), inventory(contents()))

    def test_corrupt_stream_and_receipt_duplicate_keys_never_echo(self):
        sentinel = b"private-example-never-print"
        files = contents()
        files["candidate-receipt.json"] = b'{"secret":"' + sentinel + b'","secret":0}'
        with self.assertRaises(Refusal) as ctx:
            archive.inspect_archive(package(files), inventory(files))
        self.assertNotIn(sentinel.decode(), str(ctx.exception))
        self.assertIsNone(ctx.exception.__cause__)
        raw = bytearray(package(compression=zipfile.ZIP_STORED))
        raw[60] ^= 1
        with self.assertRaises(Refusal) as ctx:
            archive.inspect_archive(bytes(raw), inventory(contents()))
        self.assertEqual(ctx.exception.code, "archive_format")

    def test_no_fake_hash_circularity(self):
        files = contents()
        expected = inventory(files)
        changed = copy.deepcopy(expected)
        changed[0]["sha256"] = hashlib.sha256(package(files)).hexdigest()
        with self.assertRaisesRegex(Refusal, "archive_digest"):
            archive.inspect_archive(package(files), changed)


if __name__ == "__main__":
    unittest.main()
