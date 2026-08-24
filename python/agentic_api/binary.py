from __future__ import annotations

import os
import shutil
import subprocess
import sys
import sysconfig
from pathlib import Path


REMEDIATION_MESSAGE = "Reinstall agentic-api for this platform"


class PackagedBinaryNotFoundError(FileNotFoundError):
    """Raised when a packaged executable cannot be located."""


class PackagedBinaryVersionError(RuntimeError):
    """Raised when a packaged executable cannot report its version."""


def find_packaged_binary(name: str) -> Path:
    try:
        return find_active_environment_executable(name)
    except FileNotFoundError:
        pass
    raise PackagedBinaryNotFoundError(f"{name} not found; {REMEDIATION_MESSAGE}")


def find_active_environment_executable(name: str) -> Path:
    for candidate in _candidate_paths(name):
        if _is_executable_file(candidate):
            return candidate
    raise FileNotFoundError(f"{name} executable not found in the active environment")


def read_binary_version(path: Path) -> str:
    try:
        completed = subprocess.run(
            [str(path), "--version"],
            check=True,
            capture_output=True,
            text=True,
        )
    except OSError as error:  # pragma: no cover - exercised via unit tests.
        raise PackagedBinaryVersionError(f"unable to launch {path}: {error.strerror or error}") from error
    except subprocess.CalledProcessError as error:
        raise PackagedBinaryVersionError(
            f"{path} exited with status {error.returncode} while reporting its version"
        ) from error

    output = (completed.stdout or completed.stderr).strip()
    if not output:
        raise PackagedBinaryVersionError(f"{path} did not report a version")
    return output.splitlines()[0].strip()


def _candidate_paths(name: str) -> list[Path]:
    candidates: list[Path] = []

    scripts_dir = sysconfig.get_path("scripts")
    if scripts_dir:
        candidates.append(Path(scripts_dir) / name)

    candidates.append(Path(sys.executable).resolve().parent / name)

    which_path = shutil.which(name)
    if which_path:
        candidates.append(Path(which_path))

    unique_candidates: list[Path] = []
    seen: set[Path] = set()
    for candidate in candidates:
        resolved = candidate.resolve(strict=False)
        if resolved in seen:
            continue
        seen.add(resolved)
        unique_candidates.append(candidate)
    return unique_candidates


def _is_executable_file(path: Path) -> bool:
    return path.is_file() and os.access(path, os.X_OK)
