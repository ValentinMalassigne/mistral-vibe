//! The model compacting its own Turn with `request_compaction`.

use super::*;

/// Compaction is switched on with a threshold high enough that only the agent
/// moves it: what these scenarios see comes from the tool, not from the window.
fn agent_compaction_config() -> HarnessConfig {
    let mut value = serde_json::to_value(automatic_compaction_config_with_threshold(200_000))
        .expect("config serializes");
    value["settings"]["tools"]["agent_compaction"] = json!({"mode": "enabled"});
    serde_json::from_value(value).expect("agent compaction config is valid")
}

fn note() -> String {
    "Finish step 7: wire the guards.\n".to_string()
}

/// The shared helper names the automatic trigger; an agent compaction is the
/// same observation under another one.
fn agent_context_compacted(turn_id: &str, action: &Value, summary: &str) -> Value {
    let mut observation = context_compacted(turn_id, action, summary);
    observation["trigger"] = json!("agent");
    observation
}

/// The failure a Runtime returns when it cannot read the note.
fn tool_failure(action: &Value, message: &str) -> Value {
    json!({
        "type": "tool_failed",
        "action_id": action["action_id"],
        "call_id": action["call_id"],
        "result": {
            "type": "failure",
            "content": [],
            "error": {
                "code": "tool_failed",
                "message": message,
                "retryable": false,
                "details": null,
            },
        },
    })
}

///
/// *Prepare*: A long Turn where the model writes a note and asks to compact.
/// *Do*: Return the note, end the Turn, then deliver the summary.
/// *Assert*: The compaction runs at the end of the Turn with the agent trigger,
/// and the resumed request carries the summary then the note.
///
#[test]
fn a_requested_compaction_runs_when_the_turn_ends_and_resumes_with_the_note() {
    // Prepare
    let turn_id = "agent-compaction";
    let mut runtime = SynchronousRuntime::new(agent_compaction_config());
    let completion = runtime.start_turn(turn_id, &large_text("work"));
    let calls = vec![tool_call(
        "handoff",
        "request_compaction",
        json!({"handoff_path": "handoff.md"}),
    )];
    let request = runtime
        .apply(
            tool_call_completion(action_id(&completion), calls.clone()),
            running(turn_id)
                .observe(assistant_tool_calls_committed(turn_id, &completion, &calls))
                .dispatch(runtime_tool(
                    "request_compaction",
                    json!({"handoff_path": "handoff.md"}),
                ))
                .observe(tool_execution_started(turn_id, "handoff")),
        )
        .only_action();

    // Do
    let result = structured_tool_success(
        &request,
        json!({
            "handoff_note": note(),
            "handoff_bytes": note().len(),
            "truncated": false,
        }),
    );
    let compaction = runtime
        .apply(
            result.clone(),
            running(turn_id)
                .dispatch(compaction_call("agent", 1, 1))
                .observe_completed_tool_result(&request, &result),
        )
        .only_action();
    let resumed = runtime
        .apply(
            summary_completion(&compaction, "the work so far"),
            running(turn_id)
                .dispatch(llm_call(1))
                .observe(agent_context_compacted(
                    turn_id,
                    &compaction,
                    "the work so far",
                )),
        )
        .only_action();

    // Assert
    assert_eq!(compaction["purpose"], "compaction");
    assert_eq!(compaction["trigger"], "agent");
    assert!(compaction.get("turn_id").is_some());
    assert_eq!(resumed["purpose"], "agent");
    let messages = action_messages(&resumed);
    let note_index = messages
        .iter()
        .position(|message| message_text(message).is_some_and(|text| text.contains(note().trim())))
        .expect("the resumed request carries the handoff note");
    let summary_index = messages
        .iter()
        .position(|message| {
            message_text(message).is_some_and(|text| text.contains("the work so far"))
        })
        .expect("the resumed request carries the summary");
    assert!(
        summary_index < note_index,
        "the note follows the summary, never precedes it"
    );
}

