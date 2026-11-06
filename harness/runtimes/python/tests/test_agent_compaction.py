from __future__ import annotations

import asyncio
from pathlib import Path

import pytest

from mistralai_vibe_local_harness.protocol import RustRuntimeBuiltinToolCallAction
from mistralai_vibe_local_harness.vibe._agent_compaction import (
    execute_agent_compaction,
)
from mistralai_vibe_local_harness.vibe._runtime_config import (
    LocalRuntimeAdapterConfig,
    SessionWorkspace,
)


def workspace(tmp_path: Path) -> LocalRuntimeAdapterConfig:
    return LocalRuntimeAdapterConfig(
        workspace=SessionWorkspace(cwd=tmp_path, roots=(tmp_path,))
    )


def action(handoff_path: str) -> RustRuntimeBuiltinToolCallAction:
    return RustRuntimeBuiltinToolCallAction.model_validate(
        {
            "type": "runtime_builtin_tool_call",
            "action_id": "action",
            "turn_id": "turn",
            "call_id": "call",
            "call": {
                "name": "request_compaction",
                "arguments": {"handoff_path": handoff_path},
            },
        }
    )


def run(config: LocalRuntimeAdapterConfig, handoff_path: str) -> dict[str, object]:
    event = asyncio.run(execute_agent_compaction(action(handoff_path), config))
    return event.model_dump(mode="json")["result"]


def test_the_note_is_the_file_the_model_wrote(tmp_path: Path) -> None:
    note = tmp_path / "handoff.md"
    note.write_text("Finish step 3.\n")

    result = run(workspace(tmp_path), str(note))

    assert result["type"] == "success"
    assert result["structured_content"] == {
        "handoff_note": "Finish step 3.\n",
        "handoff_bytes": 15,
        "truncated": False,
    }


def test_a_file_longer_than_the_read_reports_itself_truncated(
    tmp_path: Path,
) -> None:
    note = tmp_path / "handoff.md"
    note.write_text("\n".join(f"line {index}" for index in range(5_000)))

    result = run(workspace(tmp_path), str(note))

    content = result["structured_content"]
    assert result["type"] == "success"
    assert content["truncated"] is True
    assert content["handoff_bytes"] < note.stat().st_size


def test_an_empty_note_is_refused_before_anything_is_compacted(
    tmp_path: Path,
) -> None:
    note = tmp_path / "handoff.md"
    note.write_text("  \n\n")

    result = run(workspace(tmp_path), str(note))

    assert result["type"] == "failure"
    assert "Nothing was compacted" in result["error"]["message"]


def test_a_path_outside_the_workspace_is_refused(tmp_path: Path) -> None:
    outside = Path("/etc/hosts")

    result = run(workspace(tmp_path), str(outside))

    assert result["type"] == "failure"
    assert "Nothing was compacted" in result["error"]["message"]


def test_a_missing_note_is_refused(tmp_path: Path) -> None:
    result = run(workspace(tmp_path), str(tmp_path / "handoff.md"))

    assert result["type"] == "failure"
    assert "Nothing was compacted" in result["error"]["message"]


@pytest.mark.parametrize("handoff_path", ["", "  "])
def test_a_blank_path_never_reaches_the_file_system(
    tmp_path: Path, handoff_path: str
) -> None:
    result = run(workspace(tmp_path), handoff_path)

    assert result["type"] == "failure"
