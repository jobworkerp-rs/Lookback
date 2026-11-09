use lookback_tauri_lib::native_terminal::doctor::{
    NativeCliProbeValidationError, validate_native_cli_probe_result,
};

#[test]
fn doctor_accepts_an_arbitrary_observed_cli_version_after_a_clean_terminal_stream() {
    let result = validate_native_cli_probe_result(
        b"0.200.0-nightly.4+linux\r\n",
        b"",
        Some(0),
        true,
        true,
        false,
    )
    .expect("a safe observed CLI version is valid evidence");

    assert_eq!(result.version(), "0.200.0-nightly.4+linux");
    assert_eq!(result.exit_code(), 0);
    assert!(result.end_received());
    assert!(result.stream_eof());
}

#[test]
fn doctor_does_not_turn_zero_exit_into_version_evidence() {
    for (stdout, stderr, exit, end, eof, stream_error) in [
        (b"".as_slice(), b"".as_slice(), Some(0), true, true, false),
        (
            b"1.18.3\n".as_slice(),
            b"".as_slice(),
            Some(0),
            false,
            true,
            false,
        ),
        (
            b"1.18.3\n".as_slice(),
            b"".as_slice(),
            Some(0),
            true,
            false,
            false,
        ),
        (
            b"1.18.3\n".as_slice(),
            b"".as_slice(),
            Some(7),
            true,
            true,
            false,
        ),
        (
            b"1.18.3\nlog output\n".as_slice(),
            b"".as_slice(),
            Some(0),
            true,
            true,
            false,
        ),
        (
            b"1.18.3\n".as_slice(),
            b"".as_slice(),
            Some(0),
            true,
            true,
            true,
        ),
    ] {
        assert!(
            validate_native_cli_probe_result(stdout, stderr, exit, end, eof, stream_error).is_err(),
            "incomplete or malformed probe output must not mint a lease"
        );
    }
}

#[test]
fn doctor_rejects_unbounded_or_non_utf8_probe_output_without_echoing_it() {
    let invalid_utf8 = [b'1', b'.', b'1', b'8', b'.', b'3', 0xff];
    for output in [
        b"1.18.3 extra\n".as_slice(),
        b"1.18.3\n\n",
        &invalid_utf8,
        &vec![b'1'; 64 * 1024 + 1],
    ] {
        let error = validate_native_cli_probe_result(output, b"", Some(0), true, true, false)
            .expect_err("unsupported output must fail closed");
        assert!(matches!(
            error,
            NativeCliProbeValidationError::OutputTooLarge
                | NativeCliProbeValidationError::InvalidVersionOutput
        ));
        assert!(
            !error.to_string().contains("1.18.4"),
            "errors must not include untrusted subprocess bytes"
        );
    }
}

#[test]
fn doctor_checks_the_selected_agent_version_and_real_stream_completion() {
    use lookback_tauri_lib::native_terminal::{
        doctor::validate_native_cli_probe_for_profile,
        launch::{NativeAgentKind, NativeCliProfile},
    };
    for (kind, hint, version, output, wrong) in [
        (
            NativeAgentKind::OpenCode,
            "1.18.3",
            "2.4.0-beta.2+build.9",
            "2.4.0-beta.2+build.9\n",
            "opencode 2.4.0\n",
        ),
        (
            NativeAgentKind::Codex,
            "0.160.1",
            "0.200.0-nightly.4",
            "codex-cli 0.200.0-nightly.4\n",
            "codex 0.200.0-nightly.4\n",
        ),
        (
            NativeAgentKind::ClaudeCode,
            "2.1.292",
            "3.0.0-rc.1+linux",
            "3.0.0-rc.1+linux (Claude Code)\n",
            "codex 3.0.0-rc.1+linux\n",
        ),
    ] {
        let profile = NativeCliProfile {
            kind,
            cli_version: hint.into(),
            runtime_image_digest: format!("sha256:{}", "a".repeat(64)),
            expected_profile_command: vec![],
        };
        let result = validate_native_cli_probe_for_profile(
            &profile,
            output.as_bytes(),
            b"",
            Some(0),
            true,
            true,
            false,
        )
        .unwrap();
        assert_eq!(result.version(), version);
        assert!(
            validate_native_cli_probe_for_profile(
                &profile,
                wrong.as_bytes(),
                b"",
                Some(0),
                true,
                true,
                false
            )
            .is_err()
        );
        assert!(
            validate_native_cli_probe_for_profile(
                &profile,
                output.as_bytes(),
                b"",
                Some(0),
                false,
                true,
                false
            )
            .is_err()
        );
    }
}

