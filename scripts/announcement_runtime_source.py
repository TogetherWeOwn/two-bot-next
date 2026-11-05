"""Read-only complete Git-object inventories and offline topology predicates.

No candidate checkout, import, build, filter, hook, network request or revision
execution is performed. These are local observations, NOT authenticated remote
identity, independent review, installed tooling or build/served provenance.
"""

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import selectors
import subprocess
import time

from announcement_runtime_schema import SHA, SHA256, Refusal, _validate, require


CANDIDATE = "9b713c932dcc9a1604d99fcb5b3c55e1d288b2a7"
CANDIDATE_TREE = "deeb6095e05a2538f3cc485cac4a94adbe260534"
RECORDED_BASE = "4469ca165f03555535295ec71509dc08be173ca9"
RECORDED_BASE_TREE = "24396e678fdd4c27ee57feb045d388a57848ec3d"
MERGE_BASE = "a81ee58f46d69478bc435028c728fc782db94809"
ENTRY_LIMIT = 16384
OBJECT_LIMIT = 32 * 1024 * 1024
TREE_BYTES_LIMIT = 256 * 1024 * 1024
PATH_BYTES_LIMIT = 4096
GIT_TIMEOUT = 30
GIT_ADDRESS_SPACE = 512 * 1024 * 1024
INVENTORY_TIMEOUT = 120
REGULAR_MODES = frozenset({"100644", "100755"})


@dataclass(frozen=True)
class Entry:
    raw_path: bytes
    mode: str
    object_id: str
    size: int
    sha256: str

    def record(self):
        # Hex preserves raw path bytes without Git quoting, Unicode normalization
        # or delimiter ambiguity. Content hashes cover bytes, including binaries.
        return {"path_hex": self.raw_path.hex(), "mode": self.mode,
                "object_id": self.object_id, "size": self.size, "sha256": self.sha256}


@dataclass(frozen=True)
class Manifest:
    commit: str
    tree: str
    entries: tuple

    def record(self):
        return {"schema": "two.bot.announcement-source-manifest.v1",
                "commit": self.commit, "tree": self.tree,
                "entries": [entry.record() for entry in self.entries]}

    def digest(self):
        validate_manifest(self)
        return _digest(self.record())


def _digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":"),
                                     ensure_ascii=True).encode("ascii")).hexdigest()


def _path(raw):
    require(type(raw) is bytes and 0 < len(raw) <= PATH_BYTES_LIMIT, "source_path")
    require(not raw.startswith(b"/") and b"\\" not in raw and
            all(byte >= 32 and byte != 127 for byte in raw), "source_path")
    require(all(part and part not in (b".", b"..") and part.lower() != b".git"
                for part in raw.split(b"/")), "source_path")


def _validate_raw_tree(data):
    # ls-tree normalizes noncanonical modes (for example 100664 -> 100644).
    # Validate the actual hash-bound bytes first, including canonical order and
    # nonempty directories, so normalization cannot erase a source difference.
    require(bool(data), "source_tree")
    position = 0
    names = set()
    order = []
    while position < len(data):
        space = data.find(b" ", position)
        require(position < space <= position + 6, "source_mode")
        mode = data[position:space]
        require(mode in (b"40000", b"100644", b"100755"), "source_mode")
        end = data.find(b"\0", space + 1)
        require(end > space + 1 and end + 21 <= len(data), "source_tree")
        name = data[space + 1:end]
        _path(name)
        require(b"/" not in name and name not in names, "source_tree")
        names.add(name)
        require(len(names) <= 2 * ENTRY_LIMIT, "source_inventory")
        order.append(name + (b"/" if mode == b"40000" else b""))
        position = end + 21
    require(order == sorted(order), "source_tree")


