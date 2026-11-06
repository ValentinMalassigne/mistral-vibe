# Spec — Agent-triggered compaction

Status: proposal validated against `main` @ v3.1.0 (`c96a6dbb`).
Branch: `feat/agent-compact`. Fork: `ValentinMalassigne/mistral-vibe`.

## 1. Goal

Today `/compact` is a **user** command: the human decides to compact. Automatic compaction is
purely reactive (a token threshold). The middle case is missing: **the agent knows** it is
reaching a breaking point and wants to compact itself, at the right moment.

Target use case: the agent follows a plan (`plan.md` or equivalent) containing an explicit step
such as "compact the context before moving to the next phase". Today the agent can only say so
in prose and wait for the human to type `/compact`, which breaks the flow.

So the agent can request a compaction, supplying a **handoff note** (a checkpoint) that survives
the compaction. Compaction becomes non-destructive: the summary replaces the history, the note
re-injects what matters for what comes next.

## 2. Non-goals

- No agent-initiated compaction mid-turn (see §4.1: the state machine forbids it).
- No replacement of threshold-based auto-compaction: both coexist.
- No general agent access to slash commands. One narrow entry point only.
- No change to the summary prompt from the agent side: it cannot rewrite the compaction
  instruction, only add free-form instructions.

## 3. Expected behaviour

```
agent follows plan.md
  └─ step "compact before phase 2"
       ├─ 1. agent writes its checkpoint (write_file) → .vibe/handoff-phase2.md
       ├─ 2. agent calls request_compaction(handoff_path=..., extra_instructions=...)
       │       └─ the harness validates, holds the intent as pending
       ├─ 3. the agent ends its turn (no tool left to call)
       ├─ 4. the harness runs the compaction (trigger = agent)
       │       ├─ the model summarises the history
       │       ├─ the history is replaced by the summary
       │       └─ the checkpoint is re-injected right after the summary
       └─ 5. the turn resumes: the agent receives summary + note and continues phase 2
```

Key points:

- **Step 5 is where the value is.** The agent does not stop to wait for the human: the turn
  continues after the compaction, as in-turn auto-compaction already does
  (`CompactionResolution::ContinueTurn`). The plan unfolds without intervention.
- Compaction is **deferred to the end of the turn**, not immediate (§4.1).
- The summary and the note arrive in the same turn, so the agent needs no new user message to
  resume.
- The note is a file on disk: the human can read it, and it survives even if the compaction
  fails.

## 4. Design decisions

### 4.1 Compaction is deferred to the end of the turn — and that is the only option

`TurnEvent::ManualCompaction` is accepted only when the turn is neither `Active` nor
`Compacting` (`harness/core/src/core/turn.rs:129`), otherwise `invalid_state`. A tool that tried
to compact mid-turn would fail every time.

So the "next turn" path is not a cautious preference, it is the only one compatible with the
state machine. And it happens to be the desired behaviour: compaction after the turn ends,
followed by an automatic resume, is exactly what a plan-following agent expects.

### 4.2 A direct tool, named `request_compaction`

The name says what happens: we **request**, we do not trigger synchronously. A model calling
`compact` and receiving "done" while nothing has been compacted yet would be misled.

Surface: a **direct tool** (like `wait`, `read_file`), not the TypeScript `self.*` namespace.
Reason: the tool is not for writing programs, and the programmatic namespace additionally
imposes the program checkpoint schema. Cost details in §2 of the plan.

### 4.3 The checkpoint is mandatory

`handoff_path` is a **required** parameter. Without a note, a compaction is an accepted but
blind loss of information: that is precisely the accident the agent is best placed to avoid,
since it is the one that knows what it is about to miss.

The harness refuses the request if the file is missing or empty, with a message telling the
agent to write the note first.

### 4.4 The harness does not read the file

Core performs no file I/O: it emits actions, the runtime executes them. In this architecture,
the **Python runtime** reads the checkpoint when the tool executes, and returns its (bounded)
content to core in the tool's structured result. Core stores that note and injects it after the
summary. No exception to the Core/Runtime ownership principle.

Size is bounded (8 KiB by default, truncated with an explicit mention): an oversized note would
eat the window that was just freed.

### 4.5 A distinct trace

A new `CompactionTrigger::Agent` value next to `Manual` and `Automatic`, so logs, telemetry and
the UI distinguish the three origins. The summary is produced by the same path as manual
compaction.

