# Technical plan — Agent-triggered compaction

Target: `main` @ `c96a6dbb` (v3.1.0). Line numbers refer to that commit.
This plan corrects the handoff's code map on three structural points (see §1).

Working rule for this fork: **we follow the repo conventions**. An added feature aligns with
what already exists (defaults, naming, structure, tests) rather than imposing its own choices.
When a real conflict appears between what we want and the convention, we raise it and do not
decide unilaterally.

Read before coding (the repo requires this for any agent capability): `AGENTS.md` at the root,
plus ADR [0002](../../docs/adr/0002-core-engine-and-delivery-surfaces.md) (Core/Runtime
ownership), [0004](../../docs/adr/0004-typed-permissioned-tools.md) (tool contract) and
[0018](../../docs/adr/0018-modular-app-server-capabilities.md) (new capability).

## 1. Actual architecture (and what the handoff got wrong)

```
vibe-rs (Rust TUI, thin client)
   │  app-server JSON-RPC
   ▼
vibe-app-server (Python)  ── config, sessions, UI projections
   │  Step Protocol (HarnessCommand / Action / Observation)
   ▼
Unified Harness Core (Rust, harness/core)  ── state machine, tools, compaction
   ▲
   │  execution actions
Python Runtime (harness/runtimes/python)  ── actually executes the tools
```

Three corrections against the handoff:

| Handoff | Reality |
|---|---|
| The tool is implemented in `harness/core` | Core **declares** the tool (schema + route) but the **Python runtime executes it**. A tool cannot mutate core state; it returns a result that core interprets. |
| Mid-turn compaction = risky option | `TurnEvent::ManualCompaction` is refused when `TurnState::Active` or `Compacting` (`turn.rs:129`). Mid-turn is **impossible** without rewriting the state machine. |
| The trigger is local | `CompactionTrigger` is serialised all the way to the client (`protocol.py:1408,1419,1493`): a new variant touches the Python runtime and the TUI. |

Two pieces of good news: `/compact` already goes through a reusable path
(`HarnessCommand::Compact` → `router.rs:80` → `TurnEvent::ManualCompaction`), and the engine
already knows how to **resume a turn after compaction**
(`CompactionResolution::ContinueTurn`, `turn.rs:250`) — exactly the mechanism needed for the
agent to continue its plan after compacting.

## 2. End-to-end flow

```
1. model calls      request_compaction{handoff_path, extra_instructions, reason}
2. core (assembly) routed the tool → ToolCall Action → runtime
3. runtime: reads the handoff, validates, returns Success{handoff_note, truncated, bytes}
4. core finish_pending_tool (turn.rs:330): runs the guards, then stores
   PendingAgentCompaction{handoff_note, extra_instructions} in HarnessState
5. the model ends its turn (no tool left to call)
6. core apply_tool_batch_transition (turn.rs:701), ContinueTurn branch:
   instead of continuing the turn, if pending → start_compaction(CompactionRequest::Agent{..})
7. compaction: summary, history replacement (compacted(), execution.rs:198)
8. apply_compaction_resolution → ContinueTurn (turn.rs:250):
   the handoff note is injected into the context, then continue_after_compaction
9. the agent receives summary + note and continues with the next phase
```

Step 6 is the only real insertion point in the state machine. It reuses
`apply_in_turn_compaction_start` (`turn.rs:446`), already written for in-turn auto-compaction.

## 3. Core changes (Rust)

### 3.1 `harness/core/src/core/features/agent_compaction.rs` — new

Modelled on `model_wait.rs` (same role: a tool gated by settings).

