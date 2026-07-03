#!/usr/bin/env python3
"""Generate the pacto-bot-sdk Python SDK from schemas/jsonrpc.json.

This script is invoked by `cargo xtask codegen`. It reads the canonical
OpenRPC catalog and emits typed Pydantic models and a low-level async client
under python/src/pacto_bot_sdk/_generated/."""

from __future__ import annotations

import argparse
import json
import re
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any


GENERATED_HEADER = """\
# Generated from schemas/jsonrpc.json — do not edit manually.
# Run `cargo xtask codegen` to regenerate.

from __future__ import annotations
"""


# ---------------------------------------------------------------------------
# Naming helpers
# ---------------------------------------------------------------------------


def to_pascal_case(name: str) -> str:
    """Convert a dotted/snake identifier to PascalCase."""
    normalized = re.sub(r"[._]", "_", name)
    return "".join(part.capitalize() for part in normalized.split("_") if part)


def to_snake_case(name: str) -> str:
    """Return a JSON Schema property name as snake_case."""
    return name.lower()


# ---------------------------------------------------------------------------
# Model representation
# ---------------------------------------------------------------------------


@dataclass
class FieldDef:
    name: str
    annotation: str
    required: bool
    description: str


@dataclass
class ModelDef:
    name: str
    jsonrpc_method: str
    summary: str
    fields: list[FieldDef] = field(default_factory=list)


@dataclass
class ResultAlias:
    name: str
    jsonrpc_method: str
    annotation: str


# ---------------------------------------------------------------------------
# Schema traversal
# ---------------------------------------------------------------------------


def _collect_object_model(
    class_name: str,
    jsonrpc_method: str,
    summary: str,
    schema: dict[str, Any],
) -> ModelDef:
    """Build a ModelDef for an object schema, recursively collecting nested models."""
    props = schema.get("properties", {})
    required = set(schema.get("required", []))
    model = ModelDef(
        name=class_name,
        jsonrpc_method=jsonrpc_method,
        summary=summary or f"Model for JSON-RPC method `{jsonrpc_method}`.",
    )

    for prop_name, prop_schema in sorted(props.items()):
        annotation, nested_models = _type_annotation(
            prop_schema,
            parent_name=class_name,
            prop_name=prop_name,
            jsonrpc_method=jsonrpc_method,
        )
        field_required = prop_name in required
        model.fields.append(
            FieldDef(
                name=to_snake_case(prop_name),
                annotation=annotation,
                required=field_required,
                description=prop_schema.get("description", ""),
            )
        )
        # Nested models are emitted globally; we don't need to attach them here.
        _emitted_nested_models.update({m.name: m for m in nested_models})

    return model


# Global collector for nested models discovered during traversal.
_emitted_nested_models: dict[str, ModelDef] = {}


def _type_annotation(
    schema: dict[str, Any],
    *,
    parent_name: str,
    prop_name: str,
    jsonrpc_method: str,
) -> tuple[str, list[ModelDef]]:
    """Return a Pydantic type annotation and any nested models it requires."""
    if "$ref" in schema:
        return "dict[str, Any]", []

    schema_type = schema.get("type")

    if schema_type == "string":
        return "str", []
    if schema_type == "integer":
        return "int", []
    if schema_type == "boolean":
        return "bool", []
    if schema_type == "array":
        items = schema.get("items", {})
        inner, nested = _type_annotation(
            items,
            parent_name=parent_name,
            prop_name=prop_name,
            jsonrpc_method=jsonrpc_method,
        )
        return f"list[{inner}]", nested
    if schema_type == "object":
        nested_name = f"{parent_name}{to_pascal_case(prop_name)}Model"
        nested_model = _collect_object_model(
            nested_name,
            jsonrpc_method,
            f"Nested object for `{prop_name}` of `{jsonrpc_method}`.",
            schema,
        )
        return nested_name, [nested_model]

    return "Any", []


def _result_annotation(
    method: dict[str, Any],
    class_name: str,
) -> ModelDef | ResultAlias | None:
    """Return either a model definition or a type alias for a method result."""
    result = method.get("result")
    if not result:
        return None
    schema = result.get("schema", {})

    if "$ref" in schema:
        return ResultAlias(
            name=class_name,
            jsonrpc_method=method["name"],
            annotation="dict[str, Any]",
        )

    schema_type = schema.get("type")
    if schema_type == "object":
        return _collect_object_model(
            class_name,
            method["name"],
            method.get("summary", ""),
            schema,
        )
    if schema_type == "string":
        return ResultAlias(name=class_name, jsonrpc_method=method["name"], annotation="str")
    if schema_type == "integer":
        return ResultAlias(name=class_name, jsonrpc_method=method["name"], annotation="int")
    if schema_type == "boolean":
        return ResultAlias(name=class_name, jsonrpc_method=method["name"], annotation="bool")

    return ResultAlias(name=class_name, jsonrpc_method=method["name"], annotation="Any")


