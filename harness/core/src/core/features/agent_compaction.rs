//! Agent-requested compaction: the setting, the tool contract and the bounds.
//!
//! The model calls `request_compaction` on itself, once, with a handoff note it
//! has written. Core honours the request when the Turn ends: compaction cannot
//! start while the Turn is active, and the Turn resumes right after the summary.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::step_protocol::ToolDefinition;

/// Below this input size, compacting frees less than the resume re-sends: the
/// summary request and the continuation both re-read the conversation.
pub(crate) const MIN_INPUT_TOKENS: u64 = 20_000;

/// A Session compacts this many times at most, so a model that keeps asking
/// cannot trade the whole conversation for summaries.
pub(crate) const MAX_PER_SESSION: u32 = 8;

/// Two compactions with too little new context in between only lose detail.
pub(crate) const MIN_GROWTH_TOKENS: u64 = 5_000;

/// The handoff note re-enters the context after the summary, so bound it.
pub(crate) const MAX_HANDOFF_BYTES: u64 = 8_192;

/// Whether Core offers the tool. Off unless the Host turns it on: the tool
/// changes what the model can decide about its own context.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Mode {
    #[default]
    Disabled,
    Enabled,
}

impl Mode {
    pub(crate) fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

pub(crate) fn tool(description: &str) -> ToolDefinition {
    ToolDefinition {
        name: "request_compaction".to_string(),
        description: description.to_owned(),
        parameters: input_schema(),
    }
}

/// `handoff_path` is required on purpose: a compaction without a note would
/// throw away the plan the model is following, and re-injecting the note is
/// what makes the compaction non-destructive.
pub(crate) fn input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "handoff_path": {"type": "string", "minLength": 1},
            "extra_instructions": {"type": "string"},
            "reason": {"type": "string", "maxLength": 200},
        },
        "required": ["handoff_path"],
    })
}

pub(crate) fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "handoff_note": {"type": "string"},
            "handoff_bytes": {"type": "integer", "minimum": 0},
            "truncated": {"type": "boolean"},
        },
        "required": ["handoff_note", "handoff_bytes", "truncated"],
    })
}