```rust
//! Agent-requested compaction: the settings, the guards and the tool contract.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::core::config::ToolSettings;
use crate::core::step_protocol::ToolDefinition;

pub(crate) const DEFAULT_MIN_INPUT_TOKENS: u64 = 20_000;
pub(crate) const DEFAULT_MAX_PER_SESSION: u32 = 8;
pub(crate) const DEFAULT_MIN_GROWTH_TOKENS: u64 = 5_000;
pub(crate) const DEFAULT_MAX_HANDOFF_BYTES: u64 = 8_192;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "min_input_tokens")]
    pub min_input_tokens: u64,
    #[serde(default = "max_per_session")]
    pub max_per_session: u32,
    #[serde(default = "min_growth_tokens")]
    pub min_growth_tokens: u64,
    #[serde(default = "max_handoff_bytes")]
    pub max_handoff_bytes: u64,
}

impl Default for Settings { /* enabled: false, everything else at its default */ }

impl Settings {
    pub(crate) fn is_default(&self) -> bool { *self == Self::default() }
    pub(crate) fn is_offered(&self) -> bool { self.enabled }
}

/// The structured result the runtime returns after reading the handoff.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HandoffReply {
    pub handoff_note: String,
    #[serde(default)]
    pub truncated: bool,
}

/// Why a request was refused. Every variant produces an actionable message:
/// the model reads it and must be able to correct itself.
pub(crate) enum Refusal {
    Disabled,
    BelowFloor { estimated: u64, floor: u64 },
    InsufficientGrowth { grown: u64, required: u64 },
    QuotaReached { done: u32, max: u32 },
    AlreadyPending,
}

impl Refusal {
    pub(crate) fn message(&self) -> String { /* … */ }
}

pub(crate) fn tool(description: &str, settings: &Settings) -> ToolDefinition {
    /* JSON schema below; `description` comes from the prompts, cf. §3.1b */
}
```

Tool schema:

```json
{
  "type": "object",
  "additionalProperties": false,
  "properties": {
    "handoff_path":   { "type": "string", "minLength": 1 },
    "extra_instructions": { "type": "string" },
    "reason":         { "type": "string", "maxLength": 200 }
  },
  "required": ["handoff_path"]
}
```

### 3.1b The tool description lives in the prompts — repo convention

`wait` does not hardcode its description: it sits in `core/prompt/default_prompts.json` and can
be overridden through `settings.prompts.wait_description`. We follow the same rule, otherwise
this will be the only tool in the catalogue that is not customisable
(`prompt/settings.rs:38`). Four additional touch points:

1. `core/prompt/default_prompts.json`: a `request_compaction_description` entry (English text,
   same tone as `wait_description`).
2. `core/prompt/settings.rs`: a `pub request_compaction_description: String` field in
   `PromptSettings`.
3. `PromptSettings::tool_text_changed` (line 62): add the comparison — without it a description
   change does not revoke the tool catalogue.
4. `PromptSettings::validate` (line 70): add the pair to the "prompt must not be empty" check.

Proposed text (goes into `default_prompts.json`):

> Compact the conversation to free context. Use it when your current plan says to compact
> before continuing, or when you are about to start a long phase that needs room. Before
> calling it, write what must survive the compaction (decisions taken, files touched,
> remaining steps) to `handoff_path`. The compaction runs when your turn ends and the
> conversation resumes right after, with your handoff note re-injected.

### 3.2 `core/config.rs` — `ToolSettings` (line 277)

```rust
#[serde(default, skip_serializing_if = "agent_compaction::Settings::is_default")]
pub agent_compaction: agent_compaction::Settings,
```

`ToolSettings` has `deny_unknown_fields`: harmless here since core and runtime are rebuilt
together in the fork. For an upstream PR, this is the compatible direction (old config, new
core), cf. ADR 0014.

### 3.3 `core/tools/external.rs`

- `RuntimeBuiltinToolName::AgentCompactionRequest` (new variant).
- `as_str()` → `"request_compaction"`.
- `from_str` (around line 87) → the inverse name.
- `direct_name()` (lines 183 and 213: the `Some(...)` arm) → `Some("request_compaction")`.
- `hook_target()` (line 243) → `HookToolTarget::SelfTool`. Deliberate choice: this is an
  internal Core tool, and `self` is already the group for internal control tools (`self.wait`,
  `self.sleep`). Avoids touching the user-facing hooks contract.
