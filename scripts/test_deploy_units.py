"""Lint the shipped deploy/*.service units and exercise the drill's age refusal.

Offline: reads unit files, runs the drill's own ExecStart script under bash
against temporary archives and a stub binary. No host, timer, systemd or
database is touched.
"""
import json
import os
import re
import shutil
import stat
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEPLOY = ROOT / "deploy"
HANDLER = "two-bot-next-failure@.service"
ON_FAILURE = "OnFailure=two-bot-next-failure@%n.service"
DRILL = "two-bot-next-restore-drill.service"
UNIT_SECONDS = {"s": 1, "sec": 1, "min": 60, "m": 60, "h": 3600}
MAX_START_S = 6 * 3600
MAX_ARCHIVE_AGE_S = 3 * 86400
SHIPPED_MAX_ARCHIVE_AGE_S = 172800


def directives(text):
    """Directive lines after systemd joins backslash-continued lines."""
    joined = text.replace("\\\n", " ")
    return [line.strip() for line in joined.splitlines()
            if line.strip() and not line.lstrip().startswith(("#", ";"))]


def parse_sections(text):
    """Map section name to directive lines; systemd ignores misplaced directives."""
    joined = text.replace("\\\n", " ")
    sections = {}
    current = None
    for raw in joined.splitlines():
        line = raw.strip()
        if not line or line.startswith(("#", ";")):
            continue
        if line.startswith("[") and line.endswith("]"):
            current = line[1:-1]
            sections.setdefault(current, [])
            continue
        if current is None:
            continue
        sections[current].append(line)
    return sections


def timeout_start_s(lines):
    """Last TimeoutStartSec as seconds; None when unset, 0 for infinity."""
    value = None
    for line in lines:
        if line.startswith("TimeoutStartSec="):
            value = line.split("=", 1)[1].strip()
    if value is None:
        return None
    if value == "infinity":
        return 0
    match = re.fullmatch(r"(\d+)\s*(s|sec|min|m|h)?", value)
    if not match:
        return 0
    return int(match[1]) * UNIT_SECONDS[match[2] or "s"]


def lint(deploy_dir):
    """One finding per problem, each naming the unit file."""
    findings = []
    units = sorted(Path(deploy_dir).glob("*.service"))
    if not any(unit.name == HANDLER for unit in units):
        findings.append(f"{HANDLER}: failure handler is missing")
    for unit in units:
        sections = parse_sections(unit.read_text())
        service_lines = sections.get("Service", [])
        unit_lines = sections.get("Unit", [])
        all_lines = [line for lines in sections.values() for line in lines]
        seconds = timeout_start_s(service_lines)
        if not seconds or seconds > MAX_START_S:
            findings.append(f"{unit.name}: TimeoutStartSec must be finite and <= {MAX_START_S}s in [Service]")
        if unit.name != HANDLER and ON_FAILURE not in unit_lines:
            findings.append(f"{unit.name}: missing {ON_FAILURE} in [Unit]")
        if unit.name == HANDLER and any(line.startswith("OnFailure=") for line in all_lines):
            findings.append(f"{unit.name}: the handler must not name an OnFailure=")
    drill = Path(deploy_dir) / DRILL
    if drill.exists():
        sections = parse_sections(drill.read_text())
        service_lines = sections.get("Service", [])
        age_lines = [line for line in service_lines
                     if line.startswith("Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=")]
        if not age_lines:
            findings.append(f"{DRILL}: missing the documented max archive age in [Service]")
        else:
            try:
                age_value = int(age_lines[-1].split("=", 2)[2])
            except (IndexError, ValueError):
                findings.append(f"{DRILL}: max archive age must be a positive whole number of seconds")
            else:
                if not 1 <= age_value <= MAX_ARCHIVE_AGE_S:
                    findings.append(
                        f"{DRILL}: max archive age must be 1..{MAX_ARCHIVE_AGE_S}s in [Service]")
        exec_lines = [line for line in service_lines if line.startswith("ExecStart=")]
        if len(exec_lines) != 1 or "archive_stale" not in exec_lines[0]:
            findings.append(f"{DRILL}: ExecStart must refuse a stale archive (archive_stale)")
        elif exec_lines[0].find("archive_stale") > exec_lines[0].find("restore-drill"):
            findings.append(f"{DRILL}: the age refusal must come before provisioning")
    return findings


