use lookback_tauri_lib::native_terminal::wire::{
    NativeTerminalEventWire, NativeTerminalInputRequest, NativeTerminalOutputEventWire,
    NativeTerminalOutputRecordWire, NativeTerminalPrepareArgs, NativeTerminalSessionStateWire,
    NativeTerminalStartArgs, NativeTerminalStatusWire,
};
use lookback_tauri_lib::native_terminal::{NativeTerminalOutputEvent, NativeTerminalOutputRecord};
use serde_json::json;

#[test]
fn wire_dtos_match_the_frontend_command_contract_and_reject_extra_request_fields() {
    let prepare: NativeTerminalPrepareArgs = serde_json::from_value(json!({
        "request": {
            "issueId": "issue-42",
            "profileId": "opencode-main",
            "baseBranch": "release/native",
            "prompt": "Please inspect the failing test."
        }
    }))
    .unwrap();
    assert_eq!(prepare.request.issue_id, "issue-42");
    assert_eq!(
        prepare.request.prompt.as_deref(),
        Some("Please inspect the failing test.")
    );
    assert!(!format!("{prepare:?}").contains("Please inspect the failing test."));
    assert!(
        serde_json::from_value::<NativeTerminalPrepareArgs>(json!({
            "request": {
                "issueId": "issue-42",
                "profileId": "opencode-main",
                "baseBranch": "main",
                "prompt": null,
                "workingDirectory": "/tmp/renderer-path"
            }
        }))
        .is_err()
    );

    let start: NativeTerminalStartArgs = serde_json::from_value(json!({
        "request": {
            "previewId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "previewDigest": "a".repeat(64)
        }
    }))
    .unwrap();
    assert_eq!(start.request.preview_digest, "a".repeat(64));

    let input: NativeTerminalInputRequest = serde_json::from_value(json!({
        "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
        "bytes": [0, 27, 91, 65, 255]
    }))
    .unwrap();
    assert_eq!(input.bytes, [0, 27, 91, 65, 255]);
    assert!(
        serde_json::from_value::<NativeTerminalInputRequest>(json!({
            "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "bytes": [-1]
        }))
        .is_err()
    );
}

#[test]
fn wire_output_uses_decimal_sequences_and_never_serializes_host_session_context() {
    let status = NativeTerminalStatusWire {
        available: true,
        blockers: vec![],
        profiles: vec![],
    };
    let status_json = serde_json::to_value(status).unwrap();
    assert_eq!(
        status_json,
        json!({"available": true, "blockers": [], "profiles": []})
    );

    let event: NativeTerminalOutputEventWire = serde_json::from_value(json!({
        "subscriptionId": "6bda8e76-36d5-4ee8-9206-bf14df989111",
        "record": {
            "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "sequence": "9007199254740993",
            "event": {"kind": "output", "stream": "stdout", "data": [65, 10]}
        }
    }))
    .unwrap();
    let encoded = serde_json::to_value(&event).unwrap();
    assert_eq!(encoded["record"]["sequence"], "9007199254740993");
    assert_eq!(encoded["record"]["event"]["kind"], "output");
    assert_eq!(
        encoded["record"]["event"],
        json!({"kind": "output", "stream": "stdout", "data": [65, 10]})
    );
    assert!(!format!("{event:?}").contains("[65, 10]"));
    let encoded_text = encoded.to_string();
    for forbidden in ["context", "credential", "canonicalHostRoot", "token"] {
        assert!(
            !encoded_text
                .to_lowercase()
                .contains(&forbidden.to_lowercase())
        );
    }

    assert_eq!(
        serde_json::to_string(&NativeTerminalSessionStateWire::Unknown).unwrap(),
        "\"unknown\""
    );
}

#[test]
fn serialized_output_variants_use_the_frontend_camel_case_wire_contract() {
    let session_id = "2dd2a912-56d3-4ce4-a1eb-57d6547d6904";
    let cases = [
        (
            NativeTerminalOutputEvent::Output {
                stream: "stdout".to_owned(),
                data: vec![65, 10],
            },
            json!({"kind": "output", "stream": "stdout", "data": [65, 10]}),
        ),
        (
            NativeTerminalOutputEvent::Exit {
                exit_code: 143,
                execution_time_ms: 9_007_199_254_740_993,
                sandbox_id: "sandbox-private-id".to_owned(),
            },
            json!({
                "kind": "exit",
                "exitCode": 143,
                "executionTimeMs": "9007199254740993",
                "sandboxId": "sandbox-private-id"
            }),
        ),
        (NativeTerminalOutputEvent::End, json!({"kind": "end"})),
    ];

    for (event, expected) in cases {
        let record = NativeTerminalOutputRecordWire::from_record(
            session_id,
            &NativeTerminalOutputRecord {
                sequence: 9_007_199_254_740_993,
                event,
            },
        )
        .unwrap();
        let encoded = serde_json::to_value(record).unwrap();
        assert_eq!(encoded["sessionId"], session_id);
        assert_eq!(encoded["sequence"], "9007199254740993");
        assert_eq!(encoded["event"], expected);
        assert!(!encoded.to_string().contains("context"));
        assert!(!encoded.to_string().contains("nativeJobId"));
    }

    assert_eq!(
        serde_json::to_value(NativeTerminalEventWire::Exit {
            exit_code: 143,
            execution_time_ms: "9007199254740993".to_owned(),
            sandbox_id: "sandbox-private-id".to_owned(),
        })
        .unwrap(),
        json!({
            "kind": "exit",
            "exitCode": 143,
            "executionTimeMs": "9007199254740993",
            "sandboxId": "sandbox-private-id"
        })
    );
    let zero_exit = serde_json::to_value(NativeTerminalEventWire::Exit {
        exit_code: 0,
        execution_time_ms: "1".to_owned(),
        sandbox_id: "sandbox-private-id".to_owned(),
    })
    .unwrap();
    assert_eq!(zero_exit["kind"], "exit");
    assert_ne!(zero_exit["kind"], "done");
}