- `checkpoint/v1/schema/program.rs` only needs touching if the tool must be callable from a
  TypeScript program. **Not retained in v1** → no checkpoint change.

### 3.4 `core/tools/resolved/assembly.rs`

Two insertions, exactly where `wait` is added:

- `builtin_direct_tools` (line 215): when `is_offered()`,
  `tools.push(agent_compaction::tool(&config.settings.prompts.request_compaction_description, &config.settings.tools.agent_compaction))`
  — same shape as the `model_wait::tool` call right above it.
- `direct_routes` (line 27): when `is_offered()`, route `"request_compaction"` to
  `ToolTarget::RuntimeBuiltin { name: AgentCompactionRequest }`.

Effect: flag disabled → the tool exists neither in the model catalogue nor in the routing.

### 3.5 `core/features/compaction/mod.rs` — `CompactionTrigger` (line 145)

```rust
pub(crate) enum CompactionTrigger {
    Manual,
    Automatic,
    Agent,
}
```

Serialised in snake_case → `"agent"`.

### 3.6 `core/features/compaction/execution.rs`

`CompactionContinuation` (line 25) must carry the trigger for the in-turn case:

```rust
enum CompactionContinuation {
    InTurn { turn_id: String, iterations: u32, trigger: CompactionTrigger },
    BetweenTurns { trigger: CompactionTrigger },
}
```

`trigger()` (line 85) simply returns the field. `in_turn_identity()` (line 92) is unchanged:
the `(turn_id, iterations)` pair remains the resume identity.

`CompactionRequest` (line 494):

```rust
pub(crate) enum CompactionRequest<'a> {
    Automatic { turn_id: &'a str, iterations: u32 },
    Manual { extra_instructions: &'a str },
    Agent { turn_id: &'a str, iterations: u32, extra_instructions: &'a str },
}

impl<'a> CompactionRequest<'a> {
    pub(crate) fn agent(turn_id: &'a str, iterations: u32, extra_instructions: &'a str) -> Self { … }
}
```

`start_compaction` (line 512): the `Agent` branch produces
`CompactionContinuation::InTurn { turn_id, iterations, trigger: CompactionTrigger::Agent }` —
an **in-turn** continuation (the agent carries on) with the `agent` trace.

`PendingCompaction::in_turn` (line 31) passes `trigger: CompactionTrigger::Automatic` for
auto-compaction.

`compacted()` (line 198) is unchanged: the note is injected from `turn.rs` (§3.7), so the
compaction module does not depend on session state.

### 3.7 `core/turn.rs` + `core/state.rs`

New state (transient, as compaction state already is: nothing of compaction appears in the
checkpoint schema):

```rust
pub(crate) struct PendingAgentCompaction {
    pub handoff_note: String,
    pub extra_instructions: String,
    pub reason: Option<String>,
}

// in HarnessState
pub pending_agent_compaction: Option<PendingAgentCompaction>,
pub agent_compactions_done: u32,
pub agent_compaction_baseline_tokens: Option<u64>,
```

**(a) The guards run when the result arrives, not at the end of the turn**

A firm decision, and not cosmetic: if the refusal only happened at the end of the turn, the tool
would already have answered "accepted" to the model, which would carry on believing its context
was compacted. The refusal must be an **immediate tool failure**, read and acted upon by the
model within the same turn.

Insertion point: `finish_pending_tool` (`turn.rs:330`). It already has everything —
`&mut HarnessState`, `call_id`, the batch via `ActivePhase::AwaitingToolBatch`, and the
validated result (lines 337-353).

