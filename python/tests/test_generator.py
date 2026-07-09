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
    # Capabilities the handler requests. Valid values include ReadMessages, SendMessages, ManageProfile, SendGroupMessages, ReceiveGroupMessages, CreateMlsGroup, and InviteToMlsGroup.
    capabilities: list[str]
    # Event types the handler wants to receive.
    event_types: list[str]

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
    assert "AgentMetricsResponse = dict[str, Any]" in models_source
    assert "AgentVersionResponse = dict[str, Any]" in models_source


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


def test_client_init_accepts_timeout(run_generator):
    """PactoClient.__init__ accepts an optional timeout parameter."""
    client_source = run_generator[1].decode("utf-8")
    assert (
        "def __init__(self, transport: Any, timeout: float | None = None)"
        in client_source
    )


def test_client_request_accepts_timeout(run_generator):
    """The internal _request method accepts an optional timeout parameter."""
    client_source = run_generator[1].decode("utf-8")
    assert "async def _request(\n" in client_source
    assert "timeout: float | None = None" in client_source


def test_request_method_forwards_timeout(run_generator):
    """Generated request methods accept timeout and pass it to _request."""
    client_source = run_generator[1].decode("utf-8")
    # handler.register is a representative request method.
    assert (
        "async def handler_register(self, bot_ids: list[str], capabilities: list[str], event_types: list[str], timeout: float | None = _DEFAULT_TIMEOUT)"
        in client_source
    )
    assert (
        'await self._request("handler.register", params_dict, timeout=timeout)'
        in client_source
    )


def test_notification_method_has_no_timeout(run_generator):
    """Generated notification methods do not accept a timeout parameter."""
    client_source = run_generator[1].decode("utf-8")
    assert (
        "async def agent_error(self, bot_id: str, message: str, code: str | None = None, data: Any | None = None) -> None:"
        in client_source
    )
    notification_block = client_source.split("async def agent_error")[1].split(
        "async def"
    )[0]
    assert "timeout" not in notification_block
