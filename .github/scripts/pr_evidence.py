import ast
from dataclasses import dataclass
import hashlib
import html
import os
from pathlib import Path
import re
import subprocess
import sys

SHA = re.compile(r"[0-9a-f]{40}")
NUMBER = re.compile(r"[1-9][0-9]*")
REPOSITORY = re.compile(r"[A-Za-z0-9-]+/[A-Za-z0-9_.-]+")
RUN_LINK = re.compile(r"\bgithub\.com/[^/\s]+/[^/\s]+/actions/runs/", re.I)
CUBIC_FOOTER = re.compile(
    r'\n<!-- This is an auto-generated description by cubic\. -->\n'
    r'<a href="https://cubic\.dev/pr/[^\n]+\n<!-- End of auto-generated description by cubic\. -->\s*\Z')
MAX_SOURCE = 2 * 1024 * 1024


class EvidenceError(ValueError):
    """A refusal safe to expose in Actions."""


@dataclass(frozen=True)
class EvidenceVerdict:
    """A passing validation for one PR snapshot, issued by a trusted verifier."""

    repo: str
    number: int
    base: str
    head: str
    description_sha256: str

    def matches_current(self, reader, number, repo, base, head):
        """Compare a trusted verdict with a fresh read of the intended PR."""
        try:
            current, _ = current_snapshot(reader, number, repo, base, head)
        except EvidenceError:
            return False
        return self == current


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


def evidence_sections(policy, body):
    from markdown_it import MarkdownIt
    body = CUBIC_FOOTER.sub("", body).replace("\r\n", "\n").replace("\r", "\n")
    lines, tokens = body.split("\n"), MarkdownIt().parse(body)
    headings = [(token.map, tokens[index + 1].children) for index, token in enumerate(tokens)
                if token.type == "heading_open" and token.level == 0 and token.tag in {"h1", "h2"}]
    stops = [span[0] for span, _ in headings[1:]] + [len(lines)]
    sections = {}
    for ((start, content), children), stop in zip(headings, stops):
        label = "".join(child.content for child in children if child.type in {"text", "code_inline"})
        title = "## " + label.strip().title()
        if title not in {"## Verification", "## Attestation"}:
            continue
        if lines[start].strip() != title or title in sections:
            raise EvidenceError("Use unique canonical Verification and Attestation headings.")
        sections[title] = "\n".join(lines[content:stop]).strip("\n")
    visible = "\n".join(policy.visible_lines(body))
    tail = f"\n{visible}\n".partition("\n## Attestation\n")[2]
    return sections.get("## Verification"), sections.get("## Attestation"), re.split(r"^#{1,2} ", tail, maxsplit=1, flags=re.M)[0]


def sources(base, head):
    diff = command("git", "diff", "--no-ext-diff", "--no-renames", "--ignore-submodules=none",
                   "--name-status", "-z", f"{base}...{head}", "--").rstrip("\0").split("\0")
    kinds = {"A": "added", "M": "modified", "D": "removed", "T": "modified"}
    return [{"filename": path, "status": kinds[status]}
            for status, path in zip(diff[::2], diff[1::2], strict=True)]


def source_blob(head, path):
    row = command("git", "ls-tree", "-l", "-z", head, "--", f":(literal){path}")
    mode, kind, obj, size = row.split("\t", 1)[0].split()
    if mode not in {"100644", "100755"} or kind != "blob" or int(size) > MAX_SOURCE:
        raise EvidenceError("An attested source must be a regular file within the size limit.")
    return command("git", "cat-file", "blob", obj)


def named_tests(path, source):
    suffix = Path(path).suffix
    if suffix == ".py":
        functions = (ast.FunctionDef, ast.AsyncFunctionDef)
        test_cases = {"TestCase", "IsolatedAsyncioTestCase"}
        for node in ast.parse(source).body:
            if isinstance(node, functions) and node.name.startswith("test_"):
                yield node.name
            if isinstance(node, ast.ClassDef) and (
                    any(ast.unparse(base).rsplit(".", 1)[-1] in test_cases for base in node.bases)):
                test_cases.add(node.name)
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
                    if re.match(rb"#\s*\[\s*(?:\w+\s*::\s*)*test\s*(?:\(|\])", attribute.text):
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
    verification, attestation, policy_attestation = evidence_sections(policy, body)
    if attestation is None or not any(
            policy.meaningful(line.strip("-* ")) for line in policy.visible_lines(verification or "")):
        raise EvidenceError("Fill in Verification with visible commands and results and include Attestation.")
    if any(RUN_LINK.search(section) for section in (verification, attestation, policy_attestation)):
        raise EvidenceError("Actions URLs in Verification or Attestation can go stale. Use the job summary.")
    report = policy.validate(CUBIC_FOOTER.sub("", body), files)
    if report["issues"]:
        raise EvidenceError("Attestation issues: " + ", ".join(report["issues"]))
    titles = {}
    tests = filter(None, map(policy.TEST.fullmatch, policy_attestation.splitlines()))
    for index, test in enumerate(tests, start=1):
        path, _, selector = test[2].strip().partition("::")
        if not selector:
            read(path)
            continue
        if path not in titles:
            titles[path] = list(named_tests(path, read(path)))
        names, suffix = titles[path], Path(path).suffix
        separator = {".py": ".", ".rs": "::"}.get(suffix)
        selector = selector.replace("::", ".") if suffix == ".py" else selector
        if separator and separator not in selector:
            names = [name.rsplit(separator, 1)[-1] for name in names]
        if names.count(selector) != 1:
            raise EvidenceError(f"Attestation test entry {index} has no unique matching test on this head.")