```rust
fn finish_pending_tool(state, action_id, call_id, report, determinism) -> … {
    // … existing validation …
    let mut result = /* validated result */;

    if let Some(call) = batch.call(&call_id) {          // accessor to add, see below
        if call.name == "request_compaction" {
            match admit_agent_compaction(state, settings) {
                Ok(()) => {
                    let handoff = parse_handoff(&result)?;   // HandoffReply ← structured_content
                    state.pending_handoff_note = Some(handoff.handoff_note.clone());
                    state.pending_agent_compaction = Some(PendingAgentCompaction {
                        handoff_note: handoff.handoff_note,
                        extra_instructions: call.arguments…,   // see note below
                        reason: …,
                    });
                }
                Err(refusal) => {
                    // The model sees the refusal right away; its turn continues.
                    result = ToolResult::Failure {
                        content: vec![],
                        structured_content: StructuredContent::Absent,
                        meta: None,
                        error: ProtocolError {
                            code: "tool_failed".to_string(),
                            message: refusal.message(),
                            retryable: false,
                            details: None,
                        },
                    };
                }
            }
        }
    }
    // … unchanged: finish_tool_batch_action(tools, batch, &action_id, result, …) …
}
```

Two implementation details:

- `ToolBatch` does not expose a call's name by `call_id`: add
  `pub(crate) fn call(&self, call_id: &str) -> Option<&ToolCall>`. This is the only API addition
  to the batch module — `batch.rs` stays pure and no existing signature changes.
- `extra_instructions` and `reason` live in the call's **arguments**, not in the result. Read
  them from `call.arguments` (JSON validated by the schema) rather than trusting the runtime to
  echo them back.

`parse_handoff` fails cleanly (`CoreError::invalid_command`) when the runtime returns a
non-conforming `structured_content`: an old or buggy runtime must not corrupt the state.

**(b) The guard**

```rust
fn admit_agent_compaction(state: &HarnessState, settings: &Settings) -> Result<(), Refusal>
```

Sources, all already present:
- estimated context size: `estimate_agent_request_tokens(state)` (`request_size.rs:179`);
- growth since the last agent compaction: `agent_compaction_baseline_tokens` compared against
  `state.last_reported_context_tokens`;
- quota: `agent_compactions_done`;
- "already pending": `pending_agent_compaction.is_some()`, and
  `matches!(state.turn, TurnState::Compacting { .. })`.

A refusal leaves the state intact: no compaction, no recorded intent, the turn continues
normally.

**(c) Triggering** — `orchestration.rs` already exposes the state wrapper
`start_manual_compaction(state, extra_instructions)` (line 36), which assembles
`generated_system`, `compaction_inputs` and `compaction_count`. We add its twin:

```rust
pub(crate) fn start_agent_compaction(
    state: &mut HarnessState,
    turn_id: &str,
    iterations: u32,
    extra_instructions: &str,
) -> Result<CompactionStartResolution, CoreError> {
    start(state, CompactionRequest::agent(turn_id, iterations, extra_instructions))
}
```

Then, in `apply_tool_batch_transition` (`turn.rs:701`), `ToolBatchResolution::ContinueTurn`
branch (line 718), before `continue_active_turn`:

```rust
if let Some(pending) = state.pending_agent_compaction.take() {
    let (turn_id, iterations) = {
        let active = state.require_active()?;
        (active.turn_id.clone(), active.iterations)
    };
    let start = start_agent_compaction(
        state, &turn_id, iterations, &pending.extra_instructions,
    )?;
    state.agent_compactions_done += 1;
    state.agent_compaction_baseline_tokens = state.last_reported_context_tokens;
    state.pending_handoff_note = Some(pending.handoff_note);
    return Ok(outcome.after(apply_in_turn_compaction_start(state, start)?));
}
```

Mind the order: `apply_in_turn_compaction_start` installs `ActivePhase::AwaitingCompaction`,
so `pending_handoff_note` is set before it and must survive until the resume.

Implementation note: `turn.rs` already imports this family of functions under an alias
(`turn.rs:23`, `start_manual_compaction as start_manual_compaction_request`) because the file has
its own `fn start_manual_compaction`. Keep the same convention.