# ---------------------------------------------------------------------------
# Model file emission
# ---------------------------------------------------------------------------


def _example_kwargs(fields: list[FieldDef]) -> str:
    """Build a minimal usage example for a model's docstring."""
    kwargs: list[str] = []
    for f in fields:
        if not f.required:
            continue
        if f.annotation == "str":
            kwargs.append(f'{f.name}="..."')
        elif f.annotation == "int":
            kwargs.append(f"{f.name}=0")
        elif f.annotation == "bool":
            kwargs.append(f"{f.name}=True")
        elif f.annotation.startswith("list["):
            kwargs.append(f"{f.name}=[]")
        else:
            kwargs.append(f"{f.name}=...")
    return ", ".join(kwargs)


def _emit_model_class(out: list[str], model: ModelDef) -> None:
    out.append(f"class {model.name}(BaseModel):\n")

    doc_lines: list[str] = [
        f"Model for JSON-RPC method `{model.jsonrpc_method}`.",
        "",
    ]
    if model.summary:
        doc_lines.append(model.summary)
        doc_lines.append("")
    if model.fields:
        example = f"{model.name}({_example_kwargs(model.fields)})"
        doc_lines.extend(["Example:", "", f"    >>> {example}", ""])
    doc_lines.append(f'jsonrpc_method: ``"{model.jsonrpc_method}"``')
    out.append(_indent_docstring(doc_lines, indent=4))
    out.append("\n")

    out.append(f'    jsonrpc_method: ClassVar[str] = "{model.jsonrpc_method}"\n')

    if not model.fields:
        out.append("    pass\n\n")
        return

    for f in model.fields:
        if f.description:
            out.append(f"    # {f.description}\n")
        annot = f.annotation if f.required else f"{f.annotation} | None"
        default = "" if f.required else " = None"
        out.append(f"    {f.name}: {annot}{default}\n")

    out.append("\n\n")


def _indent_docstring(lines: list[str], *, indent: int) -> str:
    """Render a list of lines as an indented triple-quoted docstring."""
    spaces = " " * indent
    body = "\n".join(spaces + line if line else "" for line in lines)
    return f'{spaces}"""\n{body}\n{spaces}"""'


def generate_models(schema: dict[str, Any], output_path: Path) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)

    global _emitted_nested_models
    _emitted_nested_models = {}

    models: dict[str, ModelDef] = {}
    aliases: list[ResultAlias] = []

    methods = sorted(schema.get("methods", []), key=lambda m: m["name"])

    for method in methods:
        method_name = method["name"]
        summary = method.get("summary", "")
        base_name = to_pascal_case(method_name)

        # Params model (always emitted, even if the params schema is empty).
        params_schema: dict[str, Any]
        params_list = method.get("params", [])
        if params_list and params_list[0].get("schema"):
            params_schema = params_list[0]["schema"]
        else:
            params_schema = {"type": "object"}
        params_class = f"{base_name}Params"
        models[params_class] = _collect_object_model(
            params_class, method_name, summary, params_schema
        )

        # Result model or alias.
        result_def = _result_annotation(method, f"{base_name}Result")
        if isinstance(result_def, ModelDef):
            models[result_def.name] = result_def
        elif isinstance(result_def, ResultAlias):
            aliases.append(result_def)

    # Merge nested models discovered during traversal; they take precedence if
    # a top-level model name collides (should not happen with sane schemas).
    for name, nested_model in _emitted_nested_models.items():
        models.setdefault(name, nested_model)

    # Deterministic ordering.
    sorted_model_names = sorted(models)
    sorted_alias_names = sorted(a.name for a in aliases)

    out: list[str] = [GENERATED_HEADER]
    out.append("from typing import Any, ClassVar\n\n")
    out.append("from pydantic import BaseModel\n\n")
    out.append('"""Pydantic models generated from schemas/jsonrpc.json."""\n\n')

    for alias in sorted(aliases, key=lambda a: a.name):
        out.append(f"# Result type alias for `{alias.jsonrpc_method}`.\n")
        out.append(f"{alias.name} = {alias.annotation}\n\n")

    for name in sorted_model_names:
        _emit_model_class(out, models[name])

    out.append(f"__all__: list[str] = {sorted_alias_names + sorted_model_names}\n")

    output_path.write_text("".join(out), encoding="utf-8")