def validate_manifest(manifest):
    require(type(manifest) is Manifest and type(manifest.entries) is tuple and
            0 < len(manifest.entries) <= ENTRY_LIMIT, "source_inventory")
    _validate(manifest.commit, SHA)
    _validate(manifest.tree, SHA)
    paths = []
    total = 0
    for entry in manifest.entries:
        require(type(entry) is Entry, "source_entry")
        _path(entry.raw_path)
        require(type(entry.mode) is str and entry.mode in REGULAR_MODES, "source_mode")
        _validate(entry.object_id, SHA)
        _validate(entry.sha256, SHA256)
        require(type(entry.size) is int and 0 <= entry.size <= OBJECT_LIMIT, "source_size")
        total += entry.size
        paths.append(entry.raw_path)
    require(total <= TREE_BYTES_LIMIT, "source_size")
    require(paths == sorted(set(paths)), "source_inventory")
    path_set = set(paths)
    require(not any(b"/".join(path.split(b"/")[:i]) in path_set
                    for path in paths for i in range(1, len(path.split(b"/")))),
            "source_path_collision")


def complete_delta(before, after):
    """Every changed raw path, without rename detection or runtime path filters."""
    validate_manifest(before)
    validate_manifest(after)
    old = {entry.raw_path: entry for entry in before.entries}
    new = {entry.raw_path: entry for entry in after.entries}
    changes = []
    for path in sorted(old.keys() | new.keys()):
        left, right = old.get(path), new.get(path)
        if left != right:
            changes.append({"path_hex": path.hex(),
                            "before": left.record() if left else None,
                            "after": right.record() if right else None})
    return {"schema": "two.bot.announcement-source-delta.v1",
            "before_commit": before.commit, "before_tree": before.tree,
            "after_commit": after.commit, "after_tree": after.tree,
            "changes": changes}


def check_topology_claims(*, candidate, recorded_base, control, merge_bases_cb,
                          merge_bases_ct, ancestor_pairs, current_pr, current_main):
    """Pure consistency check; copied API/ancestor facts do not authenticate them.

    The current T must come from separately authenticated exact implementation
    approval and installation, never a caller-selected ref or today's main tip.
    These checks cannot supply that missing binding or enable runtime_preflight.
    """
    for sha in (candidate, recorded_base, control, current_main):
        _validate(sha, SHA)
    require(candidate == CANDIDATE and recorded_base == RECORDED_BASE and
            control != candidate, "source_identity")
    require(type(merge_bases_cb) is list and merge_bases_cb == [MERGE_BASE] and
            type(merge_bases_ct) is list and merge_bases_ct == [MERGE_BASE], "topology")
    require(type(ancestor_pairs) is frozenset and
            all((MERGE_BASE, sha) in ancestor_pairs
                for sha in (candidate, recorded_base, control)), "topology")
    expected_pr = {
        "repository_id": "1396224242", "full_name": "TogetherWeOwn/two-bot-next",
        "head_repository_id": "1396224242", "base_repository_id": "1396224242",
        "number": "1639", "state": "open", "head_ref": "feat/announcement-command-live-dispatch",
        "base_ref": "main", "head_sha": candidate, "head_tree": CANDIDATE_TREE,
        "base_sha": control,
    }
    require(type(current_pr) is dict and current_pr == expected_pr, "pr_identity")
    require(current_main == control, "control_drift")