**(d) Re-injecting the note** — `apply_compaction_resolution` (`turn.rs:245`),
`CompactionResolution::ContinueTurn` branch: when an agent note is pending, inject it before
`continue_after_compaction`:

```rust
if let Some(note) = state.pending_handoff_note.take() {
    state.context.push_before_next_turn(StoredMessage::injected(...))?;
}
```

`push_before_next_turn` is already used by `handle_context_message` (`turn.rs:761`) for context
injection: same mechanics, so the note enters the persisted history right after the summary.
Drop it when the compaction fails (the `FailTurn` branch): on failure the original history is
kept and the note has no reason to be injected.

### 3.8 `BetweenTurns` compaction: unchanged

`/compact` stays exactly on its current path. Only difference: `trigger()` of a manual
compaction remains `Manual`.

## 4. Python Runtime changes

### 4.1 `harness/runtimes/python/python/mistralai_vibe_local_harness/`

- `protocol.py`:
  - `RustRuntimeBuiltinToolName` (line 34) and `RUNTIME_BUILTIN_TOOL_NAMES` (line 61): add
    `"request_compaction"`.
  - The four `Literal["automatic", "manual"]` (lines 1020, 1408, 1419, 1493): add `"agent"`.
  - Line 1050, the `if self.trigger == "automatic" and self.turn_id is not None` condition must
    include `"agent"`: the agent trigger also carries a `turn_id` (in-turn continuation).
- `vibe/_agent_compaction.py` — new: the tool execution. The runtime **only** reads the file and
  validates the arguments: no guards here (§3.7a), it knows neither the tokens nor the quota.
  ```python
  async def execute_agent_compaction(action, *, config, max_bytes: int) -> Event
  ```
  - **reuse the existing machinery**, do not rewrite path resolution: `_read_roots(config)`,
    `_path_policy(…)` and `_is_within(path, roots)` in `vibe/_file_tools.py:323,339,368` are
    exactly the rules of `file_system.read_file` (allowed roots, symlinks). A home-grown
    resolution would diverge from the sandbox.
  - refuse outside the roots, missing, empty, non-regular — with a message telling the agent to
    write the note first;
  - read at most `max_bytes`, truncate cleanly (UTF-8 boundary), return
    `Success{handoff_note, handoff_bytes, truncated}`.
- `vibe/_runtime.py` (~line 2750, next to `execute_model_wait` / `self.sleep`): a dispatch arm
  for `request_compaction`.
- `vibe/_hook_matcher.py` (line 63): `"request_compaction": ("request_compaction",)`.
- `session_api/app_server/session/models/configuration.py` (next to the `tools.self.sleep`
  description): expose the new settings block.

### 4.2 `vibe/` (app server and config)

- `vibe/core/config/models.py`: an `AgentCompactionSettings` Pydantic model (`enabled`,
  `min_input_tokens`, `max_per_session`, `min_growth_tokens`, `max_handoff_bytes`), wired into
  the existing `Tools` model.
- `vibe/core/config/_defaults.py` + `vibe/core/config/vibe_schema.py`: defaults and schema.
- `vibe/app_server/_runtime.py` (~1719, where `model_wait=RustEnabledModelWait()` is set): map
  the config to `RustToolSettings.agent_compaction`.
- `vibe/app_server/_unified_tool_projection.py` (~1807, next to `"self.sleep"`):
  `"request_compaction": _ToolLabel("Compacting", "", "Compacted")`.
- `vibe/app_server/_session_api_commands.py`: nothing to change; `compact()` stays the manual
  command.

### 4.3 `vibe/cli-rust/`

Nothing mandatory in v1: the compaction status row is already painted from the checkpoints, and
the trigger is not displayed there. To surface the origin explicitly, touch
`ui/transcript/entry/status.rs:21` plus a `client-e2e` scenario and its golden. A second step.

## 5. Tests

### 5.0 Commands — note that `make` does not cover the harness

