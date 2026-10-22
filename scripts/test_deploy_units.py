"""Lint the shipped deploy/*.service units and exercise the drill's age refusal.

Offline: reads unit files, runs the drill's own ExecStart script under bash
against temporary archives and a stub binary. No host, timer, systemd or
database is touched.
"""
import json
import os
import re
import shlex
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
ON_FAILURE_TARGET = "two-bot-next-failure@%n.service"
ON_FAILURE = f"OnFailure={ON_FAILURE_TARGET}"
EXPECTED_HANDLER_EXECSTART = (
    "/usr/bin/systemd-cat -t two-bot-next-unit-failed -p crit "
    '/usr/bin/echo "unit failed: %i - inspect with journalctl -u %i"'
)
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


def directive_parts(line):
    """(key, value) with systemd whitespace around '=' ignored; None if no '='."""
    if "=" not in line:
        return None
    key, value = line.split("=", 1)
    return key.strip(), value.strip()


def values_for(lines, key):
    """Values for key in order; whitespace around '=' is ignored (systemd.syntax)."""
    values = []
    for line in lines:
        parts = directive_parts(line)
        if parts and parts[0] == key:
            values.append(parts[1])
    return values


def timeout_start_s(lines):
    """Last TimeoutStartSec as seconds; None when unset, 0 for infinity."""
    values = values_for(lines, "TimeoutStartSec")
    if not values:
        return None
    value = values[-1]
    if value == "infinity":
        return 0
    match = re.fullmatch(r"([0-9]+)\s*(s|sec|min|m|h)?", value)
    if not match:
        return 0
    return int(match[1]) * UNIT_SECONDS[match[2] or "s"]


def unquoted(word):
    """Strip one layer of matching systemd quotes; bare words pass through."""
    bare = word.strip()
    if len(bare) >= 2 and bare[0] == bare[-1] and bare[0] in ("'", '"'):
        return bare[1:-1].strip()
    return bare


def effective_onfailure(unit_lines):
    """Effective OnFailure targets; an empty assignment resets the list (systemd.unit)."""
    effective = []
    for value in values_for(unit_lines, "OnFailure"):
        targets = unquoted(value)
        if targets == "":
            effective = []
        else:
            effective.extend(targets.split())
    return effective


ENV_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def effective_environment(service_lines):
    """(values, invalid) honouring empty resets, quoting and last-wins (systemd.exec)."""
    values = {}
    for rest in values_for(service_lines, "Environment"):
        if rest == "":
            values.clear()
            continue
        if "\\" in rest:
            # systemd C-unescapes Environment= (\x53 is S); shlex cannot model that, so fail closed.
            return values, True
        try:
            assignments = shlex.split(rest)
        except ValueError:
            return values, True
        if not assignments:
            values.clear()
            continue
        for assignment in assignments:
            if "=" not in assignment:
                return values, True
            var, val = assignment.split("=", 1)
            if not ENV_NAME.fullmatch(var):
                return values, True
            values[var] = val
    return values, False


EXECSTART_CHAIN = re.compile(r"\s*ExecStart=")


def _trailing_backslashes(line):
    return len(line) - len(line.rstrip("\\"))