class LocalSourceInventory:
    """Observe a local object database, never fetch missing history or execute C.

    Use from a trusted tooling directory, not from a downloaded candidate tree.
    The repo argument identifies an observation target, not a verifier root.
    Results remain untrusted for runtime admission even when all checks pass.
    """

    def __init__(self, repo):
        self.repo = Path(repo).resolve()
        self._deadline = time.monotonic() + INVENTORY_TIMEOUT
        self._blobs = {}
        self._objects = set()
        self._object_bytes = 0
        require(self._git("rev-parse", "--show-object-format").strip() == b"sha1",
                "source_object_format")
        require(self._git("rev-parse", "--is-shallow-repository").strip() == b"false",
                "source_incomplete_history")
        require(not self._git("for-each-ref", "--format=%(refname)", "refs/replace"),
                "source_replaced_history")
        grafts = self._git("rev-parse", "--git-path", "info/grafts").strip()
        graft_path = Path(os.fsdecode(grafts))
        if not graft_path.is_absolute():
            graft_path = self.repo / graft_path
        require(not graft_path.exists(), "source_grafted_history")
        # Lazy/promisor object retrieval is never an offline fallback. Checking
        # config inertly avoids triggering an automatic object fetch on old Git.
        config = self._git("config", "--null", "--list", "--includes")
        keys = [record.split(b"\n", 1)[0].lower() for record in config.split(b"\0")]
        require(not any(b"partialclone" in key or key.endswith(b".promisor") for key in keys),
                "source_incomplete_history")

    def _git(self, *args, limit=OBJECT_LIMIT):
        remaining = min(GIT_TIMEOUT, self._deadline - time.monotonic())
        require(remaining > 0, "source_timeout")
        # No inherited Git config injection, credentials, environment forwarding,
        # user aliases, replace objects, pager, external diff or lazy fetching.
        env = {"PATH": "/usr/bin:/bin", "HOME": "/nonexistent", "LC_ALL": "C",
               "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
               "GIT_NO_REPLACE_OBJECTS": "1", "GIT_NO_LAZY_FETCH": "1",
               "GIT_TERMINAL_PROMPT": "0", "GIT_OPTIONAL_LOCKS": "0"}
        process = None
        try:
            process = subprocess.Popen(
                ["/usr/bin/prlimit", "--as=" + str(GIT_ADDRESS_SPACE),
                 "--cpu=" + str(GIT_TIMEOUT), "--", "/usr/bin/git", "--no-pager",
                 "--no-replace-objects", "-C", str(self.repo), *args],
                env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL)
            end = time.monotonic() + remaining
            chunks = []
            size = 0
            with selectors.DefaultSelector() as selector:
                selector.register(process.stdout, selectors.EVENT_READ)
                while True:
                    wait = end - time.monotonic()
                    require(wait > 0, "source_timeout")
                    require(bool(selector.select(wait)), "source_timeout")
                    chunk = os.read(process.stdout.fileno(), 65536)
                    if not chunk:
                        break
                    size += len(chunk)
                    require(size <= limit, "source_output_size")
                    chunks.append(chunk)
            require(process.wait(timeout=max(0.001, end - time.monotonic())) == 0,
                    "source_git")
            return b"".join(chunks)
        except (OSError, subprocess.SubprocessError):
            raise Refusal("source_git") from None
        finally:
            if process is not None:
                if process.poll() is None:
                    process.kill()
                try:
                    process.wait(timeout=GIT_TIMEOUT)
                except subprocess.TimeoutExpired:
                    raise Refusal("source_timeout") from None
                finally:
                    process.stdout.close()

    def _object(self, kind, object_id):
        _validate(object_id, SHA)
        size_text = self._git("cat-file", "-s", object_id, limit=32).strip()
        require(size_text.isdigit() and len(size_text) <= 12, "source_size")
        size = int(size_text)
        require(size <= OBJECT_LIMIT, "source_size")
        key = (kind, object_id)
        if key not in self._objects:
            self._object_bytes += size
            require(self._object_bytes <= TREE_BYTES_LIMIT, "source_size")
            self._objects.add(key)
        data = self._git("cat-file", kind, object_id, limit=OBJECT_LIMIT)
        require(len(data) == size, "source_size")
        header = kind.encode("ascii") + b" " + str(size).encode("ascii") + b"\0"
        require(hashlib.sha1(header + data).hexdigest() == object_id, "source_object")
        if kind == "tree":
            _validate_raw_tree(data)
        return data

    def _commit_tree(self, commit):
        _validate(commit, SHA)
        require(self._git("cat-file", "-t", commit).strip() == b"commit", "source_commit")
        raw = self._object("commit", commit).split(b"\n", 1)[0]
        require(raw.startswith(b"tree "), "source_commit")
        try:
            tree = raw[5:].decode("ascii")
        except UnicodeError:
            raise Refusal("source_tree") from None
        _validate(tree, SHA)
        return tree

    def manifest(self, commit):
        tree = self._commit_tree(commit)
        self._object("tree", tree)
        raw = self._git("ls-tree", "-r", "-t", "-z", "--full-tree", "-l", tree)
        require(raw.endswith(b"\0"), "source_tree")
        records = raw[:-1].split(b"\0")
        require(0 < len(records) <= 2 * ENTRY_LIMIT, "source_inventory")
        entries = []
        total = 0
        for record in records:
            header, separator, path = record.partition(b"\t")
            require(bool(separator), "source_tree")
            _path(path)
            fields = header.split()
            require(len(fields) == 4, "source_tree")
            if fields[0] == b"040000":
                require(fields[1] == b"tree" and fields[3] == b"-", "source_mode")
                try:
                    subtree = fields[2].decode("ascii")
                except UnicodeError:
                    raise Refusal("source_tree") from None
                self._object("tree", subtree)
                continue
            require(fields[0] in (b"100644", b"100755") and fields[1] == b"blob",
                    "source_mode")
            try:
                object_id = fields[2].decode("ascii")
                size_text = fields[3].decode("ascii")
                mode = fields[0].decode("ascii")
            except UnicodeError:
                raise Refusal("source_tree") from None
            _validate(object_id, SHA)
            require(size_text.isdecimal() and len(size_text) <= 12, "source_size")
            size = int(size_text)
            total += size
            require(size <= OBJECT_LIMIT and total <= TREE_BYTES_LIMIT, "source_size")
            cached = self._blobs.get(object_id)
            if cached is None:
                data = self._object("blob", object_id)
                require(len(data) == size, "source_size")
                cached = (size, hashlib.sha256(data).hexdigest())
                self._blobs[object_id] = cached
            require(cached[0] == size, "source_size")
            entries.append(Entry(path, mode, object_id, size, cached[1]))
        result = Manifest(commit, tree, tuple(sorted(entries, key=lambda entry: entry.raw_path)))
        validate_manifest(result)
        return result

    def merge_bases(self, left, right):
        self._commit_tree(left)
        self._commit_tree(right)
        raw = self._git("merge-base", "--all", left, right)
        try:
            bases = raw.decode("ascii").splitlines()
        except UnicodeError:
            raise Refusal("topology") from None
        require(0 < len(bases) <= ENTRY_LIMIT, "topology")
        for base in bases:
            _validate(base, SHA)
        return bases

    def require_ancestor(self, ancestor, descendant):
        _validate(ancestor, SHA)
        _validate(descendant, SHA)
        require(self.merge_bases(ancestor, descendant) == [ancestor], "topology")