## 5. The tool

```
request_compaction(
  handoff_path: string,          // required, path relative to the workspace
  extra_instructions?: string,   // additional instructions for the summary
  reason?: string                // one line, logged, not sent to the summariser
)
```

Description exposed to the model (to be refined):

> Compact the conversation to free context. Use it when your current plan says to compact
> before continuing, or when you are about to start a long phase that needs room. Before
> calling it, write what must survive the compaction — decisions taken, files touched,
> remaining steps — to `handoff_path`. The compaction runs when your turn ends and the
> conversation resumes right after, with your handoff note re-injected.

Structured result returned to core by the runtime:

```json
{
  "handoff_note": "<file content, bounded>",
  "handoff_bytes": 1234,
  "truncated": false
}
```

No `accepted` field: the runtime decides nothing, it reads the file and passes it on. Core
accepts or refuses, immediately, so the model sees the refusal within the same turn.

On refusal the tool returns a failure (`Failure`) and **nothing is held pending**: missing or
empty file, path outside the workspace, flag disabled, context guard, quota reached, or a
compaction already pending. The failure message must be actionable (say what to do), because
the model will read it.

## 6. Guards

| Guard | Behaviour | Default |
|---|---|---|
| Activation | Tool not exposed when disabled (the model never sees it) | disabled |
| Context floor | Refuse when the estimated context is below the floor | 20,000 tokens |
| Per-session quota | Refuse beyond N agent-triggered compactions | 8 |
| Already pending | Refuse when a compaction is already pending or running | — |
| Minimum growth | Refuse when fewer than X tokens were added since the last agent compaction | 5,000 |
| Cooldown | Never two agent compactions in the same turn | — |

Why a floor: below ~20K tokens a summary costs a model call, produces a worse summary than the
original, and loses detail for no gain. The floor stops the agent from "compacting for the sake
of compacting" early in a session.

Why minimum growth: it is the real anti-loop protection. A quota alone lets a burst of useless
compactions through at the end of a session.

Refusals are tool failures, not protocol errors: the turn continues, the model sees why and
carries on without compacting.

## 7. Configuration

A single block, disabled by default (consistent with the repo convention: a tool that changes
the context does not enable itself). `~/.vibe/config.toml`:

```toml
[tools.agent_compaction]
enabled = true
min_input_tokens = 20000
max_per_session = 8
min_growth_tokens = 5000
max_handoff_bytes = 8192
```

The flag must be visible in `vibe config` and in the config schema, with the `vibe_schema.py`
migration updated.

## 8. Observability

- Observation `ContextCompacted { trigger: agent, .. }`: the UI shows the origin.
- The handoff note is an injected context message, visible in the history: you can check
  afterwards what the agent decided to keep.
- Telemetry: the existing compaction event, with the new trigger value.
- Log: the tool's `reason`.

## 9. Acceptance criteria

1. Flags disabled: the tool does not appear in the catalogue; no agent code path can compact.
2. A plan that asks for a compaction: the agent writes its note, calls the tool, the turn ends,
   the compaction runs, the turn resumes, and the agent continues without a human message.
3. After the resume, the history holds the summary **and** the handoff note, in that order.
4. A missing or empty handoff file makes the tool fail, with no compaction, and a message
   telling the agent to write the note first.
5. Below the context floor: refusal, explicit message, no compaction.
6. Quota reached: refusal, explicit message, no compaction.
7. Two calls in the same turn: the second is refused, a single compaction.
8. Manual `/compact` and threshold-based auto-compaction are unchanged.
9. An agent compaction that fails (summary rejected, too large, cut off) leaves the turn
   resuming with the original history: no silent loss.
10. `/compact` remains usable while an agent compaction is pending.

## 10. Risks

- **Upstream drift**: the main hook (`apply_tool_batch_transition`, `turn.rs:701`) and the state
  machine are active code. That is the main conflict risk, not the size of the diff. Mitigation:
  a small, localised diff and regular rebases.
- **The model calls the tool too early or too often**: covered by the guards, and by the fact
  that the tool is only offered when enabled.
- **Summary quality**: identical to `/compact`, no regression expected. The handoff note offsets
  the loss of detail, which is the whole point of the design.
- **User surprise**: an unrequested compaction changes the history. Mitigation: the
  `trigger = agent` trace, and the flag is opt-in.