The root `Makefile` drives **only the TUI crate**
(`M = --manifest-path vibe/cli-rust/Cargo.toml`). `make test_rust` will therefore test *nothing*
of the harness. For `harness/core`:

```bash
cd harness/core
cargo nextest run          # full suite (0.9.146 installed)
cargo test                 # fallback if nextest is unavailable
cargo fmt && cargo clippy --all-targets -- -D warnings
```

The TUI crate has its own rules (`vibe/cli-rust/AGENTS.md`): `make fmt`, `make lint`,
`make test_rust` from `vibe/`. Do not mix the two scopes.

### 5.1 Code conventions

- Code, comments, test names and commit messages in **English**: that is the repo language.
- `harness/core` uses inline test modules (`#[cfg(test)] mod tests`) and
  `src/tests/acceptance_scenarios/` — acceptable here. The ban on inline tests only applies to
  `vibe/cli-rust`.
- Rust: `edition = 2024`, no `unwrap()` in production code.
- Python: `uv run ruff check --fix .`, `uv run ruff format .`, `uv run pyright` (strict) after
  every change — AGENTS.md rule.

### 5.2 Tests to write

Priority order, alongside the code:

1. `harness/core/src/tests/acceptance_scenarios/compaction/agent.rs` (new, registered in
   `compaction/mod.rs`), modelled on the neighbouring files:
   - plan → handoff → tool → end of turn → compaction → resume within the same turn;
   - the context after the resume holds the summary **then** the note;
   - missing handoff → tool failure, no compaction, the turn continues;
   - below the floor → refusal;
   - quota reached → refusal;
   - two calls in the same turn → a single compaction;
   - compaction failure (summary rejected) → original history, turn resumed;
   - flag off → the tool is not in the catalogue (`builtin_direct_tools`).
2. Core unit tests for `admit_agent_compaction` and `Refusal::message`.
3. Python: mirroring `tests/` for `_agent_compaction.py` (read, truncation, path escape, empty
   file).
4. `vibe/`: test for the config → runtime settings mapping.

## 6. Implementation order

Each step compiles and passes its tests before the next.

1. Settings + **description in the prompts** + declared tool + route, flag off: verify the
   catalogue is unchanged when the flag is `false`.
2. `CompactionTrigger::Agent` + `CompactionRequest::Agent` + `InTurn` continuation carrying the
   trigger, without the tool: add a unit test that starts an agent compaction directly.
3. Tool active: schema, route, runtime execution (handoff read via `_file_tools`), note
   returned.
4. Success detection in `finish_pending_tool` + `PendingAgentCompaction` +
   `ToolBatch::call` accessor.
5. Turn resume at the end of the turn (`apply_tool_batch_transition`).
6. Note re-injection after the summary.
7. `admit_agent_compaction` and the refusals — wire this before any real use: without this
   step, the tool accepts everything.
8. `vibe/` config + UI projection.
9. Full acceptance tests.

Do not skip step 7 before testing by hand: a tool that promises a compaction it never performs
is worse than no tool at all.

## 7. Two commands side by side: `vibe` and `vibe-fork`

Constraint: `vibe` must keep launching the official Vibe. The fork therefore installs under a
second name.

`[project.scripts]` exposes `vibe-fork = "vibe.cli.launcher:main"` — same entry point, a purely
additive change. The launcher is name-agnostic: it does not read `argv[0]` to pick the binary
but resolves it from the package (`launcher.py:68`), and looks for the app server as a
**sibling of the resolved `sys.argv[0]`** (`launcher.py:88`). A `vibe-fork` entry installed in
its own venv is therefore self-contained: it finds its `vibe-app-server` in the same `bin/`.

### Installation

Do **not** use `uv tool install`: it would also write `vibe` into `~/.local/bin` and replace the
official one. A dedicated venv, then a single symlink:

```bash
uv venv ~/.vibe-fork/venv
uv pip install --python ~/.vibe-fork/venv/bin/python /Users/valentin/Code/Tests/mistral-vibe-fork
ln -sfn ~/.vibe-fork/venv/bin/vibe-fork ~/.local/bin/vibe-fork
```

`vibe` stays the official one, `vibe-fork` launches the fork. After a Rust change, rerun the
first two lines.

### Development

```bash
uv sync --all-extras
uv run vibe     # inside the repo, installing nothing on the PATH
```

The Python build (`build_backend/maturin_backend.py`) compiles **both** Rust binaries: the
harness extension (`harness/core` → `mistralai_vibe_local_harness._native`) and the TUI
(`vibe/cli-rust` → `vibe/_bin/vibe-rs`). Without `cargo` it fails explicitly
(`maturin_backend.py:101`).

A non-editable build is assumed: every Rust change requires a rebuild anyway, so trigger it
explicitly rather than at import time.

### Toolchain (installed, verified)

| Item | Version |
|---|---|
| `rustc` / `cargo` | 1.97.1 aarch64-apple-darwin, minimal profile |
| components | `rustfmt`, `clippy`, `llvm-tools` |
| `cargo-nextest` | 0.9.146 (prebuilt, the version pinned by AGENTS.md) |
| `uv` | 0.11.7 (already present) |

Installed through `rustup` with `--no-modify-path`: `$HOME/.cargo/bin` is not on the global
`PATH`. To call `cargo` directly, `source "$HOME/.cargo/env"`.

### Shared config

Both commands read the same `~/.vibe/config.toml`: API key, models and theme are shared, nothing
to duplicate.

```toml
[tools.agent_compaction]
enabled = true
```

A consequence to be aware of: the official Vibe does not know this section. It does not crash —
the schema tolerates unknown keys (`extra="ignore"`, cf. the comment in `config/models.py:107`)
— but it prints "Config setting ignored: tools.agent_compaction" (`vibe_schema.py:489`). With
`strict_config = true` it becomes a config-load error instead. If the noise becomes real, the
way out is a separate `VIBE_HOME` for the fork; to be decided when it matters.

### Authentication and billing

The fork resolves credentials through the same code path as the official build
(`vibe/utils/api_keys.py:92`), in the same order: stored OIDC login, then session credentials,
then `MISTRAL_API_KEY`, then the keyring. Same account, same API base URL, same billing
endpoints (`/v1/billing/subscription`, `/v1/billing/budget`). Nothing identifies the fork as a
distinct client, so usage is attributed and debited exactly as with the official Vibe. Which
meter is used — Vibe plan allowance through the OIDC login, or platform API usage through an
API key — depends on which credential you already use today, and the fork does not change that.

### Auto-update

The user's config may have `enable_auto_update = true`. The updater acts through
`uv tool upgrade mistral-vibe` / `uv tool install mistral-vibe` (`update_flow.rs:94`), i.e. it
only ever targets a uv tool install of the *official* package. The fork lives in its own venv
and is not a uv tool, so it cannot update itself — and the official `vibe` is untouched. The
only annoyance is an update prompt that offers the official build; nothing destructive.

## 8. Fork / upstream workflow

Already in place: `origin` = `ValentinMalassigne/mistral-vibe`, `upstream` =
`mistralai/mistral-vibe`, branch `feat/agent-compact` on `main` @ v3.1.0.

```bash
git fetch upstream && git checkout main && git merge --ff-only upstream/main
git push origin main
git checkout feat/agent-compact && git rebase main
```

The diff touches ~10 Rust files and ~6 Python files, all localised. The risk is not the size but
the `apply_tool_batch_transition` hook and `CompactionContinuation`, which are active code.

The `vibe-fork` entry in `[project.scripts]` is a **fork-only** convenience: remove it if you
open an upstream PR, since it only exists to let the two commands coexist.