# ---------------------------------------------------------------------------
# Client file emission
# ---------------------------------------------------------------------------


def _client_method_docstring(
    method_name: str,
    summary: str,
    py_name: str,
    params_model: str,
    result_repr: str,
    *,
    indent: int,
) -> str:
    lines = [
        f"Call JSON-RPC method `{method_name}`.",
        "",
    ]
    if summary:
        lines.append(summary)
        lines.append("")
    lines.extend([
        "Example:",
        "",
        f"    >>> result = await client.{py_name}(...)",
        f"    >>> isinstance(result, {result_repr})",
        "",
    ])
    lines.append(f'jsonrpc_method: ``"{method_name}"``')
    return _indent_docstring(lines, indent=indent)


def _client_notification_docstring(
    method_name: str,
    summary: str,
    py_name: str,
    *,
    indent: int,
) -> str:
    lines = [
        f"Send JSON-RPC notification `{method_name}`.",
        "",
    ]
    if summary:
        lines.append(summary)
        lines.append("")
    lines.extend([
        "Example:",
        "",
        f"    >>> await client.{py_name}(...)",
        "",
    ])
    lines.append(f'jsonrpc_method: ``"{method_name}"``')
    return _indent_docstring(lines, indent=indent)


def _emit_request_method(
    out: list[str],
    method: dict[str, Any],
    params_model: str,
    result_kind: str,
    result_model: str,
) -> None:
    method_name = method["name"]
    summary = method.get("summary", "")
    py_name = to_snake_case(method_name.replace(".", "_"))

    params_list = method.get("params", [])
    if params_list and params_list[0].get("schema"):
        param_fields = _collect_object_model(
            params_model,
            method_name,
            summary,
            params_list[0]["schema"],
        ).fields
    else:
        param_fields = []

    sig_parts = ["self"]
    for f in sorted(param_fields, key=lambda f: (not f.required, f.name)):
        annot = f.annotation if f.required else f"{f.annotation} | None"
        default = "" if f.required else " = None"
        sig_parts.append(f"{f.name}: {annot}{default}")

    return_type: str
    return_expr: str
    if result_kind == "model":
        return_type = f"models.{result_model}"
        return_expr = f"return models.{result_model}.model_validate(result)"
    elif result_kind == "alias":
        return_type = f"models.{result_model}"
        return_expr = "return result"
    else:
        return_type = "Any"
        return_expr = "return result"

    out.append(f"    async def {py_name}({', '.join(sig_parts)}) -> {return_type}:\n")
    out.append(
        _client_method_docstring(
            method_name,
            summary,
            py_name,
            params_model,
            result_model,
            indent=8,
        )
    )
    out.append("\n")

    if param_fields:
        kwargs = ", ".join(f"{f.name}={f.name}" for f in param_fields)
        out.append(f"        params = models.{params_model}({kwargs})\n")
        out.append(
            "        params_dict = params.model_dump(mode='json', exclude_none=True)\n"
        )
    else:
        out.append("        params_dict: dict[str, Any] = {}\n")

    out.append("        response = await self._request(")
    out.append(f'"{method_name}", params_dict)\n')
    out.append("        result = response.get('result')\n")
    out.append(f"        {return_expr}\n\n")


def _emit_notification_method(
    out: list[str],
    method: dict[str, Any],
    params_model: str,
) -> None:
    method_name = method["name"]
    summary = method.get("summary", "")
    py_name = to_snake_case(method_name.replace(".", "_"))

    params_list = method.get("params", [])
    if params_list and params_list[0].get("schema"):
        param_fields = _collect_object_model(
            params_model,
            method_name,
            summary,
            params_list[0]["schema"],
        ).fields
    else:
        param_fields = []

    sig_parts = ["self"]
    for f in sorted(param_fields, key=lambda f: (not f.required, f.name)):
        annot = f.annotation if f.required else f"{f.annotation} | None"
        default = "" if f.required else " = None"
        sig_parts.append(f"{f.name}: {annot}{default}")

    out.append(f"    async def {py_name}({', '.join(sig_parts)}) -> None:\n")
    out.append(_client_notification_docstring(method_name, summary, py_name, indent=8))
    out.append("\n")

    if param_fields:
        kwargs = ", ".join(f"{f.name}={f.name}" for f in param_fields)
        out.append(f"        params = models.{params_model}({kwargs})\n")
        out.append(
            "        params_dict = params.model_dump(mode='json', exclude_none=True)\n"
        )
    else:
        out.append("        params_dict: dict[str, Any] = {}\n")

    out.append('        frame = {\n')
    out.append('            "jsonrpc": "2.0",\n')
    out.append(f'            "method": "{method_name}",\n')
    out.append('            "params": params_dict,\n')
    out.append('        }\n')
    out.append("        await self.transport.write_frame(frame)\n\n")