///
/// *Prepare*: A model asks to compact, and the Runtime cannot read the note.
/// *Do*: Return the Runtime's failure.
/// *Assert*: Nothing is compacted and the Turn asks the model for its next step.
///
#[test]
fn a_note_the_runtime_cannot_read_compacts_nothing() {
    // Prepare
    let turn_id = "agent-compaction-refused";
    let mut runtime = SynchronousRuntime::new(agent_compaction_config());
    let completion = runtime.start_turn(turn_id, &large_text("work"));
    let calls = vec![tool_call(
        "handoff",
        "request_compaction",
        json!({"handoff_path": "missing.md"}),
    )];
    let request = runtime
        .apply(
            tool_call_completion(action_id(&completion), calls.clone()),
            running(turn_id)
                .observe(assistant_tool_calls_committed(turn_id, &completion, &calls))
                .dispatch(runtime_tool(
                    "request_compaction",
                    json!({"handoff_path": "missing.md"}),
                ))
                .observe(tool_execution_started(turn_id, "handoff")),
        )
        .only_action();

    // Do
    let denied = tool_failure(&request, "Nothing was compacted: no such file.");
    let resumed = runtime
        .apply(
            denied.clone(),
            running(turn_id)
                .dispatch(llm_call(1))
                .observe_completed_tool_result(&request, &denied),
        )
        .only_action();

    // Assert
    assert_eq!(resumed["purpose"], "agent");
}

///
/// *Prepare*: One batch holds two requests, and the Runtime reads both notes.
/// *Do*: Return both results in batch order.
/// *Assert*: The first is recorded, the second is refused as already scheduled,
/// and a single compaction runs when the Turn ends.
///
#[test]
fn a_second_request_in_the_same_batch_is_refused() {
    // Prepare
    let turn_id = "agent-compaction-twice";
    let mut runtime = SynchronousRuntime::new(agent_compaction_config());
    let completion = runtime.start_turn(turn_id, &large_text("work"));
    let calls = vec![
        tool_call(
            "first",
            "request_compaction",
            json!({"handoff_path": "first.md"}),
        ),
        tool_call(
            "second",
            "request_compaction",
            json!({"handoff_path": "second.md"}),
        ),
    ];
    let actions = runtime
        .apply(
            tool_call_completion(action_id(&completion), calls.clone()),
            running(turn_id)
                .observe(assistant_tool_calls_committed(turn_id, &completion, &calls))
                .dispatch(runtime_tool(
                    "request_compaction",
                    json!({"handoff_path": "first.md"}),
                ))
                .dispatch(runtime_tool(
                    "request_compaction",
                    json!({"handoff_path": "second.md"}),
                ))
                .observe(tool_execution_started(turn_id, "first"))
                .observe(tool_execution_started(turn_id, "second")),
        )
        .actions();
    let (first, second) = (actions[0].clone(), actions[1].clone());

    // Do
    let first_result = structured_tool_success(
        &first,
        json!({
            "handoff_note": note(),
            "handoff_bytes": note().len(),
            "truncated": false,
        }),
    );
    runtime.apply(
        first_result.clone(),
        running(turn_id)
            .keep(&second)
            .observe_completed_tool_result(&first, &first_result),
    );
    let second_result = structured_tool_success(
        &second,
        json!({
            "handoff_note": note(),
            "handoff_bytes": note().len(),
            "truncated": false,
        }),
    );
    let refused = json!({
        "type": "failure",
        "content": [],
        "error": {
            "code": "tool_failed",
            "message": "Nothing was compacted: this Turn already has a compaction \
                        scheduled. Do not call this tool again; end your Turn.",
            "retryable": false,
            "details": null,
        },
    });
    let compaction = runtime
        .apply(
            second_result,
            running(turn_id)
                .dispatch(compaction_call("agent", 1, 1))
                .observe(tool_result_committed(turn_id, &second, refused.clone()))
                .observe_tool_execution_finished("second", refused),
        )
        .only_action();

    // Assert
    assert_eq!(compaction["trigger"], "agent");
}
