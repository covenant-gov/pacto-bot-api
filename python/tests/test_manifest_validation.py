"""Tests for the example manifest schema and loader helpers."""

from __future__ import annotations

from pathlib import Path

import pytest

from conftest import (
    discover_bot_files,
    load_manifest,
    manifest_path_for_bot,
    validate_manifest,
)


def test_greeting_bot_manifest_is_valid() -> None:
    root = Path(__file__).resolve().parent.parent
    bot_file = root / "examples" / "greeting_bot.py"
    manifest = load_manifest(bot_file)
    validate_manifest(manifest, bot_file)
    assert manifest["manifest_version"] == "1"
    assert manifest["bot_file"] == "greeting_bot.py"


def test_manifest_path_derivation() -> None:
    bot_file = Path("/tmp/examples/greet_bot.py")
    assert manifest_path_for_bot(bot_file) == Path("/tmp/examples/greet_bot.manifest.json")


def test_unsupported_manifest_version_is_rejected(tmp_path: Path) -> None:
    manifest = {
        "manifest_version": "99",
        "bot_file": "x.py",
        "contract_pieces": [],
    }
    with pytest.raises(AssertionError, match="manifest validation failed"):
        validate_manifest(manifest, tmp_path / "x.py")


def test_unknown_contract_piece_type_is_rejected(tmp_path: Path) -> None:
    manifest = {
        "manifest_version": "1",
        "bot_file": "x.py",
        "contract_pieces": [{"name": "bad", "type": "unknown"}],
    }
    with pytest.raises(AssertionError, match="manifest validation failed"):
        validate_manifest(manifest, tmp_path / "x.py")


def test_rpc_call_missing_method_is_rejected(tmp_path: Path) -> None:
    manifest = {
        "manifest_version": "1",
        "bot_file": "x.py",
        "contract_pieces": [{"name": "bad", "type": "rpc_call"}],
    }
    with pytest.raises(AssertionError, match="manifest validation failed"):
        validate_manifest(manifest, tmp_path / "x.py")


def test_missing_manifest_raises_clear_diagnostic(tmp_path: Path) -> None:
    bot_file = tmp_path / "ghost_bot.py"
    bot_file.write_text("# no manifest\n")
    with pytest.raises(FileNotFoundError, match="missing manifest for ghost_bot.py"):
        load_manifest(bot_file)


def test_discovery_excludes_tests_and_dot_dirs() -> None:
    bots = discover_bot_files()
    names = {b.name for b in bots}
    assert "greeting_bot.py" in names
    assert "joke_bot.py" in names
    assert "test_greeting_bot.py" not in names
    assert "test_manifest_validation.py" not in names


if __name__ == "__main__":
    pytest.main([__file__, "-v"])