def generate_client(schema: dict[str, Any], output_path: Path) -> None:
    output_path.parent.mkdir(parents=True, exist_ok=True)

    methods = sorted(schema.get("methods", []), key=lambda m: m["name"])

    # Mapping of JSON-RPC method name to params/result model names for the
    # notification dispatch table.
    incoming_notifications = {"agent.event", "agent.status"}
    params_by_method: dict[str, str] = {}
    result_by_method: dict[str, tuple[str, str, str]] = {}

    for method in methods:
        method_name = method["name"]
        base_name = to_pascal_case(method_name)
        params_by_method[method_name] = f"{base_name}Params"
        if method.get("result"):
            result_schema = method["result"].get("schema", {})
            result_base = f"{base_name}Result"
            if result_schema.get("type") == "object" and "$ref" not in result_schema:
                result_by_method[method_name] = ("model", result_base, result_base)
            else:
                # Scalar or unresolved reference: emit a type alias in models.py
                # and return it directly from the client method.
                annotation = _result_annotation(method, result_base)
                assert isinstance(annotation, ResultAlias)
                result_by_method[method_name] = ("alias", result_base, annotation.annotation)

    out: list[str] = [GENERATED_HEADER]
    out.append("import asyncio\n")
    out.append("import json\n")
    out.append("import uuid\n")
    out.append("from typing import Any\n\n")
    out.append("from . import models\n")
    out.append("from pydantic import BaseModel\n\n")
    out.append('"""Low-level async JSON-RPC client generated from schemas/jsonrpc.json."""\n\n')

    out.append("class PactoClientError(Exception):\n")
    out.append("    \"\"\"Error returned by the daemon for a JSON-RPC request.\"\"\"\n\n")

    out.append("class PactoClient:\n")
    out.append(
        '    """Transport-agnostic async client for the Pacto daemon."""\n\n'
    )
    out.append("    def __init__(self, transport: Any) -> None:\n")
    out.append("        self.transport = transport\n")
    out.append(
        "        self._inflight: dict[str, asyncio.Future[dict[str, Any]]] = {}\n"
    )
    out.append("        self._notify_queue: asyncio.Queue[BaseModel | None] = asyncio.Queue()\n")
    out.append("        self._read_task: asyncio.Task[None] | None = None\n")
    out.append("        self._closed = False\n\n")

    out.append("    async def connect(self) -> None:\n")
    out.append('        \"\"\"Connect the transport and start the background read loop.\"\"\"\n')
    out.append("        await self.transport.connect()\n")
    out.append(
        "        self._read_task = asyncio.create_task(self._read_loop())\n\n"
    )

    out.append("    async def close(self) -> None:\n")
    out.append('        """Stop the read loop and close the transport."""\n')
    out.append("        self._closed = True\n")
    out.append("        await self._notify_queue.put(None)\n")
    out.append("        if self._read_task is not None:\n")
    out.append("            self._read_task.cancel()\n")
    out.append("            try:\n")
    out.append("                await self._read_task\n")
    out.append("            except asyncio.CancelledError:\n")
    out.append("                pass\n")
    out.append("        await self.transport.close()\n\n")

    out.append("    async def _request(\n")
    out.append("        self, method: str, params: dict[str, Any]\n")
    out.append("    ) -> dict[str, Any]:\n")
    out.append('        \"\"\"Send a JSON-RPC request and await its correlated response.\"\"\"\n')
    out.append("        request_id = str(uuid.uuid4())\n")
    out.append('        frame = {\n')
    out.append('            "jsonrpc": "2.0",\n')
    out.append('            "id": request_id,\n')
    out.append('            "method": method,\n')
    out.append('            "params": params,\n')
    out.append('        }\n')
    out.append(
        "        future: asyncio.Future[dict[str, Any]] = asyncio.get_running_loop().create_future()\n"
    )
    out.append("        self._inflight[request_id] = future\n")
    out.append("        try:\n")
    out.append("            immediate = await self.transport.write_frame(frame)\n")
    out.append("            if immediate is not None:\n")
    out.append("                self._resolve(request_id, immediate)\n")
    out.append("            response = await future\n")
    out.append("            if \"error\" in response:\n")
    out.append("                error = response[\"error\"]\n")
    out.append("                raise PactoClientError(\n")
    out.append("                    error.get(\"message\", str(error))\n")
    out.append("                ) from None\n")
    out.append("            return response\n")
    out.append("        finally:\n")
    out.append("            self._inflight.pop(request_id, None)\n\n")

    out.append("    def _resolve(self, request_id: str, response: dict[str, Any]) -> None:\n")
    out.append("        future = self._inflight.pop(request_id, None)\n")
    out.append("        if future is not None and not future.done():\n")
    out.append("            future.set_result(response)\n\n")

    out.append("    async def _read_loop(self) -> None:\n")
    out.append("        while not self._closed:\n")
    out.append("            try:\n")
    out.append("                line = await self.transport.readline()\n")
    out.append("            except asyncio.CancelledError:\n")
    out.append("                break\n")
    out.append("            except Exception:  # pragma: no cover - transport disconnect\n")
    out.append("                break\n")
    out.append("            if not line:\n")
    out.append("                break\n")
    out.append("            try:\n")
    out.append("                frame = json.loads(line)\n")
    out.append("            except json.JSONDecodeError:\n")
    out.append("                continue\n")
    out.append("            await self._dispatch_frame(frame)\n")
    out.append("        await self._notify_queue.put(None)\n")

    out.append("    async def _dispatch_frame(self, frame: dict[str, Any]) -> None:\n")
    out.append('        if "id" in frame:\n')
    out.append("            self._resolve(str(frame['id']), frame)\n")
    out.append("            return\n")
    out.append("        method = frame.get('method')\n")
    out.append("        params = frame.get('params', {})\n")
    out.append("        if method == 'agent.event':\n")
    out.append("            await self._notify_queue.put(models.AgentEventParams.model_validate(params))\n")
    out.append("        elif method == 'agent.status':\n")
    out.append("            await self._notify_queue.put(models.AgentStatusParams.model_validate(params))\n\n")

    out.append("    async def notifications(self) -> Any:\n")
    out.append('        """Async iterator over incoming daemon notifications."""\n')
    out.append("        while not self._closed:\n")
    out.append("            notification = await self._notify_queue.get()\n")
    out.append("            if notification is None:\n")
    out.append("                break\n")
    out.append("            yield notification\n\n")

    for method in methods:
        method_name = method["name"]
        base_name = to_pascal_case(method_name)
        params_model = f"{base_name}Params"

        if method_name in result_by_method:
            kind, result_name, annotation = result_by_method[method_name]
            _emit_request_method(
                out,
                method,
                params_model,
                result_kind=kind,
                result_model=result_name,
            )
        elif method_name not in incoming_notifications:
            # Outgoing notification.
            _emit_notification_method(out, method, params_model)

    out.append("__all__ = ['PactoClient', 'PactoClientError']\n")

    output_path.write_text("".join(out), encoding="utf-8")


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------