The banner in `vibe/cli-rust/src/ui/banner/mod.rs` reads `Mistral Vibe-Fork` for the same
reason: it tells the two installed commands apart at a glance. Revert it, and the scenarios
that assert its text (`client-e2e/scenarios/banner/lechonk.py`,
`text_selection/banner_selection.py`, `conversation/subagent_view_empty.py`) with the goldens
they own, before an upstream PR.

## 9. Estimate

| Step | Volume |
|---|---|
| Settings + tool + route | ~120 lines Rust |
| Trigger + `CompactionRequest::Agent` | ~60 lines Rust |
| State + detection + resume + note | ~150 lines Rust |
| Guards | ~100 lines Rust |
| Python runtime (execution + protocol) | ~120 lines Python |
| App server config + projection | ~80 lines Python |
| Tests | ~400 lines |

About 1,000 lines in total, half of it tests. 10 Rust files, 6 Python files.

## 10. Risks

| Risk | Mitigation |
|---|---|
| Upstream conflict on `turn.rs` / `compaction/execution.rs` | minimal diff, frequent rebases, no opportunistic refactor |
| Badly ordered resume (note before or after the summary) | dedicated acceptance test on the context order |
| Compaction loop | minimum growth plus quota, not the quota alone |
| Oversized note | byte bound, truncation reported to the model (`truncated: true`) |
| Model calling the tool without a handoff | required parameter + actionable refusal, the model corrects itself |
| Transient state lost on session resume | accepted: compaction state is not checkpointed either; so is the tool result |

## 11. Corrections found while implementing

The plan was written by reading the code. Implementing it proved nine points wrong or
incomplete. They are recorded here rather than edited into the sections above, so the plan
stays a record of what was known before coding.

| Section | What the plan said | What the code required |
|---|---|---|
| §3.3 | `checkpoint/v1/schema/program.rs` needs no change | `CheckpointRuntimeBuiltinToolName::capture` matches exhaustively over `RuntimeBuiltinToolName`, so a new tool forces a schema entry: a pending call must be checkpointable like any other |
| §3.6 | `InTurn` carries the trigger | The checkpoint had to record it too: without it a restored agent compaction is relabelled automatic. `AwaitingCompaction.trigger` defaults to `automatic`, so older checkpoints keep their meaning |
| §3.1, §7 | `Settings { enabled: bool }`, `[tools.agent_compaction]` in `config.toml` | Core has no boolean toggle: activation is a tagged `Mode` enum. Harness tool settings are not user-facing, and `tools.*` already means custom tool definitions. The user chose the repository convention: `Mode` in Core, a flat `enable_agent_compaction` in the config, bounds as Core constants |
| §4.2 | A `Tools` Pydantic model in `config/models.py` | No such model exists; the setting lives in `VibeConfigSchema` |
| §3.7 | The baseline is `last_reported_context_tokens` taken before the compaction | Taken before, it would make every later request look like it had not grown. It is measured when the Turn resumes |
| §3.7(d) | `push_before_next_turn` re-injects the note | The resumed Turn owns the current group, so `push_in_current_turn` puts the note between the summary and the next request |
| §4.1 | `vibe/_agent_compaction.py` resolves the path with `_read_roots` / `_is_within` | Reading through `read_file` itself is the only way to follow a sandbox, symlinks and approvals. The runtime returns the text; Core owns `MAX_HANDOFF_BYTES` and truncates |
| §4.1 | A dispatch arm in `vibe/_runtime.py` around line 2750 | Runtime built-ins dispatch in `vibe/_local_actions.py`; `_runtime.py` only special-cases waiting and sleeping |
| §5.0 | `cargo nextest run` runs the harness suite | It does not compile: `harness/core/tests/` is not published, and six `include_str!` sites need it. The V1 fixtures are regenerable with `UPDATE_CHECKPOINT_FIXTURES=1`; the retired-agent ones are not |

Python runtime tests live in `harness/runtimes/python/tests/`, next to the runtime they
cover: the repository root `tests/` mirrors the app server, and the runtime ships without its
own suite.
