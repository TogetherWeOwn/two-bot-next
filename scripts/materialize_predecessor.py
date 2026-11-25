"""Materialize the verified predecessor bundle for a private local consumer."""

from contextlib import contextmanager
import os
from pathlib import Path
from shutil import copyfileobj
from struct import error as StructError
from tempfile import TemporaryDirectory
from zipfile import BadZipFile, ZipFile
from zlib import error as CompressionError

from predecessor_bundle import BUILD, IMAGE, WORKER, verify_predecessor_bundle
from staging_rollout import GateError


def _reserved_on_windows(relative):
    if os.name != "nt":
        return False
    if hasattr(os.path, "isreserved"):
        return os.path.isreserved(relative)
    # Windows on Python 3.12 and earlier exposes the check through pathlib.
    return any(Path(part).is_reserved() for part in relative.parts)


@contextmanager
def materialize_predecessor_bundle(archive, *, scratch_parent=None, **verification_pins):
    """Yield a private directory containing the complete original bundle files.

    The caller owns the seekable archive stream and keeps it private and stable.
    This helper needs exclusive access during verification and copying. Use a
    trusted scratch parent when overriding the default temporary directory. The
    yielded Path and its contents are available only within the context.
    """
    receipt = verify_predecessor_bundle(archive, **verification_pins)
    names = {BUILD, IMAGE, *(WORKER + path for path in receipt["worker_modules_sha256"])}

    with TemporaryDirectory(dir=scratch_parent, prefix="predecessor-") as scratch:
        root = Path(scratch)
        try:
            with ZipFile(archive) as bundle:
                for name in sorted(names):
                    relative = Path(name)
                    if _reserved_on_windows(relative):
                        raise GateError("predecessor_materialization_failed")
                    info = bundle.getinfo(name)
                    target = root / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    with bundle.open(info) as source, target.open("xb") as destination:
                        copyfileobj(source, destination)
                    if target.stat().st_size != info.file_size:
                        raise GateError("predecessor_materialization_failed")
            # A filesystem can alias different archive directories, even when
            # all output files can be created exclusively.
            if {path.relative_to(root).as_posix() for path in root.rglob("*") if path.is_file()} != names:
                raise GateError("predecessor_materialization_failed")
        except (BadZipFile, CompressionError, EOFError, KeyError, OSError, RuntimeError, StructError, ValueError):
            raise GateError("predecessor_materialization_failed") from None
        yield root
