use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_servoloop"))
}

fn read_http_request(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let length = headers
        .lines()
        .find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
        })
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
}

fn respond(stream: &mut std::net::TcpStream, body: serde_json::Value) {
    let payload = serde_json::to_vec(&body).unwrap();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    )
    .unwrap();
    stream.write_all(&payload).unwrap();
}

#[test]
fn help_and_version_are_available() {
    assert!(bin().arg("--help").output().unwrap().status.success());
    assert!(bin().arg("--version").output().unwrap().status.success());
    assert!(bin()
        .args(["run", "--help"])
        .output()
        .unwrap()
        .status
        .success());
    assert!(bin()
        .args(["config", "--help"])
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn config_init_is_atomic_and_never_clobbers() {
    let root = std::env::temp_dir().join(format!("servoloop-init-{}", std::process::id()));
    let _ = fs::remove_file(&root);
    let out = bin()
        .args(["config", "init", "--config"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let original = fs::read_to_string(&root).unwrap();
    let out = bin()
        .args(["config", "init", "--config"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(fs::read_to_string(&root).unwrap(), original);
    let _ = fs::remove_file(root);
}

#[cfg(unix)]
#[test]
fn config_init_does_not_follow_destination_symlink() {
    use std::os::unix::fs::symlink;
    let root = std::env::temp_dir().join(format!("servoloop-link-{}", std::process::id()));
    let target = std::env::temp_dir().join(format!("servoloop-target-{}", std::process::id()));
    let _ = fs::remove_file(&root);
    let _ = fs::remove_file(&target);
    fs::write(&target, b"keep").unwrap();
    symlink(&target, &root).unwrap();
    let out = bin()
        .args(["config", "init", "--config"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(fs::read(&target).unwrap(), b"keep");
    let _ = fs::remove_file(root);
    let _ = fs::remove_file(target);
}

#[test]
fn stdin_prompt_is_bounded_and_empty_input_is_rejected() {
    let mut child = bin()
        .args([
            "run",
            "--prompt",
            "-",
            "--provider",
            "ollama",
            "--model",
            "local",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&vec![b'x'; 1024 * 1024 + 1])
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exceeds"));
}

#[test]
fn demo_runs_real_loop_and_writes_journal() {
    let root = std::env::temp_dir().join(format!("servoloop-cli-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let out = bin()
        .args(["run", "--demo", "--store"])
        .arg(&root)
        .args(["--session", "integration-session"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    let second = bin()
        .args([
            "resume",
            "integration-session",
            "--demo",
            "--prompt",
            "continue",
            "--store",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let session_dir = root.join("integration-session");
    let text = fs::read_to_string(session_dir.join("journal.ndjson")).unwrap();
    let records: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let intents: Vec<&str> = records
        .iter()
        .filter(|r| r["kind"] == "intent")
        .map(|r| r["intent_id"].as_str().unwrap())
        .collect();
    assert_eq!(intents.len(), 2);
    assert_ne!(intents[0], intents[1]);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(session_dir.join("snapshot.json")).unwrap()).unwrap();
    assert!(!snapshot["session"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn demo_respects_turn_limit_through_shared_runner() {
    let root = std::env::temp_dir().join(format!("servoloop-demo-limit-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let output = bin()
        .args(["run", "--demo", "--max-turns", "1", "--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(r#""outcome":"failed""#));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resume_sends_saved_history_and_only_dispatches_the_new_call() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    let server = thread::spawn(move || {
        for call_id in ["old-call", "new-call"] {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            seen.lock().unwrap().push(request);
            let args = serde_json::json!({
                "command": "move_joint",
                "joint": "shoulder",
                "position": 0.2
            })
            .to_string();
            respond(
                &mut stream,
                serde_json::json!({"choices":[{"message":{"content":"moving","tool_calls":[{"id":call_id,"type":"function","function":{"name":"robot_command","arguments":args}}]},"finish_reason":"tool_calls"}]}),
            );
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            seen.lock().unwrap().push(request);
            respond(
                &mut stream,
                serde_json::json!({"choices":[{"message":{"content":"verified"},"finish_reason":"stop"}]}),
            );
        }
    });
    let root = std::env::temp_dir().join(format!("servoloop-resume-{}", std::process::id()));
    let base = format!("http://{address}/v1");
    let first = bin()
        .env("OPENAI_API_KEY", "resume-key")
        .args([
            "run",
            "--provider",
            "openai",
            "--model",
            "mock",
            "--prompt",
            "old prompt",
            "--base-url",
        ])
        .arg(&base)
        .args(["--session", "resume-session", "--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = bin()
        .env("OPENAI_API_KEY", "resume-key")
        .args([
            "resume",
            "resume-session",
            "--provider",
            "openai",
            "--model",
            "mock",
            "--prompt",
            "new prompt",
            "--base-url",
        ])
        .arg(&base)
        .args(["--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    server.join().unwrap();

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let resumed = &requests[2]["messages"];
    let resumed_json = resumed.to_string();
    assert!(resumed_json.contains("old prompt"));
    assert!(resumed_json.contains("old-call"));
    assert!(resumed_json.contains("simulator accepted command"));
    assert!(resumed_json.contains("new prompt"));
    let final_messages = requests[3]["messages"].as_array().unwrap();
    let ids: Vec<_> = final_messages
        .iter()
        .flat_map(|message| message["tool_calls"].as_array().into_iter().flatten())
        .filter_map(|call| call["id"].as_str())
        .collect();
    assert_eq!(ids, vec!["old-call", "new-call"]);
    assert_eq!(ids.iter().filter(|id| **id == "new-call").count(), 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_resume_session_refuses_before_networking() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let root = std::env::temp_dir().join(format!("servoloop-missing-{}", std::process::id()));
    let out = bin()
        .env("OPENAI_API_KEY", "test-key")
        .args([
            "resume",
            "does-not-exist",
            "--provider",
            "openai",
            "--model",
            "mock",
            "--prompt",
            "hello",
            "--base-url",
        ])
        .arg(base)
        .args(["--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("session not found"));
    drop(listener);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn run_existing_session_refuses_without_mutating_snapshot_or_journal() {
    let root = std::env::temp_dir().join(format!("servoloop-existing-{}", std::process::id()));
    let first = bin()
        .args(["run", "--demo", "--session", "existing", "--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(first.status.success());
    let journal = fs::read(root.join("existing/journal.ndjson")).unwrap();
    let snapshot = fs::read(root.join("existing/snapshot.json")).unwrap();
    let second = bin()
        .args(["run", "--demo", "--session", "existing", "--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("use `resume`"));
    assert_eq!(
        fs::read(root.join("existing/journal.ndjson")).unwrap(),
        journal
    );
    assert_eq!(
        fs::read(root.join("existing/snapshot.json")).unwrap(),
        snapshot
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resume_json_flag_is_accepted_and_stale_snapshot_is_refused() {
    let root = std::env::temp_dir().join(format!("servoloop-watermark-{}", std::process::id()));
    let first = bin()
        .args(["run", "--demo", "--session", "watermark", "--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(first.status.success());
    let snapshot_path = root.join("watermark/snapshot.json");
    let mut snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
    snapshot["journal_sequence"] = serde_json::json!(0);
    fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
    let out = bin()
        .args([
            "resume",
            "watermark",
            "--demo",
            "--json",
            "--prompt",
            "continue",
            "--store",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("watermark"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resume_refuses_unknown_journal_outcome_before_networking() {
    let root = std::env::temp_dir().join(format!("servoloop-unknown-{}", std::process::id()));
    let first = bin()
        .args(["run", "--demo", "--session", "unknown", "--store"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(first.status.success());
    let path = root.join("unknown/journal.ndjson");
    let mut records: Vec<serde_json::Value> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let next = records.len() as u64 + 1;
    records.push(serde_json::json!({"version":1,"sequence":next,"session_id":"unknown","intent_id":"unknown-intent","kind":"intent","arguments":{},"outcome":null}));
    records.push(serde_json::json!({"version":1,"sequence":next + 1,"session_id":"unknown","intent_id":"unknown-intent","kind":"result","arguments":{},"outcome":null}));
    let text = records
        .iter()
        .map(|r| serde_json::to_string(r).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(path, text).unwrap();
    let out = bin()
        .args([
            "resume", "unknown", "--demo", "--prompt", "continue", "--store",
        ])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unresolved"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn run_accepts_machine_output_flags() {
    let root = std::env::temp_dir().join(format!("servoloop-cli-output-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let out = bin()
        .args(["run", "--demo", "--json", "--output", "json", "--store"])
        .arg(&root)
        .args(["--session", "output-session"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap();
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn invalid_output_format_is_rejected_with_usage() {
    let out = bin()
        .args(["models", "--provider", "ollama", "--output", "yaml"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid value") || stderr.contains("possible values"));
    assert!(stderr.contains("--help"));
}

#[test]
fn offline_model_does_not_network() {
    let out = bin()
        .args([
            "models",
            "--provider",
            "ollama",
            "--offline",
            "--model",
            "local",
            "--output",
            "json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "[\"local\"]");
}

#[test]
fn invalid_provider_is_rejected() {
    let out = bin()
        .args([
            "models",
            "--provider",
            "no-such",
            "--offline",
            "--model",
            "x",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown provider"));
}

#[test]
fn unknown_argument_is_rejected() {
    let out = bin()
        .args(["run", "--demo", "--not-an-option"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown option"));
}

#[test]
fn live_run_requires_prompt_before_networking() {
    let out = bin()
        .args(["run", "--provider", "ollama", "--model", "local"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--prompt is required"));
}

#[test]
fn live_run_uses_local_model_and_persists_verified_tool_action() {
    let secret = r#"test"key\slash"#.to_string();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    let response_secret = secret.clone();
    let server = thread::spawn(move || {
        for index in 0..3 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end = loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            while bytes.len() < header_end + length {
                let count = stream.read(&mut buffer).unwrap();
                bytes.extend_from_slice(&buffer[..count]);
            }
            let body =
                String::from_utf8_lossy(&bytes[header_end..header_end + length]).into_owned();
            seen.lock().unwrap().push(format!("{headers}{body}"));
            let response = if index < 2 {
                let arguments =
                    serde_json::json!({"command":"move_joint","joint":"shoulder","position":0.2 + index as f64 * 0.1})
                        .to_string();
                serde_json::json!({"choices":[{"message":{"content":format!("echo {response_secret}"),"tool_calls":[{"id":format!("move-{index}"),"type":"function","function":{"name":"robot_command","arguments":arguments}}]},"finish_reason":"tool_calls"}]})
            } else {
                serde_json::json!({"choices":[{"message":{"content":"verified"},"finish_reason":"stop"}]})
            };
            let payload = serde_json::to_vec(&response).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len()).unwrap();
            stream.write_all(&payload).unwrap();
        }
    });
    let root = std::env::temp_dir().join(format!("servoloop-live-{}", std::process::id()));
    let base = format!("http://{address}/v1");
    let out = bin()
        .env("OPENAI_API_KEY", &secret)
        .args([
            "run",
            "--provider",
            "openai",
            "--model",
            "mock",
            "--prompt",
            "move the shoulder",
            "--base-url",
        ])
        .arg(base)
        .args(["--store"])
        .arg(&root)
        .output()
        .unwrap();
    server.join().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 3);
    assert!(captured.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {secret}"))
    }));
    assert!(captured[0].contains("move the shoulder"));
    assert!(captured[1].contains("simulator accepted command"));
    assert!(captured[2].contains("simulator accepted command"));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("\"event\":\"session_finished\""))
            .count(),
        1
    );
    assert!(lines
        .iter()
        .any(|line| line.contains("\"outcome\":\"success\"")));
    let journal = fs::read_dir(&root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("journal.ndjson");
    let journal_bytes = fs::read(&journal).unwrap();
    assert!(!String::from_utf8_lossy(&journal_bytes).contains(&secret));
    let journal_text = String::from_utf8_lossy(&journal_bytes);
    let records: Vec<serde_json::Value> = journal_text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let intents: Vec<&str> = records
        .iter()
        .filter(|record| record["kind"] == "intent")
        .map(|record| record["intent_id"].as_str().unwrap())
        .collect();
    assert_eq!(intents.len(), 2);
    assert_ne!(intents[0], intents[1]);
    let snapshot: serde_json::Value = serde_json::from_slice(
        &fs::read(
            fs::read_dir(&root)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path()
                .join("snapshot.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(!snapshot["session"]["messages"]
        .as_array()
        .unwrap()
        .is_empty());
    let snapshot_bytes = fs::read(
        fs::read_dir(&root)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("snapshot.json"),
    )
    .unwrap();
    assert!(!stdout.contains(&secret));
    assert!(!String::from_utf8_lossy(&snapshot_bytes).contains(&secret));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn config_show_redacts_url_credentials() {
    let path = std::env::temp_dir().join(format!("servoloop-config-{}.json", std::process::id()));
    fs::write(
        &path,
        r#"{"version":1,"base_url":"http://operator:secret@example.test/v1"}"#,
    )
    .unwrap();
    let out = bin()
        .args(["config", "--config"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("operator"));
    assert!(!stdout.contains("secret"));
    assert!(stdout.contains("[REDACTED]"));
    let _ = fs::remove_file(path);
}

#[test]
fn invalid_store_path_is_rejected() {
    let path = std::env::temp_dir().join(format!("servoloop-file-{}", std::process::id()));
    fs::write(&path, "not a directory").unwrap();
    let out = bin()
        .args(["run", "--demo", "--store"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let _ = fs::remove_file(path);
}
