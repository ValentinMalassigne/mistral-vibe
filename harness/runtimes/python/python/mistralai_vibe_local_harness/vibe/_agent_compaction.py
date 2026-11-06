from __future__ import annotations

from pathlib import Path

from pydantic import BaseModel, ConfigDict, Field, ValidationError

from mistralai_vibe_local_harness.protocol import (
    RustProtocolError,
    RustRuntimeBuiltinToolCallAction,
    RustToolFailedEvent,
    RustToolFailureResult,
    RustToolSucceededEvent,
    RustToolSuccessResult,
)
from mistralai_vibe_local_harness.vibe._file_tools import read_file_text
from mistralai_vibe_local_harness.vibe._runtime_config import LocalRuntimeAdapterConfig


class RequestCompactionArgs(BaseModel):
    model_config = ConfigDict(extra="forbid", strict=True)

    handoff_path: str = Field(min_length=1)
    extra_instructions: str = ""
    reason: str | None = None


async def execute_agent_compaction(
    action: RustRuntimeBuiltinToolCallAction,
    config: LocalRuntimeAdapterConfig,
    *,
    additional_read_roots: tuple[Path, ...] = (),
    workspace_read_roots: tuple[Path, ...] = (),
) -> RustToolSucceededEvent | RustToolFailedEvent:
    """Read the handoff note the model wrote before asking to compact.

    The Runtime only reads and validates: whether the Session may compact at all
    is Core's decision, taken when this result arrives, because only Core knows
    the context size and how many compactions the Session already used.

    A refusal here must tell the model what to do next, because the compaction
    runs at the end of the Turn it is still in.
    """
    try:
        args = RequestCompactionArgs.model_validate(action.call.arguments)
    except ValidationError as exc:
        return _failed(action, str(exc))
    try:
        read = await read_file_text(
            args.handoff_path,
            config,
            additional_read_roots=additional_read_roots,
            workspace_read_roots=workspace_read_roots,
        )
    except (OSError, UnicodeError, ValueError) as exc:
        return _failed(action, _unreadable(args.handoff_path, str(exc)))
    if not read.content.strip():
        return _failed(
            action,
            _unreadable(args.handoff_path, "the file is empty"),
        )
    return RustToolSucceededEvent(
        action_id=action.action_id,
        call_id=action.call_id,
        result=RustToolSuccessResult(
            structured_content={
                "handoff_note": read.content,
                "handoff_bytes": read.returned_bytes,
                # The file may be longer than the read returned: the note that
                # survives the compaction is the part Core keeps, and it says so.
                "truncated": read.was_truncated,
            }
        ),
    )


def _unreadable(path: str, reason: str) -> str:
    return (
        f"Cannot read the handoff note at {path}: {reason}. Nothing was compacted. "
        "Write the note first, with what must survive the compaction: the decisions "
        "taken, the files touched and the steps that remain."
    )


def _failed(
    action: RustRuntimeBuiltinToolCallAction, message: str
) -> RustToolFailedEvent:
    return RustToolFailedEvent(
        action_id=action.action_id,
        call_id=action.call_id,
        result=RustToolFailureResult(
            error=RustProtocolError(
                code="tool_failed", message=message, retryable=False, details=None
            )
        ),
    )


__all__ = ["RequestCompactionArgs", "execute_agent_compaction"]
