"""Tests for the Python SDK generator."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parent.parent.parent
GENERATOR = ROOT / "python" / "scripts" / "generate.py"
MODELS = ROOT / "python" / "src" / "pacto_bot_sdk" / "_generated" / "models.py"
CLIENT = ROOT / "python" / "src" / "pacto_bot_sdk" / "_generated" / "client.py"


HANDLER_REGISTER_PARAMS_SNAPSHOT = '''\
class HandlerRegisterParams(BaseModel):
    """
    Model for JSON-RPC method `handler.register`.

    Register a handler connection for event delivery.

    Example:

        >>> HandlerRegisterParams(bot_ids=[], capabilities=[], event_types=[])

    jsonrpc_method: ``"handler.register"``
    """
    jsonrpc_method: ClassVar[str] = "handler.register"
    # Bot identities this handler wants to serve.
    bot_ids: list[str]
    # Capabilities the handler requests.
    capabilities: list[str]
    # Event types the handler wants to receive.
    event_types: list[str]
    # Optional previously assigned handler id to reuse after reconnect.
    handler_id: str | None = None

'''


@pytest.fixture
def run_generator():
    """Run the generator and return the generated file contents."""
    result = subprocess.run(
        [sys.executable, str(GENERATOR)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    assert "python: generated SDK" in result.stdout
    return MODELS.read_bytes(), CLIENT.read_bytes()


def test_generator_idempotent(run_generator):
    """Running the generator twice must produce byte-identical output."""
    first_models, first_client = run_generator
    subprocess.run(
        [sys.executable, str(GENERATOR)],
        cwd=ROOT,
        check=True,
    )
    second_models = MODELS.read_bytes()
    second_client = CLIENT.read_bytes()
    assert first_models == second_models
    assert first_client == second_client


def test_handler_register_params_snapshot(run_generator):
    """The emitted HandlerRegisterParams class matches the expected shape."""
    models_source = run_generator[0].decode("utf-8")
    match = re.search(
        r"^(class HandlerRegisterParams\(BaseModel\):.*?)(?=\nclass |\Z)",
        models_source,
        re.S | re.M,
    )
    assert match is not None
    assert match.group(1) == HANDLER_REGISTER_PARAMS_SNAPSHOT


def test_unresolved_ref_result_is_dict(run_generator):
    """External $ref results are typed as dict[str, Any]."""
    models_source = run_generator[0].decode("utf-8")
    assert "AgentMetricsResult = dict[str, Any]" in models_source
    assert "AgentVersionResult = dict[str, Any]" in models_source


def test_agent_event_required_and_optional_fields(run_generator):
    """AgentEventParams has the expected required and optional fields."""
    models_source = run_generator[0].decode("utf-8")
    assert "class AgentEventParams(BaseModel):" in models_source
    # Required fields have no default.
    assert "\n    content: str\n" in models_source
    assert "\n    rumor_id: str\n" in models_source
    assert "\n    author: str\n" in models_source
    assert "\n    timestamp: int\n" in models_source
    # Optional chat_id defaults to None.
    assert "\n    chat_id: str | None = None\n" in models_source
