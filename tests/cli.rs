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

#[test]
fn oauth_server_configuration_is_checked_before_keychain() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["auth", "login", "--json"],
        vec![
            "auth",
            "login",
            "--server",
            "https://example.com/profile",
            "--json",
        ],
    ] {
        let result = command(&dir)
            .env_remove("NOTION_TOKEN")
            .env(
                "ZOTTION_AUTH_SERVER",
                "https://user:private-password@example.com",
            )
            .args(args)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        let error = String::from_utf8(result.stderr).unwrap();
        assert!(!error.contains("private-password"));
        assert_eq!(
            serde_json::from_str::<Value>(&error).unwrap()["error"]["code"],
            2
        );
    }
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(dir.path().join("config/config.toml"),"[profiles.work]\nselected_workspace = 'test'\n[profiles.work.workspaces.test]\nidentity = 'fixture'\nserver = 'http://remote.example.com'\nkeyring = false\n").unwrap();
    let result = command(&dir)
        .env_remove("NOTION_TOKEN")
        .env_remove("ZOTTION_AUTH_SERVER")
        .args(["--profile", "work", "auth", "login", "--json"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&result.stderr).unwrap()["error"]["code"],
        2
    );
}

#[cfg(unix)]
#[test]
fn native_login_keeps_helper_output_and_tokens_out_of_json() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("ntn");
    std::fs::write(&helper, r#"#!/bin/sh
set -eu
[ "$NOTION_KEYRING" = 1 ] && [ "$NOTION_ENV" = prod ]
[ "$NOTION_WORKSPACE_ID" = b55c9c91-384d-452b-81db-d1ef79372b75 ]
[ "${NOTION_API_TOKEN+x}${NOTION_BASE_URL+x}${NOTION_API_BASE_URL+x}${NOTION_WORKERS_CONFIG_FILE+x}" = '' ]
case "$*" in
  'login --no-browser') printf 'helper-login\n' ;;
  'login poll') printf 'helper-poll\n' ;;
  'auth token --plain') printf 'private-export-error\n' >&2; printf '%8193s' x ;;
  *) exit 99 ;;
esac
"#).unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = command(&dir)
        .env_remove("NOTION_TOKEN")
        .env_remove("ZOTTION_AUTH_SERVER")
        .env("PATH", dir.path())
        .env("NOTION_KEYRING", "0")
        .env("NOTION_ENV", "dev")
        .env("NOTION_WORKSPACE_ID", "hidden-workspace")
        .env("NOTION_API_TOKEN", "private-env-token")
        .env("NOTION_BASE_URL", "http://invalid")
        .env("NOTION_API_BASE_URL", "http://invalid")
        .env("NOTION_WORKERS_CONFIG_FILE", "/invalid")
        .args(["--workspace", ID, "auth", "login", "--json"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    assert!(result.stdout.is_empty());
    let stderr = String::from_utf8(result.stderr).unwrap();
    assert!(
        stderr.starts_with("helper-login\nhelper-poll\n"),
        "{stderr}"
    );
    assert!(!stderr.contains("private-export-error"));
    assert!(!stderr.contains("private-env-token"));
    let error: Value = serde_json::from_str(stderr.lines().last().unwrap()).unwrap();
    assert_eq!(error["error"]["code"], 3);
    // An implicit selected UUID must select the same native workspace as the save target.
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    std::fs::write(
        dir.path().join("config/config.toml"),
        format!("[profiles.default]\nselected_workspace = '{ID}'\n"),
    )
    .unwrap();
    let selected = command(&dir)
        .env_remove("NOTION_TOKEN")
        .env_remove("ZOTTION_AUTH_SERVER")
        .env_remove("NOTION_WORKSPACE_ID")
        .env("PATH", dir.path())
        .args(["auth", "login", "--json"])
        .output()
        .unwrap();
    assert_eq!(selected.status.code(), Some(3));
    assert!(selected.stdout.is_empty());
    assert!(
        String::from_utf8(selected.stderr)
            .unwrap()
            .starts_with("helper-login\nhelper-poll\n")
    );
    // Reject conflicting credentials and aliases before starting the browser helper.
    for (token, workspace) in [(true, ID), (false, "workspace-alias")] {
        let mut cmd = command(&dir);
        if !token {
            cmd.env_remove("NOTION_TOKEN");
        }
        let result = cmd
            .env_remove("ZOTTION_AUTH_SERVER")
            .env("PATH", dir.path())
            .args(["--workspace", workspace, "auth", "login", "--json"])
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&result.stderr).unwrap()["error"]["code"],
            2
        );
    }
    let result = command(&dir)
        .env_remove("NOTION_TOKEN")
        .env_remove("ZOTTION_AUTH_SERVER")
        .env("PATH", dir.path().join("missing"))
        .args(["auth", "login", "--json"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(3));
    assert!(result.stdout.is_empty());
    let error: Value = serde_json::from_slice(&result.stderr).unwrap();
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("https://developers.notion.com/cli/get-started/installation")
    );
}