def collect_source_comparisons(repo, *, control, served_before):
    """Enumerate M/C, historical B0/C, reviewed B0/T and actual S/C separately.

    T and S are observation inputs, not approved bindings. This function cannot
    attest that S was actually served, that T is installed, or that any manifest
    was independently reviewed. Refuse missing pins rather than infer them.
    """
    _validate(control, SHA)
    _validate(served_before, SHA)
    require(control != CANDIDATE, "source_identity")
    source = LocalSourceInventory(repo)
    manifests = {sha: source.manifest(sha) for sha in dict.fromkeys(
        (MERGE_BASE, RECORDED_BASE, CANDIDATE, control, served_before))}
    require(manifests[CANDIDATE].tree == CANDIDATE_TREE and
            manifests[RECORDED_BASE].tree == RECORDED_BASE_TREE, "source_identity")
    require(source.merge_bases(CANDIDATE, RECORDED_BASE) == [MERGE_BASE] and
            source.merge_bases(CANDIDATE, control) == [MERGE_BASE], "topology")
    for descendant in (CANDIDATE, RECORDED_BASE, control):
        source.require_ancestor(MERGE_BASE, descendant)
    pairs = {"source": (MERGE_BASE, CANDIDATE), "historical": (RECORDED_BASE, CANDIDATE),
             "installation": (RECORDED_BASE, control), "served": (served_before, CANDIDATE)}
    return {"manifests": manifests,
            "deltas": {name: complete_delta(manifests[left], manifests[right])
                       for name, (left, right) in pairs.items()}}
