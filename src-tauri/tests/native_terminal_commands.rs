use lookback_tauri_lib::native_terminal::wire::{
    NativeTerminalAttachArgs, NativeTerminalDetachArgs, NativeTerminalInputArgs,
    NativeTerminalInputDeliveryWire, NativeTerminalOutputArgs, NativeTerminalOutputEventWire,
    NativeTerminalOutputRecordWire, NativeTerminalPrepareArgs, NativeTerminalStartArgs,
};
use serde_json::json;

#[test]
fn command_request_envelopes_match_frontend_and_reject_renderer_authority_fields() {
    let prepare: NativeTerminalPrepareArgs = serde_json::from_value(json!({
        "request": {
            "issueId": "issue-42",
            "profileId": "opencode-main",
            "baseBranch": "main",
            "prompt": "Please inspect this Issue."
        }
    }))
    .unwrap();
    assert_eq!(prepare.request.issue_id, "issue-42");
    assert_eq!(prepare.request.profile_id, "opencode-main");

    let start: NativeTerminalStartArgs = serde_json::from_value(json!({
        "request": {
            "previewId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "previewDigest": "a".repeat(64)
        }
    }))
    .unwrap();
    assert_eq!(start.request.preview_digest, "a".repeat(64));

    let attach: NativeTerminalAttachArgs = serde_json::from_value(json!({
        "request": {
            "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "afterSequence": "9007199254740993"
        }
    }))
    .unwrap();
    assert_eq!(attach.request.after_sequence, "9007199254740993");

    let output: NativeTerminalOutputArgs = serde_json::from_value(json!({
        "request": {
            "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "afterSequence": "0"
        }
    }))
    .unwrap();
    assert_eq!(output.request.after_sequence, "0");

    let input: NativeTerminalInputArgs = serde_json::from_value(json!({
        "request": {
            "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "bytes": [0, 27, 91, 65, 255]
        }
    }))
    .unwrap();
    assert_eq!(input.request.bytes, [0, 27, 91, 65, 255]);

    let detach: NativeTerminalDetachArgs = serde_json::from_value(json!({
        "request": {
            "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
            "subscriptionId": "6bda8e76-36d5-4ee8-9206-bf14df989111"
        }
    }))
    .unwrap();
    assert_eq!(
        detach.request.subscription_id,
        "6bda8e76-36d5-4ee8-9206-bf14df989111"
    );

    let invalid = [
        json!({
            "request": {
                "issueId": "issue-42",
                "profileId": "opencode-main",
                "baseBranch": "main",
                "prompt": null,
                "ownerWindowLabel": "other-window"
            }
        }),
        json!({
            "request": {
                "issueId": "issue-42",
                "profileId": "opencode-main",
                "baseBranch": "main",
                "prompt": null,
                "runtimeEnv": {"API_KEY": "renderer-controlled"}
            }
        }),
        json!({
            "request": {
                "previewId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
                "previewDigest": "a".repeat(64),
                "observedLease": true
            }
        }),
    ];
    assert!(serde_json::from_value::<NativeTerminalPrepareArgs>(invalid[0].clone()).is_err());
    assert!(serde_json::from_value::<NativeTerminalPrepareArgs>(invalid[1].clone()).is_err());
    assert!(serde_json::from_value::<NativeTerminalStartArgs>(invalid[2].clone()).is_err());
}

#[test]
fn input_bytes_are_unsigned_octets_and_output_events_keep_decimal_cursors() {
    assert!(
        serde_json::from_value::<NativeTerminalInputArgs>(json!({
            "request": {
                "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
                "bytes": [-1]
            }
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<NativeTerminalInputArgs>(json!({
            "request": {
                "sessionId": "2dd2a912-56d3-4ce4-a1eb-57d6547d6904",
                "bytes": [256]
            }
        }))
        .is_err()
    );

    for (delivery, expected) in [
        (NativeTerminalInputDeliveryWire::Accepted, "accepted"),
        (NativeTerminalInputDeliveryWire::Unknown, "unknown"),
        (NativeTerminalInputDeliveryWire::Unavailable, "unavailable"),
    ] {
        assert_eq!(
            serde_json::to_string(&delivery).unwrap(),
            format!("\"{expected}\"")
        );
    }

    let event = NativeTerminalOutputEventWire {
        subscription_id: "6bda8e76-36d5-4ee8-9206-bf14df989111".into(),
        record: NativeTerminalOutputRecordWire {
            session_id: "2dd2a912-56d3-4ce4-a1eb-57d6547d6904".into(),
            sequence: "9007199254740993".into(),
            event: lookback_tauri_lib::native_terminal::wire::NativeTerminalEventWire::Output {
                stream: "stdout".into(),
                data: vec![65, 10],
            },
        },
    };
    let json = serde_json::to_value(event).unwrap();
    assert_eq!(
        json["subscriptionId"],
        "6bda8e76-36d5-4ee8-9206-bf14df989111"
    );
    assert_eq!(json["record"]["sequence"], "9007199254740993");
    assert_eq!(
        json["record"]["event"],
        json!({
            "kind": "output",
            "stream": "stdout",
            "data": [65, 10]
        })
    );
    assert!(json.get("context").is_none());
}
