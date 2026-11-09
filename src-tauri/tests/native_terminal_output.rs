use std::{
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
};

use lookback_tauri_lib::native_terminal::output::{
    NativeTerminalOutputError, NativeTerminalOutputEvent, NativeTerminalOutputStore,
};

fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

fn database(root: &Path) -> PathBuf {
    root.join("native-terminal-output.sqlite3")
}

fn session_id() -> &'static str {
    "00000000-0000-4000-8000-000000000001"
}

#[test]
fn terminal_display_does_not_create_or_reopen_an_app_transcript() {
    let temp = tempfile::tempdir().unwrap();
    let path = database(temp.path());
    let store = NativeTerminalOutputStore::open(&path).unwrap();
    let event = store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: b"live terminal bytes".to_vec(),
            },
        )
        .unwrap();
    assert_eq!(store.page_after(session_id(), 0, 1).unwrap(), vec![event]);
    assert!(!path.exists(), "terminal bytes must stay in memory");
    drop(store);
    assert!(
        NativeTerminalOutputStore::open(&path)
            .unwrap()
            .page_after(session_id(), 0, 1)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn live_output_preserves_binary_event_order_and_bounded_pages() {
    let temp = tempfile::tempdir().unwrap();
    private_dir(temp.path());

    let store = NativeTerminalOutputStore::open(&database(temp.path())).unwrap();
    let bytes = vec![0, 0xff, b'\n', 0, b'<', b's', b'c', b'>'];
    let first = store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: bytes.clone(),
            },
        )
        .unwrap();
    let second = store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Exit {
                exit_code: 7,
                execution_time_ms: 91,
                sandbox_id: "sandbox-native".into(),
            },
        )
        .unwrap();
    let third = store
        .append(session_id(), NativeTerminalOutputEvent::End)
        .unwrap();

    assert_eq!((first.sequence, second.sequence, third.sequence), (1, 2, 3));
    assert_eq!(
        store.page_after(session_id(), 0, 2).unwrap(),
        vec![first.clone(), second.clone()]
    );
    assert_eq!(store.page_after(session_id(), 2, 2).unwrap(), vec![third]);

    drop(store);
    let reopened = NativeTerminalOutputStore::open(&database(temp.path())).unwrap();
    assert!(reopened.page_after(session_id(), 0, 3).unwrap().is_empty());
}

#[test]
fn output_stream_requires_exit_before_end_and_rejects_events_after_end() {
    let temp = tempfile::tempdir().unwrap();
    private_dir(temp.path());
    let store = NativeTerminalOutputStore::open(&database(temp.path())).unwrap();

    assert!(matches!(
        store.append(session_id(), NativeTerminalOutputEvent::End),
        Err(NativeTerminalOutputError::InvalidEvent)
    ));
    store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Exit {
                exit_code: 0,
                execution_time_ms: 5,
                sandbox_id: "sandbox".into(),
            },
        )
        .unwrap();
    store
        .append(session_id(), NativeTerminalOutputEvent::End)
        .unwrap();
    assert!(matches!(
        store.append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: b"late".to_vec(),
            }
        ),
        Err(NativeTerminalOutputError::Conflict)
    ));
    assert_eq!(store.page_after(session_id(), 0, 3).unwrap().len(), 2);
}

#[test]
fn live_output_rejects_unbounded_frames_and_pages() {
    let temp = tempfile::tempdir().unwrap();
    private_dir(temp.path());
    let store = NativeTerminalOutputStore::open(&database(temp.path())).unwrap();

    let largest_frame = store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: vec![0xa5; 64 * 1024],
            },
        )
        .unwrap();
    assert_eq!(largest_frame.sequence, 1);
    assert_eq!(
        store.page_after(session_id(), 0, 1).unwrap()[0]
            .event
            .clone(),
        NativeTerminalOutputEvent::Output {
            stream: "stdout".into(),
            data: vec![0xa5; 64 * 1024],
        }
    );

    assert!(matches!(
        store.append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: vec![0; 64 * 1024 + 1],
            }
        ),
        Err(NativeTerminalOutputError::EventTooLarge)
    ));
    assert!(matches!(
        store.page_after(session_id(), 0, 0),
        Err(NativeTerminalOutputError::InvalidPage)
    ));
}

