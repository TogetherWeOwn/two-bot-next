"""Offline contracts for shell-free, non-deploying runtime image inspection."""

from datetime import datetime, timezone
import importlib.util
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location("evidence", ROOT / "scripts/runtime-image-evidence.py")
evidence = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(evidence)
PREFLIGHT_SPEC = importlib.util.spec_from_file_location("preflight", ROOT / "scripts/vulnerability-preflight.py")
preflight = importlib.util.module_from_spec(PREFLIGHT_SPEC)
PREFLIGHT_SPEC.loader.exec_module(preflight)
IMAGE_ID = "sha256:" + "a" * 64
SOURCE_SHA = "b" * 40
METADATA = [{"Id": IMAGE_ID, "Architecture": "amd64", "Os": "linux", "Config": {
    "User": "65532:65532", "Entrypoint": ["/home/nonroot/two-bot"],
    "Env": ["SECRET=must-not-be-recorded"], "Cmd": None,
}}]
LIBS = "usr/lib/x86_64-linux-gnu"


def scratch():
    return tempfile.TemporaryDirectory(dir=os.environ.get("PAPERCLIP_RUN_SCRATCH_DIR") or os.environ.get("RUNNER_TEMP"))


def elf(needed=("libgcc_s.so.1", "libc.so.6"), interpreter="/lib64/ld-linux-x86-64.so.2"):
    """A minimal little-endian ELF64 with PT_INTERP, one PT_LOAD and PT_DYNAMIC."""
    interp = interpreter.encode() + b"\0"
    strtab, names = b"\0", []
    for soname in needed:
        names.append(len(strtab))
        strtab += soname.encode() + b"\0"
    interp_at = 64 + 3 * 56
    strtab_at = interp_at + len(interp)
    dynamic_at = strtab_at + len(strtab)
    dynamic = b"".join(struct.pack("<qQ", 1, at) for at in names)
    dynamic += struct.pack("<qQ", 5, strtab_at) + struct.pack("<qQ", 0, 0)
    total = dynamic_at + len(dynamic)
    header = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    header += struct.pack("<HHIQQQIHHHHHH", 3, 62, 1, 0, 64, 0, 0, 64, 56, 3, 0, 0, 0)
    phdrs = struct.pack("<IIQQQQQQ", 3, 4, interp_at, interp_at, interp_at, len(interp), len(interp), 1)
    phdrs += struct.pack("<IIQQQQQQ", 1, 5, 0, 0, 0, total, total, 4096)
    phdrs += struct.pack("<IIQQQQQQ", 2, 6, dynamic_at, dynamic_at, dynamic_at, len(dynamic), len(dynamic), 8)
    return header + phdrs + interp + strtab + dynamic


def control(package, version, arch="amd64", extra=""):
    return f"Package: {package}\nVersion: {version}\nArchitecture: {arch}\n{extra}Description: fixture\n multi-line\n"


class ImageFixture:
    """The merged filesystem `docker export` streams for a distroless image."""

    def __init__(self):
        status = evidence.STATUS_DIR
        self.entries = {
            "lib": ("link", "usr/lib"), "lib64": ("link", "usr/lib64"), "bin": ("link", "usr/bin"),
            "usr/lib64/ld-linux-x86-64.so.2": ("link", "../lib/x86_64-linux-gnu/ld-linux-x86-64.so.2"),
            f"{LIBS}/ld-linux-x86-64.so.2": ("file", b"ld"), f"{LIBS}/libc.so.6": ("file", b"libc"),
            f"{LIBS}/libgcc_s.so.1": ("file", b"libgcc"),
            "etc/ssl/certs/ca-certificates.crt": ("file", b"-----BEGIN CERTIFICATE-----\n"),
            status + "libc6": ("file", control("libc6", "2.41-12+deb13u4").encode()),
            status + "libc6.md5sums": ("file", (
                "0" * 32 + f"  {LIBS}/libc.so.6\n" + "1" * 32 + f"  {LIBS}/ld-linux-x86-64.so.2\n").encode()),
            status + "libgcc-s1": ("file", control("libgcc-s1", "14.2.0-19").encode()),
            status + "libgcc-s1.md5sums": ("file", ("2" * 32 + f"  {LIBS}/libgcc_s.so.1\n").encode()),
            status + "zlib1g": ("file", control("zlib1g", "1:1.3.dfsg+really1.3.1-1+b1").encode()),
            status + "tzdata": ("file", control("tzdata", "2026c-0+deb13u1", "all").encode()),
            evidence.BINARY: ("file", elf()),
        }
        self.modes = {}
        self.pax = {}

    def archive(self):
        buffer = io.BytesIO()
        with tarfile.open(fileobj=buffer, mode="w", format=tarfile.PAX_FORMAT) as tar:
            for directory in ("etc", "home", "home/nonroot", "usr", "usr/lib", LIBS, "usr/lib64", "var/lib/dpkg",
                              evidence.STATUS_DIR.rstrip("/")):
                info = tarfile.TarInfo(directory)
                info.type, info.mode = tarfile.DIRTYPE, 0o755
                tar.addfile(info)
            for name, (kind, value) in self.entries.items():
                info = tarfile.TarInfo(name)
                info.mode = self.modes.get(name, 0o644)
                info.pax_headers = self.pax.get(name, {})
                if kind == "link":
                    info.type, info.linkname = tarfile.SYMTYPE, value
                    tar.addfile(info)
                else:
                    info.size = len(value)
                    tar.addfile(info, io.BytesIO(value))
        return buffer.getvalue()