def systemd_word(exec_line):
    """The single -c script systemd would hand to bash for the drill ExecStart."""
    prefix = "ExecStart=/usr/bin/bash -o pipefail -c '"
    if not (exec_line.startswith(prefix) and exec_line.endswith("'")):
        raise ValueError("drill must run one single-quoted bash -o pipefail script")
    raw = exec_line[len(prefix):-1]
    out, i = [], 0
    while i < len(raw):
        char, nxt = raw[i], raw[i + 1:i + 2]
        if char == "'":
            raise ValueError("a single quote would end the systemd word")
        if char == "\\":
            if nxt not in ('\\', '"', "n"):
                raise ValueError(f"unsupported systemd escape \\{nxt}")
            out.append({"\\": "\\", '"': '"', "n": "\n"}[nxt])
            i += 2
        elif char == "%":
            if nxt != "%":
                raise ValueError("a lone % is a systemd specifier; write %%")
            out.append("%")
            i += 2
        else:
            out.append(char)
            i += 1
    script = "".join(out)
    if "${" in script:
        raise ValueError("${ is substituted by systemd; do not use it")
    return script


def drill_script(text):
    lines = [line for line in directives(text) if line.startswith("ExecStart=")]
    assert len(lines) == 1, lines
    return systemd_word(lines[0])


class UnitLintTests(unittest.TestCase):
    def copy_deploy(self):
        tmp = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, tmp)
        for unit in DEPLOY.glob("*.service"):
            shutil.copy(unit, tmp)
        return Path(tmp)

    def test_shipped_units_pass(self):
        self.assertEqual(lint(DEPLOY), [])

    def test_every_service_is_linted(self):
        names = {unit.name for unit in DEPLOY.glob("*.service")}
        self.assertEqual(names, {
            "two-bot-next-backup.service", "two-bot-next-guild-config-backup.service",
            DRILL, HANDLER,
        }, "a new unit needs its bounds and failure hook reviewed")

    def test_no_timer_wants_the_handler(self):
        for timer in DEPLOY.glob("*.timer"):
            self.assertNotIn("failure", timer.read_text())

    def test_deleting_a_directive_names_the_unit(self):
        for unit, directive in [
            ("two-bot-next-backup.service", "TimeoutStartSec=2h"),
            ("two-bot-next-guild-config-backup.service", "TimeoutStartSec=2h"),
            (DRILL, "TimeoutStartSec=2h"),
            ("two-bot-next-backup.service", ON_FAILURE),
            ("two-bot-next-guild-config-backup.service", ON_FAILURE),
            (DRILL, ON_FAILURE),
            (HANDLER, "TimeoutStartSec=30s"),
        ]:
            with self.subTest(unit=unit, directive=directive):
                tmp = self.copy_deploy()
                path = tmp / unit
                text = path.read_text()
                self.assertIn(directive, text)
                path.write_text(text.replace(directive + "\n", "", 1))
                findings = lint(tmp)
                self.assertTrue(findings and all(f.startswith(unit + ":") for f in findings), findings)

    def test_infinite_or_huge_timeout_is_refused(self):
        for value in ["infinity", "0", "7h", "banana"]:
            with self.subTest(value=value):
                tmp = self.copy_deploy()
                path = tmp / "two-bot-next-backup.service"
                path.write_text(path.read_text().replace("TimeoutStartSec=2h", f"TimeoutStartSec={value}"))
                self.assertTrue(any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)))

    def test_missing_handler_and_dropped_age_check_are_named(self):
        tmp = self.copy_deploy()
        (tmp / HANDLER).unlink()
        self.assertTrue(any(f.startswith(HANDLER) for f in lint(tmp)))
        tmp = self.copy_deploy()
        path = tmp / DRILL
        path.write_text(path.read_text().replace("archive_stale", "archive_old"))
        self.assertTrue(any(f.startswith(DRILL + ":") for f in lint(tmp)))

    def test_handler_only_logs(self):
        text = (DEPLOY / HANDLER).read_text()
        for forbidden in ["EnvironmentFile", "LoadCredential", "DATABASE", "TOKEN", "curl", "two-bot "]:
            self.assertNotIn(forbidden, "\n".join(directives(text)))
        exec_lines = [line for line in directives(text) if line.startswith("ExecStart=")]
        self.assertEqual(len(exec_lines), 1, exec_lines)
        handler_cmd = exec_lines[0]
        for required in ["systemd-cat", "-t two-bot-next-unit-failed", "-p crit", "%i"]:
            self.assertIn(required, handler_cmd, handler_cmd)

    def test_misplaced_directive_is_refused(self):
        tmp = self.copy_deploy()
        path = tmp / "two-bot-next-backup.service"
        text = path.read_text()
        moved = text.replace("TimeoutStartSec=2h\n", "", 1).replace(
            "[Unit]\n", "[Unit]\nTimeoutStartSec=2h\n", 1)
        path.write_text(moved)
        self.assertTrue(
            any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)), lint(tmp))
        tmp = self.copy_deploy()
        path = tmp / "two-bot-next-backup.service"
        text = path.read_text()
        moved = text.replace(ON_FAILURE + "\n", "", 1) + ON_FAILURE + "\n"
        path.write_text(moved)
        self.assertTrue(
            any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)), lint(tmp))

    def test_shipped_max_archive_age_is_pinned_and_capped(self):
        unit = (DEPLOY / DRILL).read_text()
        shipped = int(re.search(
            r"^Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=(\d+)$", unit, re.M)[1])
        self.assertEqual(shipped, SHIPPED_MAX_ARCHIVE_AGE_S)
        for value in ["0", "999999999"]:
            with self.subTest(value=value):
                tmp = self.copy_deploy()
                path = tmp / DRILL
                path.write_text(path.read_text().replace(
                    f"TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S={shipped}",
                    f"TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S={value}"))
                self.assertTrue(any(f.startswith(DRILL + ":") for f in lint(tmp)), lint(tmp))


class DrillAgeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.archives = self.tmp / "archives"
        self.archives.mkdir()
        self.calls = self.tmp / "calls"
        self.binary = self.tmp / "two-bot"
        self.binary.write_text(f'#!/bin/sh\necho "$@" >> "{self.calls}"\n')
        self.binary.chmod(self.binary.stat().st_mode | stat.S_IXUSR)
        script = drill_script((DEPLOY / DRILL).read_text())
        for shipped in ["/var/backups/two-bot-next/", "/opt/two-bot-next/two-bot "]:
            self.assertEqual(script.count(shipped), 1, shipped)
        self.script = (script.replace("/var/backups/two-bot-next/", f"{self.archives}/")
                       .replace("/opt/two-bot-next/two-bot ", f"{self.binary} "))
        unit = (DEPLOY / DRILL).read_text()
        self.max_age = int(re.search(
            r"^Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=(\d+)$", unit, re.M)[1])

    def archive(self, name, age_s):
        path = self.archives / name
        path.write_bytes(b"not a real archive")
        stamp = time.time() - age_s
        os.utime(path, (stamp, stamp))
        return path

    def run_drill(self, max_age="shipped"):
        env = {"PATH": "/usr/bin:/bin"}
        if max_age == "shipped":
            max_age = str(self.max_age)
        if max_age is not None:
            env["TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S"] = max_age
        return subprocess.run(["/usr/bin/bash", "-o", "pipefail", "-c", self.script],
                              env=env, capture_output=True, text=True, timeout=30)

    def verdict(self, result):
        lines = [line for line in result.stdout.splitlines() if line.startswith("{")]
        self.assertEqual(len(lines), 1, result.stdout)
        verdict = json.loads(lines[0])
        self.assertEqual(set(verdict), {"verdict", "code", "archive_age_s"})
        return verdict

    def assert_refused(self, result, code):
        verdict = self.verdict(result)
        self.assertEqual((result.returncode, verdict["verdict"], verdict["code"]), (1, "refuse", code), result)
        self.assertIn("drill: refused:", result.stderr)
        self.assertFalse(self.calls.exists(), "no restore (no database) may start on a refusal")
        return verdict

    def test_fresh_archive_passes_and_restores(self):
        self.archive("two-funnel-20261020T041700Z.ndjson.gz", 3600)
        result = self.run_drill()
        verdict = self.verdict(result)
        self.assertEqual(result.returncode, 0, result)
        self.assertEqual((verdict["verdict"], verdict["code"]), ("pass", "archive_fresh"))
        self.assertTrue(3590 <= verdict["archive_age_s"] <= 3700, verdict)
        self.assertEqual(len(self.calls.read_text().splitlines()), 1)
        self.assertIn("restore-drill", self.calls.read_text())
        self.assertIn("--confirm-scratch", self.calls.read_text())

    def test_stale_archive_refuses_with_code_and_next_action(self):
        self.archive("two-funnel-20260101T041700Z.ndjson.gz", self.max_age + 3600)
        result = self.run_drill()
        verdict = self.assert_refused(result, "archive_stale")
        self.assertGreater(verdict["archive_age_s"], self.max_age)
        self.assertIn("journalctl -u two-bot-next-backup", result.stderr)

    def test_newest_archive_decides_not_an_older_fresh_one(self):
        self.archive("two-funnel-old.ndjson.gz", 40 * 86400)
        self.archive("two-funnel-new.ndjson.gz", 600)
        self.assertEqual(self.run_drill().returncode, 0)

    def test_missing_archive_refuses(self):
        (self.archives / "unrelated.txt").write_text("x")
        verdict = self.assert_refused(self.run_drill(), "archive_missing")
        self.assertIsNone(verdict["archive_age_s"])

    def test_unreadable_archive_refuses(self):
        path = self.archive("two-funnel-unreadable.ndjson.gz", 60)
        path.chmod(0)
        probe = subprocess.run(["/usr/bin/head", "-c", "1", str(path)],
                               capture_output=True, timeout=10)
        if probe.returncode == 0:
            self.skipTest("unreadable fixture needs a non-root user")
        verdict = self.assert_refused(self.run_drill(), "archive_unreadable")
        self.assertIsNone(verdict["archive_age_s"])

    def test_future_archive_refuses(self):
        self.archive("two-funnel-future.ndjson.gz", -7200)
        verdict = self.assert_refused(self.run_drill(), "archive_from_future")
        self.assertLess(verdict["archive_age_s"], 0)

    def test_unusable_limit_refuses_before_looking_at_archives(self):
        self.archive("two-funnel-fresh.ndjson.gz", 60)
        for value in [None, "", "0", "007", "-5", "1d", "12.5", "1234567890", "$(touch x)"]:
            with self.subTest(value=value):
                verdict = self.assert_refused(self.run_drill(max_age=value), "max_age_invalid")
                self.assertIsNone(verdict["archive_age_s"])

    def test_boundary_is_inclusive(self):
        self.archive("two-funnel-edge.ndjson.gz", 100)
        self.assertEqual(self.run_drill(max_age="1000").returncode, 0)
        self.calls.unlink()
        self.assert_refused(self.run_drill(max_age="50"), "archive_stale")

    def test_idempotent_and_leaks_no_path_or_secret(self):
        archive = self.archive("two-funnel-20260101T041700Z.ndjson.gz", self.max_age + 10)
        first, second = self.run_drill(), self.run_drill()
        self.assertEqual(self.verdict(first)["code"], self.verdict(second)["code"])
        self.assertEqual((first.returncode, second.returncode), (1, 1))
        self.assertEqual(sorted(p.name for p in self.archives.iterdir()), [archive.name])
        for text in (first.stdout, first.stderr):
            self.assertNotIn(str(self.archives), text)
            self.assertNotIn(archive.name, text)

    def test_script_survives_systemd_parsing_rules(self):
        # systemd_word raises on a lone %, a stray escape, a quote or "${".
        for bad in ["ExecStart=/usr/bin/bash -o pipefail -c 'date +%s'",
                    "ExecStart=/usr/bin/bash -o pipefail -c 'echo \\x'",
                    "ExecStart=/usr/bin/bash -o pipefail -c 'echo ${HOME}'"]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                systemd_word(bad)


if __name__ == "__main__":
    unittest.main()
