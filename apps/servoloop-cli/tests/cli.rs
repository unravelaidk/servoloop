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
    let out = bin()
        .args(["run", "--demo", "--store"])
        .arg(&root)
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
    let journal = fs::read_dir(&root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("journal.ndjson");
    let text = fs::read_to_string(journal).unwrap();
    assert!(text.contains("\"kind\":\"intent\""));
    assert!(text.contains("\"kind\":\"result\""));
    let _ = fs::remove_dir_all(root);
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
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = requests.clone();
    let server = thread::spawn(move || {
        for index in 0..2 {
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
            let response = if index == 0 {
                let arguments =
                    serde_json::json!({"command":"move_joint","joint":"shoulder","position":0.2})
                        .to_string();
                serde_json::json!({"choices":[{"message":{"content":"moving","tool_calls":[{"id":"move-1","type":"function","function":{"name":"robot_command","arguments":arguments}}]},"finish_reason":"tool_calls"}]})
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
        .env("OPENAI_API_KEY", "test-key")
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
    assert_eq!(captured.len(), 2);
    assert!(captured.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key")
    }));
    assert!(captured[0].contains("move the shoulder"));
    assert!(captured[1].contains("simulator accepted command"));
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
    let journal_text = fs::read_to_string(journal).unwrap();
    assert!(journal_text.contains("\"kind\":\"intent\""));
    assert!(journal_text.contains("\"kind\":\"result\""));
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