def continuation_findings(text, unit_name):
    """Refuse line continuations the lint could join differently than systemd.

    Joining is lexical: a trailing backslash merges the next physical line
    into the current directive before sections or keys are read. systemd
    continues only on an unescaped trailing backslash and skips comment lines
    inside continuations, so a disguised line can hide a whole directive the
    lint never sees although systemd applies it. Only the shipped shape - a
    chain starting at ExecStart=, plain script lines, single trailing
    backslashes - is accepted; everything else fails closed.
    """
    findings = []
    in_chain = False
    for lineno, line in enumerate(text.splitlines(), 1):
        if in_chain:
            stripped = line.strip()
            if (not stripped or stripped.startswith(("#", ";"))
                    or (stripped.startswith("[") and stripped.endswith("]"))):
                findings.append(
                    f"{unit_name}:{lineno}: blank, comment and section lines "
                    f"are not allowed inside a line continuation")
                in_chain = False
            elif line.endswith("\\"):
                if _trailing_backslashes(line) % 2 == 0:
                    findings.append(
                        f"{unit_name}:{lineno}: escaped trailing backslash; "
                        f"write one directive per line")
                    in_chain = False
            else:
                if line.rstrip().endswith("\\"):
                    findings.append(
                        f"{unit_name}:{lineno}: backslash before trailing "
                        f"whitespace; write one directive per line")
                in_chain = False
            continue
        if line.endswith("\\"):
            if _trailing_backslashes(line) % 2 == 0:
                findings.append(
                    f"{unit_name}:{lineno}: escaped trailing backslash; "
                    f"write one directive per line")
            elif not EXECSTART_CHAIN.match(line):
                findings.append(
                    f"{unit_name}:{lineno}: line continuations are only "
                    f"allowed on ExecStart=")
            else:
                in_chain = True
        elif line.rstrip().endswith("\\"):
            findings.append(
                f"{unit_name}:{lineno}: backslash before trailing "
                f"whitespace; write one directive per line")
    if in_chain:
        findings.append(f"{unit_name}: dangling line continuation at end of file")
    return findings