def current_snapshot(reader, number, repo, base, head):
    if (type(number) is not int or number < 1
            or not isinstance(repo, str) or not REPOSITORY.fullmatch(repo)
            or not all(isinstance(sha, str) and SHA.fullmatch(sha) for sha in (base, head))):
        raise EvidenceError("Invalid PR identity.")
    try:
        pr = reader.get(f"pulls/{number}")
        identity = (pr["number"], pr["state"], pr["base"]["repo"]["full_name"],
                    pr["base"]["sha"], pr["head"]["sha"])
        body = pr["body"]
    except Exception:
        raise EvidenceError("Could not read current PR metadata.") from None
    if type(pr["number"]) is not int or identity != (number, "open", repo, base, head):
        raise EvidenceError("PR identity moved or does not match this run.")
    if body is None:
        body = ""
    if not isinstance(body, str):
        raise EvidenceError("Invalid PR description.")
    try:
        digest = hashlib.sha256(body.encode("utf-8")).hexdigest()
    except UnicodeError:
        raise EvidenceError("Invalid PR description.") from None
    return EvidenceVerdict(repo, number, base, head, digest), body


def evaluate_current(policy, reader, number, repo, base, head):
    """Validate the current description and issue a verdict for the observed pair."""
    verdict, body = current_snapshot(reader, number, repo, base, head)
    try:
        validate(policy, body, sources(base, head), lambda path: source_blob(head, path))
    except EvidenceError:
        raise
    except Exception:
        raise EvidenceError("Could not validate the PR evidence or attested source.") from None
    if not verdict.matches_current(reader, number, repo, base, head):
        raise EvidenceError("Could not confirm the PR evidence is still current.")
    return verdict


def check(policy, reader, number, repo, base, head):
    def snapshot():
        pr = reader.get(f"pulls/{number}")
        if (pr["state"], pr["base"]["repo"]["full_name"], pr["base"]["sha"], pr["head"]["sha"]) != (
                "open", repo, base, head):
            raise EvidenceError("PR identity moved or does not match this run.")
        return pr["body"] or ""

    body = snapshot()
    baseline = Path(__file__).with_name("pr-evidence-baseline.txt").read_text().splitlines()
    digest = hashlib.sha256(repr(evidence_sections(policy, body)).encode()).hexdigest()
    grandfathered = f"{number} {head} {digest}" in baseline
    if not grandfathered:
        validate(policy, body, sources(base, head), lambda path: source_blob(head, path))
    if snapshot() != body:
        raise EvidenceError("PR evidence changed during validation. Use the newer run.")
    return "Existing PR with recorded head and evidence" if grandfathered else "Verification and Attestation passed"


def main():
    env = os.environ
    head, run, attempt = (env[key] for key in ("EVIDENCE_HEAD", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"))
    repo, event = env["GITHUB_REPOSITORY"], env["GITHUB_EVENT_NAME"]
    if not SHA.fullmatch(head) or not all(map(NUMBER.fullmatch, (run, attempt))):
        raise EvidenceError("Invalid Actions identity.")
    mode = sys.argv[1]
    workflow = {"verify": "pr-evidence.yml", "publish": "check.yml"}[mode]
    with open(env["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as output:
        output.write(publish(repo, event, head, run, attempt, workflow))
        if mode == "verify" and event == "pull_request":
            sys.path.insert(0, str(Path("attestation-policy/.github/scripts").resolve()))
            import pr_attestation as policy

            number, base = env["PR_NUMBER"], env["PR_BASE"]
            if not NUMBER.fullmatch(number) or not SHA.fullmatch(base):
                raise EvidenceError("Invalid PR identity.")
            reader = policy.GitHub(env["GH_TOKEN"], repo)
            result = check(policy, reader, number, repo, base, head)
            output.write(f"\n**{result}.**\n")


if __name__ == "__main__":
    try:
        main()
    except EvidenceError as error:
        raise SystemExit(str(error)) from None
    except Exception:
        raise SystemExit("Could not read the exact-head evidence or attested source.") from None
