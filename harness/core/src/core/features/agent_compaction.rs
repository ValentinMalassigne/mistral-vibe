//! Agent-requested compaction: the setting, the tool contract and the bounds.
//!
//! The model calls `request_compaction` on itself, once, with a handoff note it
//! has written. Core honours the request when the Turn ends: compaction cannot
//! start while the Turn is active, and the Turn resumes right after the summary.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::core::error::CoreError;
use crate::core::step_protocol::ToolDefinition;
use crate::core::wire::tool::StructuredContent;

/// The name the model calls, and the name Core routes.
pub(crate) const TOOL_NAME: &str = "request_compaction";

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
        name: TOOL_NAME.to_string(),
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

/// What the Runtime answers: the note it read, how many bytes of it, and
/// whether the file held more than the read returned.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffReply {
    handoff_note: String,
    handoff_bytes: u64,
    truncated: bool,
}

/// The compaction the model asked for, kept on the state until the Turn ends
/// and consumed when the Turn resumes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pending {
    pub handoff_note: String,
    pub extra_instructions: String,
}

/// Why a request was refused. Each message tells the model what to do instead:
/// it reads the refusal inside the Turn it wanted compacted.
pub(crate) enum Refusal {
    BelowFloor { estimated: u64, floor: u64 },
    InsufficientGrowth { grown: u64, required: u64 },
    QuotaReached { done: u32, max: u32 },
    AlreadyPending,
}

impl Refusal {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::BelowFloor { estimated, floor } => format!(
                "Nothing was compacted: this request holds about {estimated} input tokens, \
                 below the {floor} at which a compaction frees more context than it costs. \
                 Keep working and ask again once the conversation is longer."
            ),
            Self::InsufficientGrowth { grown, required } => format!(
                "Nothing was compacted: only {grown} tokens were added since the last \
                 compaction, below the {required} that make another one worth its cost. \
                 Keep working with the summary you have."
            ),
            Self::QuotaReached { done, max } => format!(
                "Nothing was compacted: this session already compacted {done} times, its \
                 limit of {max}. Finish with the context you have."
            ),
            Self::AlreadyPending => "Nothing was compacted: this Turn already has a \
                 compaction scheduled. Do not call this tool again; end your Turn."
                .to_string(),
        }
    }
}

/// The note to re-inject, and whether it is shorter than what the model wrote.
///
/// A Runtime answers another shape only if it is another Runtime, so a
/// non-conforming reply is an error rather than a silent empty note.
pub(crate) fn read_reply(
    structured_content: &StructuredContent,
) -> Result<(String, bool), CoreError> {
    let value = structured_content.as_value().ok_or_else(|| {
        CoreError::invalid_command("request_compaction answered without a handoff note")
    })?;
    let reply: HandoffReply = serde_json::from_value(value.clone()).map_err(|error| {
        CoreError::invalid_command(format!(
            "request_compaction answered an unexpected shape: {error}"
        ))
    })?;
    if reply.handoff_note.trim().is_empty() {
        return Err(CoreError::invalid_command(
            "request_compaction answered an empty handoff note",
        ));
    }
    let cut = reply.truncated || reply.handoff_bytes > MAX_HANDOFF_BYTES;
    let (note, shortened) = bounded(&reply.handoff_note);
    Ok((note, cut || shortened))
}

/// The request's own instructions, which shape the summary prompt.
pub(crate) fn extra_instructions(arguments: &Value) -> String {
    arguments
        .get("extra_instructions")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Bounds the note Core re-injects after the summary, on a character boundary
/// so the model reads valid UTF-8.
fn bounded(note: &str) -> (String, bool) {
    let limit = MAX_HANDOFF_BYTES as usize;
    if note.len() <= limit {
        return (note.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !note.is_char_boundary(end) {
        end -= 1;
    }
    (note[..end].to_string(), true)
}
