"""Inert candidate-archive inspection, never extraction or artifact authority.

The independent verifier must obtain the inventory from its authenticated
post-upload source. This function only checks bytes against an inventory; a
caller-created inventory cannot establish producer/run/approval authenticity.
No archive member is imported, executed, written to disk or promoted.
"""

import hashlib
import io
import re
import stat
import struct
import zipfile
import zlib

from announcement_runtime_schema import (
    ARCHIVE_LIMIT, RECEIPT_LIMIT, REFERENCE_LIMIT, SHA256, Refusal, _validate,
    parse_receipt, require,
)


RECEIPT_NAME = "candidate-receipt.json"
INVENTORY_FIELDS = frozenset({"path", "size", "sha256"})


def _member_name(name):
    return type(name) is str and (name == RECEIPT_NAME or re.fullmatch(
        r"evidence/[a-z][a-z0-9_-]{0,95}\.json", name) is not None)


def inspect_archive(raw, inventory):
    """Check a closed bounded ZIP and return its parsed receipt and inert files.

    Both compressed and expanded bytes are bounded. Non-receipt evidence is
    returned as bytes for subsequent typed independent verification, NOT
    treated as verified evidence. No successful result is an admission grant.
    """
    require(type(raw) is bytes and 0 < len(raw) <= ARCHIVE_LIMIT, "archive_size")
    require(type(inventory) is list and 1 <= len(inventory) <= REFERENCE_LIMIT + 1,
            "archive_inventory")
    expected = {}
    for item in inventory:
        require(type(item) is dict and item.keys() == INVENTORY_FIELDS,
                "archive_inventory")
        require(_member_name(item["path"]), "archive_name")
        require(type(item["size"]) is int and 0 < item["size"] <= ARCHIVE_LIMIT,
                "archive_member_size")
        _validate(item["sha256"], SHA256)
        require(item["path"] not in expected, "archive_duplicate")
        expected[item["path"]] = item
    require(RECEIPT_NAME in expected and expected[RECEIPT_NAME]["size"] <= RECEIPT_LIMIT,
            "archive_receipt")
    require(sum(item["size"] for item in inventory) <= ARCHIVE_LIMIT,
            "archive_expanded_size")

    try:
        # Canonical single-disk, non-ZIP64, no-comment ZIP. Reject hidden prefix,
        # suffix, descriptor or other unlisted local data, not just path names.
        require(len(raw) >= 22, "archive_format")
        end = struct.unpack_from("<4s4H2IH", raw, len(raw) - 22)
        signature, disk, directory_disk, local_count, total_count, directory_size, offset, comment = end
        require(signature == b"PK\x05\x06" and disk == directory_disk == comment == 0 and
                local_count == total_count == len(expected) and
                offset + directory_size == len(raw) - 22, "archive_format")
        # With ASCII names and no extras/comments, each central record has an
        # exact finite size. Bound parsing allocations before ZipFile reads it.
        require(directory_size == sum(46 + len(name) for name in expected),
                "archive_directory_size")
        with zipfile.ZipFile(io.BytesIO(raw)) as archive:
            infos = archive.infolist()
            require(len(infos) == len(expected), "archive_inventory")
            require(len({info.filename for info in infos}) == len(infos), "archive_duplicate")
            require({info.filename for info in infos} == set(expected), "archive_inventory")
            require(sum(info.file_size for info in infos) <= ARCHIVE_LIMIT,
                    "archive_expanded_size")
            files = {}
            local_end = 0
            for info in infos:
                require(_member_name(info.filename) and info.orig_filename == info.filename,
                        "archive_name")
                item = expected[info.filename]
                require(info.file_size == item["size"] and
                        0 <= info.compress_size <= ARCHIVE_LIMIT, "archive_member_size")
                mode = info.external_attr >> 16
                require(stat.S_IFMT(mode) in (0, stat.S_IFREG) and
                        not mode & 0o7111 and not info.is_dir(), "archive_mode")
                require(info.flag_bits == 0 and info.compress_type in (
                    zipfile.ZIP_STORED, zipfile.ZIP_DEFLATED) and
                    info.extra == info.comment == b"" and info.volume == 0 and
                    info.extract_version <= 20, "archive_metadata")
                require(info.header_offset == local_end, "archive_layout")
                header = struct.unpack_from("<4s5H3I2H", raw, local_end)
                sig, version, flags, compression, _time, _date, crc, compressed, size, name_size, extra_size = header
                require(sig == b"PK\x03\x04" and version <= 20 and flags == 0 and
                        compression == info.compress_type and crc == info.CRC and
                        compressed == info.compress_size and size == info.file_size and
                        extra_size == 0, "archive_layout")
                name_start = local_end + 30
                payload_start = name_start + name_size
                require(raw[name_start:payload_start] == info.filename.encode("ascii"),
                        "archive_layout")
                local_end = payload_start + compressed
                require(local_end <= offset, "archive_layout")
                payload = raw[payload_start:local_end]
                if compression == zipfile.ZIP_DEFLATED:
                    # ZipExtFile accepts unused bytes after a complete stream.
                    # Require one complete stream and account for every declared
                    # compressed byte, with the same bounded output ceiling.
                    inflater = zlib.decompressobj(-zlib.MAX_WBITS)
                    data = inflater.decompress(payload, item["size"] + 1)
                    require(len(data) <= item["size"], "archive_member_size")
                    require(inflater.eof and not inflater.unused_data and
                            not inflater.unconsumed_tail, "archive_format")
                else:
                    data = payload
                require(len(data) == item["size"], "archive_member_size")
                require(zlib.crc32(data) == info.CRC, "archive_format")
                require(hashlib.sha256(data).hexdigest() == item["sha256"], "archive_digest")
                files[info.filename] = data
            require(local_end == offset == archive.start_dir, "archive_layout")
    except (zipfile.BadZipFile, zipfile.LargeZipFile, RuntimeError,
            NotImplementedError, OSError, struct.error, UnicodeError, EOFError, zlib.error):
        raise Refusal("archive_format") from None
    return parse_receipt(files[RECEIPT_NAME]), files
