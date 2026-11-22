"""Publish immutable Actions links and validate test declarations on a PR head."""

import ast
import hashlib
import html
import os
from pathlib import Path
import re
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from pr_standards import parse_sections

SHA = re.compile(r"[0-9a-f]{40}")
NUMBER = re.compile(r"[1-9][0-9]*")
RUN_LINK = re.compile(r"\bgithub\.com/[^/\s]+/[^/\s]+/actions/runs/", re.I)
CUBIC_FOOTER = re.compile(
    r'\n<!-- This is an auto-generated description by cubic\. -->\n'
    r'<a href="https://cubic\.dev/pr/[^\n]+\n<!-- End of auto-generated description by cubic\. -->\s*\Z')
MAX_SOURCE = 2 * 1024 * 1024


class EvidenceError(ValueError):
    """A refusal safe to expose in Actions."""


def command(*args):
    return subprocess.check_output(args, stderr=subprocess.DEVNULL, timeout=30).decode()


def publish(repo, event, head, run, attempt, workflow):
    prefix = f"repos/{repo}/actions/runs/{run}"
    identity = command("gh", "api", f"{prefix}/attempts/{attempt}", "--jq",
                       "[.head_sha,.event,.path,.id,.run_attempt] | @tsv").strip().split("\t")
    if identity != [head, event, f".github/workflows/{workflow}", run, attempt]:
        raise EvidenceError("Actions run does not match the event head, workflow and attempt.")
    rows = command("gh", "api", "--paginate", f"{prefix}/attempts/{attempt}/jobs?per_page=100",
                   "--jq", ".jobs[] | [.id,.head_sha,.run_id,.run_attempt,"
                   " (.conclusion // .status),.name] | @tsv").splitlines()
    if not rows:
        raise EvidenceError("Actions returned no jobs for this attempt.")
    url = f"https://github.com/{repo}/actions/runs/{run}"
    summary = [f"## Actions evidence for `{head}`", "",
               f"[Workflow run, attempt {attempt}]({url}/attempts/{attempt})", ""]
    for row in rows:
        job, job_head, job_run, job_attempt, status, name = row.split("\t", 5)
        if (not NUMBER.fullmatch(job) or (job_head, job_run) != (head, run)
                or not NUMBER.fullmatch(job_attempt) or int(job_attempt) > int(attempt)
                or not re.fullmatch(r"[a-z_]+", status)):
            raise EvidenceError("Actions returned a job outside this head or attempt.")
        label = html.escape(name).replace("[", "&#91;").replace("]", "&#93;")
        summary.append(f"- [{label}]({url}/job/{job}) ({status}, attempt {job_attempt})")
    return "\n".join(summary) + "\n"


def evidence_sections(body):
    sections = parse_sections(CUBIC_FOOTER.sub("", body))
    return tuple(sections.get(name, "") for name in ("verification", "attestation"))


def sources(base, head):
    diff = command("git", "diff", "--no-ext-diff", "--no-renames", "--ignore-submodules=none",
                   "--name-status", "-z", f"{base}...{head}", "--").rstrip("\0").split("\0")
    if len(diff) < 2 or len(diff) % 2:
        raise EvidenceError("Unable to establish the changed-file inventory.")
    kinds = {"A": "added", "M": "modified", "D": "removed", "T": "modified"}
    return [{"filename": path, "status": kinds[status]}
            for status, path in zip(diff[::2], diff[1::2], strict=True)]


def source_blob(head, path):
    row = command("git", "ls-tree", "-z", head, "--", f":(literal){path}")
    if not row.startswith(("100644 blob ", "100755 blob ")):
        raise EvidenceError("An attested test path is not a source file on this head.")
    obj = row.split("\t", 1)[0].split()[2]
    if int(command("git", "cat-file", "-s", obj)) > MAX_SOURCE:
        raise EvidenceError("An attested source exceeds the size limit.")
    return command("git", "cat-file", "blob", obj)


def named_tests(path, source):
    suffix = Path(path).suffix
    if suffix == ".py":
        functions = (ast.FunctionDef, ast.AsyncFunctionDef)
        for node in ast.parse(source).body:
            if isinstance(node, functions) and node.name.startswith("test_"):
                yield node.name
            if isinstance(node, ast.ClassDef) and (
                    node.name.startswith("Test") or node.name.endswith("Tests") or any(
                        ast.unparse(base).endswith("TestCase") for base in node.bases)):
                for method in node.body:
                    if isinstance(method, functions) and method.name.startswith("test_"):
                        yield f"{node.name}.{method.name}"
        return
    if suffix not in {".rs", ".ts", ".js", ".mjs", ".cjs"}:
        return

    from tree_sitter import Language, Parser

    if suffix == ".rs":
        import tree_sitter_rust
        language = tree_sitter_rust.language()
    else:
        import tree_sitter_typescript
        language = tree_sitter_typescript.language_typescript()
    root = Parser(Language(language)).parse(source.encode()).root_node
    if root.has_error:
        raise EvidenceError("An attested source could not be parsed.")
    nodes = [(root, ())]
    while nodes:
        node, modules = nodes.pop()
        if suffix == ".rs":
            if node.type == "mod_item":
                modules = (*modules, node.child_by_field_name("name").text.decode())
            if node.type in {"source_file", "mod_item", "declaration_list"}:
                nodes.extend((child, modules) for child in node.named_children)
            if node.type == "function_item":
                attribute = node.prev_named_sibling
                while attribute and attribute.type in {"attribute_item", "line_comment", "block_comment"}:
                    if re.match(rb"#\s*\[\s*(?:\w+\s*::\s*)*test\s*[\[(\]]", attribute.text):
                        yield "::".join((*modules, node.child_by_field_name("name").text.decode()))
                        break
                    attribute = attribute.prev_named_sibling
        else:
            nodes.extend((child, ()) for child in node.named_children)
            if node.type == "call_expression" and node.child_by_field_name("function").text in {
                    b"test", b"it", b"t.test"}:
                args = node.child_by_field_name("arguments").named_children
                if len(args) > 1 and args[0].type in {"string", "template_string"}:
                    if not any(child.type == "template_substitution" for child in args[0].named_children):
                        yield re.sub(r"""\\(['"`\\])""", r"\1", args[0].text[1:-1].decode())


