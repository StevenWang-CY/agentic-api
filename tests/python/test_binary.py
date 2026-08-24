from __future__ import annotations

from pathlib import Path

import pytest

from agentic_api.binary import (
    PackagedBinaryNotFoundError,
    find_packaged_binary,
    read_binary_version,
)


def test_find_packaged_binary_prefers_scripts_directory_over_global_binary(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    scripts_dir = tmp_path / "env" / "bin"
    scripts_dir.mkdir(parents=True)
    local_binary = scripts_dir / "agentic-server"
    local_binary.write_text("#!/bin/sh\nexit 0\n")
    local_binary.chmod(0o755)

    global_binary = tmp_path / "global" / "agentic-server"
    global_binary.parent.mkdir(parents=True)
    global_binary.write_text("#!/bin/sh\nexit 0\n")
    global_binary.chmod(0o755)

    monkeypatch.setattr("agentic_api.binary.sysconfig.get_path", lambda name: str(scripts_dir))
    monkeypatch.setattr("agentic_api.binary.sys.executable", str(tmp_path / "env" / "bin" / "python"))
    monkeypatch.setattr("agentic_api.binary.shutil.which", lambda name: str(global_binary))

    assert find_packaged_binary("agentic-server") == local_binary


@pytest.mark.parametrize("path_exists", [False, True])
def test_find_packaged_binary_reports_remediation_when_packaged_binary_is_missing_or_not_executable(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, path_exists: bool
) -> None:
    scripts_dir = tmp_path / "env" / "bin"
    scripts_dir.mkdir(parents=True)
    local_binary = scripts_dir / "agentic-server"
    if path_exists:
        local_binary.write_text("#!/bin/sh\nexit 0\n")
        local_binary.chmod(0o644)

    monkeypatch.setattr("agentic_api.binary.sysconfig.get_path", lambda name: str(scripts_dir))
    monkeypatch.setattr("agentic_api.binary.sys.executable", str(tmp_path / "env" / "bin" / "python"))
    monkeypatch.setattr("agentic_api.binary.shutil.which", lambda name: None)

    with pytest.raises(PackagedBinaryNotFoundError, match="Reinstall agentic-api for this platform"):
        find_packaged_binary("agentic-server")


def test_read_binary_version_returns_first_line_from_version_output(tmp_path: Path) -> None:
    binary = tmp_path / "agentic-server"
    binary.write_text("#!/bin/sh\nprintf 'agentic-server 0.4.0\\nextra detail\\n'\n")
    binary.chmod(0o755)

    assert read_binary_version(binary) == "agentic-server 0.4.0"