class FakeDocker:
    def __init__(self, image):
        self.image = image
        self.calls = []
        self.metadata = METADATA
        self.create = (0, b"f" * 64 + b"\n", b"")
        self.export = None
        self.remove = (0, b"", b"")
        self.timeouts = set()
        self.missing_client = False

    def __call__(self, *args, timeout=30):
        self.calls.append(args)
        if self.missing_client and args[0] != "image":
            raise FileNotFoundError("docker")
        if args[0] in self.timeouts:
            raise subprocess.TimeoutExpired(["docker", *args], timeout, output=b"partial", stderr=b"\xffslow")
        if args[:2] == ("image", "inspect"):
            return subprocess.CompletedProcess(args, 0, json.dumps(self.metadata).encode(), b"")
        code, stdout, stderr = {
            "create": self.create, "rm": self.remove,
            "export": self.export or (0, self.image.archive(), b""),
        }[args[0]]
        if args[0] == "rm" and isinstance(stderr, str):
            stderr = stderr.replace("{name}", args[2]).encode()
        return subprocess.CompletedProcess(args, code, stdout, stderr)


class EvidenceTests(unittest.TestCase):
    def setUp(self):
        self.image = ImageFixture()
        self.docker = FakeDocker(self.image)

    def collect(self):
        with scratch() as temporary:
            directory = Path(temporary)
            (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
            (directory / "source-sha.txt").write_text(SOURCE_SHA + "\n")
            with patch.object(evidence, "docker", self.docker):
                report = evidence.collect("two-bot:fixture", directory)
            self.assertEqual(json.loads((directory / "runtime-image-evidence.json").read_text()), report)
            return report

    def rejected(self, message):
        report = self.collect()
        self.assertIs(report["complete"], False)
        self.assertNotIn("filesystem", report)
        self.assertRegex(report["collection_error"], message)
        self.assertEqual(self.docker.calls[-1][:2], ("rm", "--force"))
        return report

    def test_identity_mismatch_stops_before_any_container_is_created(self):
        self.docker.metadata = [{**METADATA[0], "Id": "sha256:" + "c" * 64}]
        with self.assertRaisesRegex(ValueError, "identity"):
            self.collect()
        self.assertEqual(len(self.docker.calls), 1)

    def test_invalid_provenance_stops_before_docker(self):
        with scratch() as temporary:
            directory = Path(temporary)
            (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
            (directory / "source-sha.txt").write_text("unknown\n")
            with patch.object(evidence, "docker") as docker, self.assertRaises(ValueError):
                evidence.collect("two-bot:fixture", directory)
            docker.assert_not_called()

    def test_report_is_bound_to_image_and_records_status_d_inventory_without_environment(self):
        report = self.collect()
        self.assertIs(report["complete"], True)
        self.assertNotIn("collection_error", report)
        self.assertEqual((report["schema_version"], report["image_id"], report["source_sha"]), (2, IMAGE_ID, SOURCE_SHA))
        self.assertEqual((report["architecture"], report["os"], report["configured_user"]), ("amd64", "linux", "65532:65532"))
        self.assertNotIn("SECRET", json.dumps(report))
        self.assertNotIn("accepted", json.dumps(report))
        self.assertEqual([(row["package"], row["version"], row["architecture"], row["status"])
                          for row in report["filesystem"]["installed_packages"]], [
            ("libc6", "2.41-12+deb13u4", "amd64", None), ("libgcc-s1", "14.2.0-19", "amd64", None),
            ("tzdata", "2026c-0+deb13u1", "all", None), ("zlib1g", "1:1.3.dfsg+really1.3.1-1+b1", "amd64", None)])
        self.assertEqual(report["container"]["cleanup"]["status"], "removed")
        self.assertEqual(report["container"]["export"]["stdout_bytes"], len(self.image.archive()))

    def test_preflight_consumes_the_collected_report_shape(self):
        report = self.collect()
        purl = "pkg:deb/debian/zlib1g@1.3.dfsg%2Breally1.3.1-1%2Bb1?arch=amd64&distro=debian-13.7&epoch=1"
        version = "1:1.3.dfsg+really1.3.1-1+b1"
        package = {"ID": "zlib1g@" + version, "Name": "zlib1g", "Arch": "amd64", "Identifier": {"PURL": purl}}
        scan = {"SchemaVersion": 2, "Trivy": {"Version": "0.69.3"}, "ArtifactType": "container_image",
                "Metadata": {"ImageID": IMAGE_ID, "OS": {"Family": "debian", "Name": "13.7"},
                             "ImageConfig": {"architecture": "amd64", "os": "linux"}},
                "Results": [{"Class": "os-pkgs", "Type": "debian", "Packages": [package], "Vulnerabilities": [
                    {"VulnerabilityID": "CVE-2026-99999", "Severity": "HIGH", "PkgName": "zlib1g",
                     "InstalledVersion": version, "PkgID": package["ID"], "PkgIdentifier": {"PURL": purl}}]}]}
        bound = preflight.preflight(scan, report, SOURCE_SHA, IMAGE_ID, datetime.now(timezone.utc))
        self.assertEqual([(row["package"], row["status"], row["suppressed"]) for row in bound["findings"]],
                         [("zlib1g", "not-accepted", False)])

    def test_nothing_executes_inside_the_image(self):
        self.collect()
        name = self.docker.calls[1][self.docker.calls[1].index("--name") + 1]
        self.assertTrue(name.startswith("two-bot-inspect-"))
        self.assertEqual(self.docker.calls, [
            ("image", "inspect", "two-bot:fixture"),
            ("create", "--name", name, "--network", "none", "--no-healthcheck", IMAGE_ID),
            ("export", name), ("rm", "--force", name)])

    def test_binary_linkage_resolves_through_image_symlinks_to_owning_packages(self):
        binary = self.collect()["filesystem"]["binary"]
        self.assertEqual(binary["path"], "/home/nonroot/two-bot")
        self.assertEqual(binary["size"], len(elf()))
        self.assertEqual(binary["interpreter"], {"path": "/lib64/ld-linux-x86-64.so.2",
                                                 "resolved": f"/{LIBS}/ld-linux-x86-64.so.2", "packages": ["libc6"]})
        self.assertEqual(list(binary["needed"]), ["libgcc_s.so.1", "libc.so.6"])
        self.assertEqual(binary["needed"]["libgcc_s.so.1"]["packages"], ["libgcc-s1"])
        self.assertEqual(binary["needed"]["libc.so.6"]["resolved"], f"/{LIBS}/libc.so.6")

    def test_setid_capabilities_and_shell_tools_are_recorded_not_hidden(self):
        self.image.entries["usr/bin/sh"] = ("file", b"#!")
        self.image.entries["usr/bin/ping"] = ("file", b"ping")
        self.image.modes["usr/bin/ping"] = 0o4755
        self.image.pax["usr/bin/ping"] = {evidence.CAPABILITY: "\x01"}
        filesystem = self.collect()["filesystem"]
        self.assertEqual(filesystem["setid_files"], [{"path": "/usr/bin/ping", "mode": "0o4755", "uid": 0, "gid": 0}])
        self.assertEqual(filesystem["file_capabilities"], ["/usr/bin/ping"])
        self.assertEqual(filesystem["tools_present"], ["/bin/sh", "/usr/bin/sh"])

    def test_clean_distroless_layout_records_no_privileged_files_or_tools(self):
        filesystem = self.collect()["filesystem"]
        self.assertEqual((filesystem["setid_files"], filesystem["file_capabilities"], filesystem["tools_present"]), ([], [], []))

    def test_explicit_installed_status_and_arch_qualified_file_names_are_accepted(self):
        status = evidence.STATUS_DIR
        self.image.entries[status + "libc6"] = ("file", control("libc6", "2.41-12+deb13u4", extra="Status: install ok installed\n").encode())
        self.image.entries[status + "zlib1g:amd64"] = self.image.entries.pop(status + "zlib1g")
        rows = self.collect()["filesystem"]["installed_packages"]
        self.assertEqual(rows[0]["status"], "install ok installed")
        self.assertEqual(rows[-1]["source_file"], "/" + status + "zlib1g:amd64")

    def test_status_d_inventory_fails_closed(self):
        status = evidence.STATUS_DIR
        good = control("libc6", "2.41-12+deb13u4")
        cases = {
            "not installed": ("file", control("libc6", "2.41", extra="Status: deinstall ok config-files\n").encode()),
            "missing Package, Version or": ("file", b"Package: libc6\nArchitecture: amd64\n"),
            "more than one paragraph": ("file", (good + "\n" + "Source: glibc\n").encode()),
            "malformed or duplicate": ("file", (good + "Version: 2\n").encode()),
            "continuation before": ("file", b" orphan\n" + good.encode()),
            "not UTF-8": ("file", good.encode() + b"Maintainer: \xff\n"),
            "not a regular": ("link", "libgcc-s1"),
            "file name differs": ("file", control("libc-bin", "2.41").encode()),
            "missing Package, Version": ("file", b"Package: libc6\nVersion: 2 41\nArchitecture: amd64\n"),
        }
        for message, entry in cases.items():
            with self.subTest(message):
                self.image.entries[status + "libc6"] = entry
                self.rejected("unparseable: .*" + message)
        self.image.entries[status + "libc6"] = ("file", good.encode())
        self.image.entries[status + "libc6:amd64"] = ("file", good.encode())
        self.rejected("duplicate package")

    def test_missing_or_classic_package_databases_fail_closed(self):
        self.image.entries = {name: entry for name, entry in self.image.entries.items()
                              if not name.startswith(evidence.STATUS_DIR) or name.endswith(".md5sums")}
        self.rejected("Empty status.d package inventory")
        self.image = ImageFixture()
        self.docker = FakeDocker(self.image)
        self.image.entries["var/lib/dpkg/status"] = ("file", control("libc6", "2.41").encode())
        self.rejected("dpkg status database present")

    def test_binary_and_linkage_fail_closed(self):
        cases = [("is not a regular file", lambda entries: entries.pop(evidence.BINARY)),
                 ("not a little-endian ELF64", lambda entries: entries.update({evidence.BINARY: ("file", b"#!/bin/sh\n")})),
                 ("DT_NEEDED libssl.so.3 does not resolve", lambda entries: entries.update(
                     {evidence.BINARY: ("file", elf(("libssl.so.3", "libc.so.6")))})),
                 ("interpreter /lib/ld.so does not resolve", lambda entries: entries.update(
                     {evidence.BINARY: ("file", elf(interpreter="/lib/ld.so"))})),
                 ("Symlink loop", lambda entries: entries.update(
                     {"lib64": ("link", "lib64x"), "lib64x": ("link", "lib64")}))]
        for message, mutate in cases:
            with self.subTest(message):
                self.image = ImageFixture()
                self.docker = FakeDocker(self.image)
                mutate(self.image.entries)
                self.rejected(message)

    def test_non_tar_or_duplicate_export_fails_closed(self):
        self.docker.export = (0, b"not a tar archive" * 64, b"")
        self.rejected("unparseable")
        duplicate = io.BytesIO()
        with tarfile.open(fileobj=duplicate, mode="w") as tar:
            for _ in range(2):
                tar.addfile(tarfile.TarInfo("etc/passwd"), io.BytesIO(b""))
        self.docker.export = (0, duplicate.getvalue(), b"")
        self.rejected("duplicate archive entry")

    def test_elf_parser_reports_interpreter_and_needed_in_order(self):
        self.assertEqual(evidence.elf_linkage(elf(("a.so", "b.so"))), ("/lib64/ld-linux-x86-64.so.2", ["a.so", "b.so"]))
        static = elf(())
        self.assertEqual(evidence.elf_linkage(static)[1], [])
        for data in (b"", b"\x7fELF\x01\x01", b"\x7fELF\x02\x02"):
            with self.subTest(data=data), self.assertRaises(ValueError):
                evidence.elf_linkage(data + bytes(64))

    def test_create_failure_skips_export_and_accepts_only_exact_absence(self):
        self.docker.create = (1, b"", b"Error: No such image")
        self.docker.remove = (1, b"", "Error response from daemon: No such container: {name}\n")
        report = self.rejected("export failed")
        self.assertEqual([call[0] for call in self.docker.calls], ["image", "create", "rm"])
        self.assertIsNone(report["container"]["export"])
        self.assertEqual(report["container"]["create"]["stderr"], "Error: No such image")
        self.assertEqual(report["container"]["cleanup"]["status"], "absent")
        self.docker.remove = (1, b"", b"Error response from daemon: No such container: two-bot-inspect-other\n")
        self.assertEqual(self.rejected("cleanup failed")["container"]["cleanup"]["status"], "failed")

    def test_export_failure_or_timeout_still_removes_the_named_container(self):
        self.docker.export = (1, b"", b"Error response from daemon: export refused")
        report = self.rejected("export failed")
        self.assertEqual(report["container"]["export"]["stderr"], "Error response from daemon: export refused")
        self.docker.export = (0, b"", b"")
        self.rejected("export failed")
        self.docker.export = None
        self.docker.timeouts = {"export"}
        report = self.rejected("export failed")
        export = report["container"]["export"]
        self.assertIs(export["timed_out"], True)
        self.assertEqual(export["lossy_decoding"], ["stderr"])
        self.assertEqual(export["stdout_bytes"], len(b"partial"))
        self.assertIn("exceeded 120 seconds", export["stderr"])

    def test_cleanup_failure_or_timeout_is_not_hidden_even_after_a_good_export(self):
        for remove, timeouts in (((1, b"", b"Error response from daemon: removal refused"), set()), (None, {"rm"})):
            with self.subTest(timeouts=timeouts):
                self.docker = FakeDocker(self.image)
                if remove:
                    self.docker.remove = remove
                self.docker.timeouts = timeouts
                report = self.rejected("cleanup failed")
                cleanup = report["container"]["cleanup"]
                self.assertEqual(cleanup["status"], "failed")
                self.assertTrue(report["container"]["name"].startswith("two-bot-inspect-"))

    def test_missing_docker_client_is_explicit_and_cleanup_is_attempted(self):
        self.docker.missing_client = True
        report = self.rejected("cleanup failed")
        self.assertIsNone(report["container"]["create"]["returncode"])
        self.assertIn("docker", report["container"]["create"]["stderr"])
        self.assertIsNone(report["container"]["export"])

    def test_main_fails_after_writing_the_incomplete_report(self):
        self.docker.export = (1, b"", b"refused")
        with scratch() as temporary:
            directory = Path(temporary)
            (directory / "image-id.txt").write_text(IMAGE_ID + "\n")
            (directory / "source-sha.txt").write_text(SOURCE_SHA + "\n")
            with patch.object(evidence, "docker", self.docker), \
                    patch.object(sys, "argv", ["runtime-image-evidence.py", "two-bot:fixture", str(directory)]), \
                    self.assertRaisesRegex(SystemExit, "export failed"):
                evidence.main()
            self.assertIs(json.loads((directory / "runtime-image-evidence.json").read_text())["complete"], False)

    def test_ci_retains_evidence_after_failed_gates_without_changing_gates(self):
        workflow = (ROOT / ".github/workflows/supply-chain.yml").read_text()
        self.assertIn("python3 scripts/test-runtime-image-evidence.py", workflow)
        diagnostic = workflow.index("name: Collect exact-image applicability evidence")
        self.assertGreater(diagnostic, workflow.index("name: Gate runtime image"))
        self.assertLess(diagnostic, workflow.index("name: Retain SBOMs"))
        step = workflow[diagnostic:workflow.index("name: Retain SBOMs")]
        self.assertIn("!cancelled() && steps.inventory.outcome == 'success'", step)
        self.assertIn('python3 scripts/runtime-image-evidence.py "$IMAGE" sbom || status=$?', step)
        self.assertIn("sha256sum runtime-image-evidence.json", step)
        self.assertLess(step.index("sha256sum runtime-image-evidence.json"), step.index('exit "$status"'))
        self.assertEqual(workflow.count("ignore-unfixed: false"), 2)
        self.assertEqual(workflow.count("exit-code: '1'"), 2)


if __name__ == "__main__":
    unittest.main()