def lint(deploy_dir):
    """One finding per problem, each naming the unit file."""
    findings = []
    sections_by_unit = {}
    units = sorted(Path(deploy_dir).glob("*.service"))
    if not any(unit.name == HANDLER for unit in units):
        findings.append(f"{HANDLER}: failure handler is missing")
    for unit in units:
        # Read bytes so universal-newline translation cannot mask a character.
        # systemd and Python disagree on NUL, BOM and Unicode line boundaries;
        # reject nonprinting text before any continuation or directive parsing.
        try:
            raw = unit.read_bytes().decode("utf-8")
        except UnicodeDecodeError:
            findings.append(f"{unit.name}: unit must be valid UTF-8 text")
            continue
        unsupported = next((char for char in raw
                            if char not in "\t\n" and not char.isprintable()), None)
        if unsupported is not None:
            findings.append(
                f"{unit.name}: unsupported character U+{ord(unsupported):04X}; "
                f"use printable text, tab and LF only")
            continue
        findings.extend(continuation_findings(raw, unit.name))
        sections = parse_sections(raw)
        sections_by_unit[unit.name] = sections
        service_lines = sections.get("Service", [])
        unit_lines = sections.get("Unit", [])
        all_lines = [line for lines in sections.values() for line in lines]
        seconds = timeout_start_s(service_lines)
        if not seconds or seconds > MAX_START_S:
            findings.append(f"{unit.name}: TimeoutStartSec must be finite and <= {MAX_START_S}s in [Service]")
        types = values_for(service_lines, "Type")
        if not types or types[-1] != "oneshot":
            findings.append(f"{unit.name}: Type=oneshot is required in [Service] for the start bound to apply")
        if values_for(service_lines, "TimeoutSec"):
            findings.append(f"{unit.name}: TimeoutSec= overrides TimeoutStartSec; set the start bound explicitly")
        for key in ("SuccessExitStatus", "RestartForceExitStatus"):
            if values_for(service_lines, key):
                findings.append(f"{unit.name}: {key}= must stay unset so failures reach OnFailure=")
        if unit.name != HANDLER and ON_FAILURE_TARGET not in effective_onfailure(unit_lines):
            findings.append(f"{unit.name}: missing {ON_FAILURE} in [Unit]")
        if unit.name == HANDLER:
            if any(unquoted(value) != "" for value in values_for(all_lines, "OnFailure")):
                findings.append(f"{unit.name}: the handler must not name an OnFailure=")
            service_exec = values_for(service_lines, "ExecStart")
            total_exec = values_for(all_lines, "ExecStart")
            if service_exec != [EXPECTED_HANDLER_EXECSTART] or len(total_exec) != 1:
                findings.append(
                    f"{unit.name}: ExecStart must be the documented systemd-cat command in [Service]")
    sections = sections_by_unit.get(DRILL)
    if sections is not None:
        service_lines = sections.get("Service", [])
        env_values, env_invalid = effective_environment(service_lines)
        if env_invalid:
            findings.append(f"{DRILL}: invalid Environment= assignment in [Service]")
        age_text = env_values.get("TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S")
        if age_text is None:
            findings.append(f"{DRILL}: missing the documented max archive age in [Service]")
        else:
            try:
                # Same grammar the drill's case pattern enforces: ASCII digits,
                # no leading zero, at most 9 digits (int() alone accepts 007,
                # +5, 1_000 and Unicode digits, which the drill refuses).
                if not re.fullmatch(r"[1-9][0-9]{0,8}", age_text):
                    raise ValueError(age_text)
                age_value = int(age_text)
            except ValueError:
                findings.append(f"{DRILL}: max archive age must be a positive whole number of seconds")
            else:
                if not 1 <= age_value <= MAX_ARCHIVE_AGE_S:
                    findings.append(
                        f"{DRILL}: max archive age must be 1..{MAX_ARCHIVE_AGE_S}s in [Service]")
        exec_vals = values_for(service_lines, "ExecStart")
        if len(exec_vals) != 1 or "archive_stale" not in exec_vals[0]:
            findings.append(f"{DRILL}: ExecStart must refuse a stale archive (archive_stale)")
        elif exec_vals[0].find("archive_stale") > exec_vals[0].find("restore-drill"):
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
    sections = parse_sections(text)
    exec_vals = values_for(sections.get("Service", []), "ExecStart")
    assert len(exec_vals) == 1, exec_vals
    return systemd_word(f"ExecStart={exec_vals[0]}")


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
        for value in ["infinity", "0", "7h", "banana", "٢h"]:
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
        sections = parse_sections(text)
        service_exec = values_for(sections.get("Service", []), "ExecStart")
        self.assertEqual(service_exec, [EXPECTED_HANDLER_EXECSTART], service_exec)
        total_exec = values_for(
            [line for lines in sections.values() for line in lines], "ExecStart")
        self.assertEqual(len(total_exec), 1, total_exec)

    def test_handler_execstart_section_and_command_are_pinned(self):
        tmp = self.copy_deploy()
        path = tmp / HANDLER
        text = path.read_text()
        exec_line = f"ExecStart={EXPECTED_HANDLER_EXECSTART}\n"
        self.assertIn(exec_line, text)
        moved = text.replace(exec_line, "", 1).replace("[Unit]\n", "[Unit]\n" + exec_line, 1)
        path.write_text(moved)
        self.assertTrue(any(f.startswith(HANDLER + ":") for f in lint(tmp)), lint(tmp))
        tmp = self.copy_deploy()
        path = tmp / HANDLER
        text = path.read_text()
        self.assertIn("/usr/bin/echo", text)
        path.write_text(text.replace("/usr/bin/echo", "/usr/bin/true", 1))
        self.assertTrue(any(f.startswith(HANDLER + ":") for f in lint(tmp)), lint(tmp))

    def test_effective_systemd_values_are_honoured(self):
        tmp = self.copy_deploy()
        path = tmp / "two-bot-next-backup.service"
        text = path.read_text()
        self.assertIn(ON_FAILURE + "\n", text)
        path.write_text(text.replace(ON_FAILURE + "\n", ON_FAILURE + "\nOnFailure=\n", 1))
        self.assertTrue(
            any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)), lint(tmp))
        tmp = self.copy_deploy()
        path = tmp / DRILL
        text = path.read_text()
        anchor = "Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=172800\n"
        self.assertIn(anchor, text)
        path.write_text(text.replace(
            anchor, anchor + 'Environment="TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=999999999"\n', 1))
        self.assertTrue(any(f.startswith(DRILL + ":") for f in lint(tmp)), lint(tmp))

    def test_escaped_environment_names_cannot_hide_an_override(self):
        anchor = "Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=172800\n"
        name = "TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_"
        for forged in [name + "\\x53", name + "\\123", name + "S ", "1" + name + "S"]:
            tmp = self.copy_deploy()
            path = tmp / DRILL
            text = path.read_text()
            path.write_text(text.replace(
                anchor, anchor + 'Environment="' + forged + '=999999999"\n', 1))
            self.assertTrue(any(f.startswith(DRILL + ":") for f in lint(tmp)), (forged, lint(tmp)))

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

    def test_type_oneshot_is_required(self):
        for replacement in ["", "Type=simple\n"]:
            with self.subTest(replacement=replacement or "deleted"):
                tmp = self.copy_deploy()
                path = tmp / "two-bot-next-backup.service"
                text = path.read_text()
                self.assertIn("Type=oneshot\n", text)
                path.write_text(text.replace("Type=oneshot\n", replacement, 1))
                self.assertTrue(
                    any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)), lint(tmp))

    def test_timeout_shorthand_and_success_masks_are_refused(self):
        for unit, line in [
            ("two-bot-next-backup.service", "TimeoutSec=infinity"),
            (DRILL, "TimeoutSec=infinity"),
            (DRILL, "SuccessExitStatus=1"),
            ("two-bot-next-backup.service", "RestartForceExitStatus=1"),
        ]:
            with self.subTest(unit=unit, line=line):
                tmp = self.copy_deploy()
                path = tmp / unit
                path.write_text(path.read_text() + line + "\n")
                self.assertTrue(any(f.startswith(unit + ":") for f in lint(tmp)),
                                (unit, line, lint(tmp)))

    def test_continuation_cannot_hide_a_directive(self):
        # systemd continues only on an unescaped (odd) trailing backslash, so
        # a line ending in "\\" leaves the next line a fresh directive that
        # systemd applies - while a backslash-newline replace joins it into
        # the previous value where values_for() never sees it.
        backup_anchor = "SyslogIdentifier=two-bot-next-backup\n"
        drill_anchor = "Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=172800\n"
        for unit, anchor_line, hidden in [
            ("two-bot-next-backup.service", backup_anchor, "TimeoutSec=infinity"),
            ("two-bot-next-backup.service", backup_anchor, "Type=simple"),
            (DRILL, drill_anchor,
             'Environment="TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=999999999"'),
        ]:
            with self.subTest(unit=unit, hidden=hidden):
                tmp = self.copy_deploy()
                path = tmp / unit
                text = path.read_text()
                self.assertIn(anchor_line, text)
                path.write_text(text.replace(
                    anchor_line,
                    anchor_line + "SyslogIdentifier=backup\\\\\n" + hidden + "\n", 1))
                self.assertTrue(any(f.startswith(unit + ":") for f in lint(tmp)),
                                (unit, hidden, lint(tmp)))

    def test_nonprinting_characters_cannot_hide_directives(self):
        backup = "two-bot-next-backup.service"
        age = "Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=172800\n"
        for unit, anchor, extra in [
            (backup, None, "SyslogIdentifier=x\x00TimeoutSec=infinity\n"),
            (backup, ON_FAILURE + "\n", chr(0xfeff) + "OnFailure=\n"),
            (DRILL, age, chr(0xfeff) + 'Environment="TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=999999999"\n'),
            (backup, None, "SyslogIdentifier=x\x1c[Unit]\nTimeoutSec=infinity\n[Service]\n"),
            (backup, None, "# note\x0cExecStart=/usr/bin/true \\\nSuccessExitStatus=1\n"),
        ]:
            with self.subTest(unit=unit, extra=extra):
                tmp = self.copy_deploy()
                path = tmp / unit
                text = path.read_text()
                if anchor is None:
                    text += extra
                else:
                    self.assertIn(anchor, text)
                    text = text.replace(anchor, anchor + extra, 1)
                path.write_bytes(text.encode("utf-8"))
                findings = lint(tmp)
                self.assertTrue(findings and all(f.startswith(unit + ":") for f in findings), findings)

    def test_only_printable_text_tab_and_lf_are_accepted(self):
        # Include CR as raw bytes: read_text() would silently normalize it to LF.
        forbidden = [chr(code) for code in range(0xa0)
                     if code not in (9, 10) and not chr(code).isprintable()]
        forbidden += [chr(code) for code in (0x00a0, 0x200b, 0x2028, 0x2029, 0xfeff)]
        for unit in ["two-bot-next-backup.service", DRILL, HANDLER]:
            for char in forbidden:
                with self.subTest(unit=unit, code=f"U+{ord(char):04X}"):
                    tmp = self.copy_deploy()
                    path = tmp / unit
                    path.write_bytes(path.read_bytes() + f"# unsupported {char} marker\n".encode("utf-8"))
                    self.assertTrue(any(
                        f.startswith(unit + ":") and f"U+{ord(char):04X}" in f
                        for f in lint(tmp)), lint(tmp))
        tmp = self.copy_deploy()
        for path in tmp.glob("*.service"):
            path.write_bytes(path.read_bytes() + "# Déploiement 🙂\twith LF\n".encode("utf-8"))
        self.assertEqual(lint(tmp), [])

    def test_invalid_utf8_is_a_named_finding(self):
        for unit in ["two-bot-next-backup.service", DRILL, HANDLER]:
            with self.subTest(unit=unit):
                tmp = self.copy_deploy()
                path = tmp / unit
                path.write_bytes(path.read_bytes() + b"# invalid \xff\n")
                findings = lint(tmp)
                self.assertTrue(findings and all(f.startswith(unit + ":") for f in findings), findings)
                self.assertTrue(any("UTF-8" in f for f in findings), findings)

    def test_escaped_backslash_inside_drill_continuation_is_refused(self):
        # Pin the in-chain parity diagnostic, not just any lint failure: the
        # broken script can also trip the independent age-order check.
        anchor = '  exec /opt/two-bot-next/two-bot restore-drill "$latest" --confirm-scratch\'\n'
        for count in [2, 4]:
            for hidden in ["SuccessExitStatus=1", "TimeoutSec=infinity"]:
                with self.subTest(backslashes=count, hidden=hidden):
                    tmp = self.copy_deploy()
                    path = tmp / DRILL
                    text = path.read_text()
                    self.assertIn(anchor, text)
                    extra = "  echo marker; " + "\\" * count + "\n" + hidden + "\n"
                    path.write_text(text.replace(anchor, extra + anchor, 1))
                    findings = lint(tmp)
                    self.assertTrue(any(
                        f.startswith(DRILL + ":") and "escaped trailing backslash" in f
                        for f in findings), findings)

    def test_continuation_cannot_reset_the_failure_hook_behind_a_comment(self):
        # systemd skips comment lines before joining continuations, so this
        # resets OnFailure= while the naive join still sees the shipped target.
        tmp = self.copy_deploy()
        path = tmp / "two-bot-next-backup.service"
        text = path.read_text()
        self.assertIn(ON_FAILURE + "\n", text)
        path.write_text(text.replace(
            ON_FAILURE + "\n",
            ON_FAILURE + "\nOnFailure=\\\n# rotated by the night shift\n\n", 1))
        self.assertTrue(
            any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)), lint(tmp))

    def test_ambiguous_continuation_endings_are_refused(self):
        # A backslash before trailing whitespace, an escaped (even) trailing
        # backslash, and a continued comment are all refused: the lint must
        # never guess how systemd joins the line.
        for extra in ["OnFailure=\\  \n", "# auditors were here\\\n",
                      "SyslogIdentifier=x\\\\\n", "OnFailure=\\"]:
            with self.subTest(extra=extra):
                tmp = self.copy_deploy()
                path = tmp / "two-bot-next-backup.service"
                path.write_text(path.read_text() + extra)
                self.assertTrue(
                    any(f.startswith("two-bot-next-backup.service:") for f in lint(tmp)),
                    (extra, lint(tmp)))

    def test_comment_inside_the_drill_script_continuation_is_refused(self):
        tmp = self.copy_deploy()
        path = tmp / DRILL
        text = path.read_text()
        anchor = "  max=$TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S; \\\n"
        self.assertIn(anchor, text)
        path.write_text(text.replace(anchor, anchor + "# evil\n", 1))
        self.assertTrue(any(f.startswith(DRILL + ":") for f in lint(tmp)), lint(tmp))

    def test_shipped_max_archive_age_is_pinned_and_capped(self):
        unit = (DEPLOY / DRILL).read_text()
        shipped = int(re.search(
            r"^Environment=TWO_RESTORE_DRILL_MAX_ARCHIVE_AGE_S=(\d+)$", unit, re.M)[1])
        self.assertEqual(shipped, SHIPPED_MAX_ARCHIVE_AGE_S)
        for value in ["0", "999999999", "007", "0172800", "+5", "1_000", "١٧٢٨٠٠"]:
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
