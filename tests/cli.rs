use serde_json::Value;
use std::{
    io::Write,
    process::{Command, Stdio},
};
const ID: &str = "b55c9c91-384d-452b-81db-d1ef79372b75";
fn command(dir: &tempfile::TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zottion"));
    command
        .env("ZOTTION_CONFIG_DIR", dir.path().join("config"))
        .env("ZOTTION_STATE_DIR", dir.path().join("state"))
        .env("NOTION_TOKEN", "fixture-token")
        .stdin(Stdio::null());
    command
}
#[test]
fn cli_stdin_json_drafts_profiles_and_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = command(&dir)
        .args([
            "--offline",
            "--json",
            "page",
            "create",
            "--parent",
            ID,
            "--title",
            "Черновик",
            "--file",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all("Заметка из stdin".as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["schema_version"], 1);
    assert_eq!(data["data"]["published"], false);
    let drafts = command(&dir)
        .args(["--offline", "--json", "draft", "list"])
        .output()
        .unwrap();
    let drafts: Value = serde_json::from_slice(&drafts.stdout).unwrap();
    assert_eq!(drafts["data"].as_array().unwrap().len(), 1);
    let other = command(&dir)
        .args(["--profile", "other", "--offline", "--json", "draft", "list"])
        .output()
        .unwrap();
    let other: Value = serde_json::from_slice(&other.stdout).unwrap();
    assert!(other["data"].as_array().unwrap().is_empty());
    let result = command(&dir)
        .args(["--json", "page", "trash", ID])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stderr).unwrap()["error"]["code"],
        2
    );
    let result = command(&dir)
        .args(["--json", "block", "move", ID, "--parent", ID])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(7));
    let result = command(&dir).arg("--json").output().unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    let result = command(&dir)
        .args(["--json", "page", "get", "bad"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(serde_json::from_slice::<Value>(&result.stderr).is_ok());
    let mut child = command(&dir)
        .env_remove("NOTION_TOKEN")
        .args(["auth", "login", "--token-stdin", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let invalid_token = "sensitive-invalid-token second-token";
    child
        .stdin
        .take()
        .unwrap()
        .write_all(invalid_token.as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(result.stdout.is_empty());
    let error = String::from_utf8(result.stderr).unwrap();
    assert!(!error.contains(invalid_token));
    assert_eq!(
        serde_json::from_str::<Value>(&error).unwrap()["error"]["code"],
        2
    );
}