def repo_root() -> Path:
    """Return the repository root from the script's location."""
    return Path(__file__).resolve().parent.parent.parent


def read_schema() -> dict[str, Any]:
    schema_path = repo_root() / "schemas" / "jsonrpc.json"
    with schema_path.open("r", encoding="utf-8") as f:
        return json.load(f)


def fetch_contract(source: str) -> dict[str, Any]:
    """Load the JSON-RPC contract from a local path or a http(s) URL."""
    parsed = urllib.parse.urlparse(source)
    if parsed.scheme in {"http", "https"}:
        with urllib.request.urlopen(source, timeout=30) as response:  # noqa: S310
            return json.load(response)

    path = Path(source)
    if not path.is_absolute():
        path = repo_root() / path
    with path.open("r", encoding="utf-8") as f:
        return json.load(f)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Generate the pacto-bot-sdk Python SDK from the JSON-RPC contract."
    )
    parser.add_argument(
        "--contract-source",
        default=None,
        help="Path or URL to the JSON-RPC contract artifact (default: schemas/jsonrpc.json).",
    )
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    schema = fetch_contract(args.contract_source) if args.contract_source else read_schema()
    generated_dir = repo_root() / "python" / "src" / "pacto_bot_sdk" / "_generated"
    generate_models(schema, generated_dir / "models.py")
    generate_client(schema, generated_dir / "client.py")
    source = args.contract_source or "schemas/jsonrpc.json"
    print(f"python: generated SDK from {source}")


if __name__ == "__main__":
    main()