def validate(policy, body, files, read):
    verification, attestation = evidence_sections(body)
    if (policy.visible_lines(body).count("## Verification") != 1 or not any(
            policy.meaningful(line.strip("-* ~`")) for line in verification.splitlines())):
        raise EvidenceError("Fill in the visible Verification section with commands and results.")
    if RUN_LINK.search(verification + "\n" + attestation):
        raise EvidenceError("Actions URLs in Verification or Attestation can go stale. Use the job summary.")
    report = policy.validate(CUBIC_FOOTER.sub("", body), files)
    if report["issues"]:
        raise EvidenceError("Attestation issues: " + ", ".join(report["issues"]))
    titles = {}
    tests = [line for line in policy.visible_lines(attestation) if policy.TEST.fullmatch(line)]
    for index, line in enumerate(tests, start=1):
        path, _, selector = policy.TEST.fullmatch(line)[2].strip().partition("::")
        if not selector:
            read(path)
            continue
        if path not in titles:
            titles[path] = list(named_tests(path, read(path)))
        names, suffix = titles[path], Path(path).suffix
        if "::" not in selector and suffix == ".rs":
            names = [name.rsplit("::", 1)[-1] for name in names]
        if names.count(selector.replace("::", ".") if suffix == ".py" else selector) != 1:
            raise EvidenceError(f"Attestation test entry {index} has no unique matching test on this head.")


def check(policy, reader, number, repo, base, head):
    def snapshot():
        pr = reader.get(f"pulls/{number}")
        if (pr["state"], pr["base"]["repo"]["full_name"], pr["base"]["sha"], pr["head"]["sha"]) != (
                "open", repo, base, head):
            raise EvidenceError("PR identity moved or does not match this run.")
        return pr["body"] or "", pr["head"]["ref"], pr["user"]["login"]

    body, branch, author = snapshot()
    baseline = Path(__file__).with_name("pr-evidence-baseline.txt").read_text().splitlines()
    digest = hashlib.sha256(repr(evidence_sections(body)).encode()).hexdigest()
    entry = f"{number} {head} {digest}"
    if branch.startswith("release-please--") and author.endswith("[bot]"):
        result = "Generated release PR"
    elif entry in baseline:
        result = "Existing PR with grandfathered head and evidence"
    else:
        validate(policy, body, sources(base, head), lambda path: source_blob(head, path))
        result = "Verification and Attestation passed"
    if snapshot() != (body, branch, author):
        raise EvidenceError("PR evidence changed during validation. Use the newer run.")
    return result


def main():
    env = os.environ
    head, run, attempt = (env[key] for key in ("EVIDENCE_HEAD", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"))
    repo, event = env["GITHUB_REPOSITORY"], env["GITHUB_EVENT_NAME"]
    if (not SHA.fullmatch(head) or not all(map(NUMBER.fullmatch, (run, attempt)))
            or not re.fullmatch(r"[\w.-]+/[\w.-]+", repo, re.ASCII)):
        raise EvidenceError("Invalid repository identity.")
    mode = sys.argv[1]
    workflow = {"verify": "pr-evidence.yml", "publish": "check.yml"}[mode]
    with open(env["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
        output.write(publish(repo, event, head, run, attempt, workflow))
        if mode == "verify" and event == "pull_request":
            sys.path.insert(0, str(Path("attestation-policy/.github/scripts").resolve()))
            import pr_attestation as policy

            number, base = env["PR_NUMBER"], env["PR_BASE"]
            if (not NUMBER.fullmatch(number) or not SHA.fullmatch(base)
                    or command("git", "rev-parse", "--verify", f"{head}^{{commit}}").strip() != head):
                raise EvidenceError("Missing immutable PR head identity.")
            reader = policy.GitHub(env["GH_TOKEN"], repo)
            try:
                result = check(policy, reader, number, repo, base, head)
            except policy.ObservationError:
                raise EvidenceError("Attestation policy or metadata request refused.") from None
            output.write(f"\n**{result}.**\n")


if __name__ == "__main__":
    try:
        main()
    except EvidenceError as error:
        raise SystemExit(str(error)) from None
    except (KeyError, OSError, SyntaxError, ValueError, subprocess.SubprocessError):
        raise SystemExit("Could not read the exact-head evidence or attested source.") from None