#[test]
fn doctor_accepts_safe_hints_at_the_byte_boundary_and_rejects_unsafe_forms() {
    use lookback_tauri_lib::native_terminal::{
        doctor::validate_native_cli_probe_for_profile,
        launch::{NativeAgentKind, NativeCliProfile},
    };

    let profile = |hint: String| NativeCliProfile {
        kind: NativeAgentKind::OpenCode,
        cli_version: hint,
        runtime_image_digest: format!("sha256:{}", "a".repeat(64)),
        expected_profile_command: vec!["opencode".into(), "run".into()],
    };
    for hint in [
        "".to_owned(),
        "1.18.3".to_owned(),
        "0.1.0-rc.1+build.7".to_owned(),
        "v".repeat(1024),
        "é".repeat(512),
    ] {
        let result = validate_native_cli_probe_for_profile(
            &profile(hint),
            b"2.4.0-beta.2+build.9\n",
            b"",
            Some(0),
            true,
            true,
            false,
        )
        .expect("safe version hints, including the byte limit, remain accepted");
        assert_eq!(result.version(), "2.4.0-beta.2+build.9");
    }

    for hint in [
        "v".repeat(1025),
        "é".repeat(513),
        "1.18.3 ".to_owned(),
        "1.18.3\n".to_owned(),
        "1.18.3\u{7f}".to_owned(),
    ] {
        assert!(matches!(
            validate_native_cli_probe_for_profile(
                &profile(hint),
                b"2.4.0-beta.2+build.9\n",
                b"",
                Some(0),
                true,
                true,
                false,
            ),
            Err(NativeCliProbeValidationError::InvalidVersionOutput)
        ));
    }
}

#[test]
fn warning_stderr_does_not_replace_version_or_stream_completion_evidence() {
    let result = validate_native_cli_probe_result(
        b"2.4.0-beta.2+build.9\n",
        b"PATH warning\n",
        Some(0),
        true,
        true,
        false,
    )
    .expect("bounded warnings do not invalidate a successful version probe");
    assert_eq!(result.version(), "2.4.0-beta.2+build.9");
    for (stdout, exit, end, eof, error) in [
        (
            b"2.4.0-beta.2+build.9 \n".as_slice(),
            Some(0),
            true,
            true,
            false,
        ),
        (
            b"2.4.0-beta.2+build.9\n".as_slice(),
            Some(7),
            true,
            true,
            false,
        ),
        (
            b"2.4.0-beta.2+build.9\n".as_slice(),
            Some(0),
            false,
            true,
            false,
        ),
        (
            b"2.4.0-beta.2+build.9\n".as_slice(),
            Some(0),
            true,
            false,
            false,
        ),
        (
            b"2.4.0-beta.2+build.9\n".as_slice(),
            Some(0),
            true,
            true,
            true,
        ),
    ] {
        assert!(
            validate_native_cli_probe_result(stdout, b"warning\n", exit, end, eof, error).is_err()
        );
    }
    assert_eq!(
        validate_native_cli_probe_result(
            b"1.18.3\n",
            &vec![b'w'; 64 * 1024 + 1],
            Some(0),
            true,
            true,
            false
        )
        .unwrap_err(),
        NativeCliProbeValidationError::OutputTooLarge
    );
}