#[test]
fn concurrent_live_display_access_preserves_event_order() {
    let temp = tempfile::tempdir().unwrap();
    private_dir(temp.path());
    let path = database(temp.path());
    let reader = Arc::new(NativeTerminalOutputStore::open(&path).unwrap());
    let writer = Arc::clone(&reader);
    reader
        .append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: b"first".to_vec(),
            },
        )
        .unwrap();

    let start = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        let reader_start = Arc::clone(&start);
        let reader_store = Arc::clone(&reader);
        scope.spawn(move || {
            reader_start.wait();
            for _ in 0..2_000 {
                let page = reader_store.page_after(session_id(), 0, 16).unwrap();
                assert!(!page.is_empty());
                assert!(page.len() <= 16);
                for (offset, record) in page.iter().enumerate() {
                    assert_eq!(record.sequence, offset as u64 + 1);
                }
            }
        });

        let writer_start = Arc::clone(&start);
        let writer_store = Arc::clone(&writer);
        scope.spawn(move || {
            writer_start.wait();
            for index in 0_u16..200 {
                writer_store
                    .append(
                        session_id(),
                        NativeTerminalOutputEvent::Output {
                            stream: "stdout".into(),
                            data: index.to_le_bytes().to_vec(),
                        },
                    )
                    .unwrap();
            }
        });
        start.wait();
    });

    let all_records = reader.page_after(session_id(), 0, 16).unwrap();
    assert_eq!(all_records.len(), 16);
    assert_eq!(all_records[0].sequence, 1);
    assert_eq!(all_records[15].sequence, 16);
}

#[test]
fn exhausted_live_history_reports_a_gap_without_stopping_output_delivery() {
    let temp = tempfile::tempdir().unwrap();
    let store = NativeTerminalOutputStore::open(&database(temp.path())).unwrap();
    let mut last = None;
    for _ in 0..24 {
        last = Some(
            store
                .append(
                    session_id(),
                    NativeTerminalOutputEvent::Output {
                        stream: "stdout".into(),
                        data: vec![1; 64 * 1024],
                    },
                )
                .unwrap(),
        );
    }
    assert!(matches!(
        store.page_after(session_id(), 0, 16),
        Err(NativeTerminalOutputError::Unavailable)
    ));
    let last = last.unwrap();
    assert_eq!(
        store
            .page_after(session_id(), last.sequence - 1, 1)
            .unwrap(),
        vec![last]
    );
    assert!(!database(temp.path()).exists());
}

#[test]
fn live_session_sequence_and_terminal_state_survive_other_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let store = NativeTerminalOutputStore::open(&database(temp.path())).unwrap();
    store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: b"first".to_vec(),
            },
        )
        .unwrap();
    for id in 2..36 {
        store
            .append(
                &format!("00000000-0000-4000-8000-{id:012}"),
                NativeTerminalOutputEvent::Output {
                    stream: "stdout".into(),
                    data: b"other".to_vec(),
                },
            )
            .unwrap();
    }
    let second = store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Output {
                stream: "stdout".into(),
                data: b"second".to_vec(),
            },
        )
        .unwrap();
    assert_eq!(second.sequence, 2);
    store
        .append(
            session_id(),
            NativeTerminalOutputEvent::Exit {
                exit_code: 0,
                execution_time_ms: 10,
                sandbox_id: "vm".into(),
            },
        )
        .unwrap();
    for id in 36..72 {
        store
            .append(
                &format!("00000000-0000-4000-8000-{id:012}"),
                NativeTerminalOutputEvent::Output {
                    stream: "stdout".into(),
                    data: b"other".to_vec(),
                },
            )
            .unwrap();
    }
    let end = store
        .append(session_id(), NativeTerminalOutputEvent::End)
        .unwrap();
    assert_eq!(end.sequence, 4);
}
