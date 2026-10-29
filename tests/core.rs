use reqwest::Method;
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
    time::{Duration, Instant},
};
use zottion::{
    content::{self, BlockId, PageId},
    notion::Client,
    store::{OperationState, Scope, Store},
};

#[test]
fn ids_rich_text_and_protected_content() {
    let id = "b55c9c91-384d-452b-81db-d1ef79372b75";
    assert_eq!(id.parse::<PageId>().unwrap().to_string(), id);
    assert_eq!(
        "https://notion.so/title-b55c9c91384d452b81dbd1ef79372b75?pvs=4"
            .parse::<BlockId>()
            .unwrap()
            .to_string(),
        id
    );
    assert!(
        "https://evil.test/b55c9c91384d452b81dbd1ef79372b75"
            .parse::<PageId>()
            .is_err()
    );
    let original = json!({"object":"block","id":id,"type":"paragraph","paragraph":{"color":"red","rich_text":[
        {"type":"text","plain_text":"Привет","href":"https://example.com","annotations":{"bold":true,"color":"blue"},"text":{"content":"Привет","link":{"url":"https://example.com"}}},
        {"type":"mention","plain_text":"@Игорь","mention":{"type":"user","user":{"object":"user","id":id}},"annotations":{"italic":true}}
    ]}});
    let patch = content::text_patch(&original, "Приветы@Игорь").unwrap();
    assert_eq!(
        patch["paragraph"]["rich_text"][0]["text"]["content"],
        "Приветы"
    );
    assert_eq!(
        patch["paragraph"]["rich_text"][0]["annotations"],
        original["paragraph"]["rich_text"][0]["annotations"]
    );
    assert_eq!(
        patch["paragraph"]["rich_text"][0]["text"]["link"],
        original["paragraph"]["rich_text"][0]["text"]["link"]
    );
    assert_eq!(
        patch["paragraph"]["rich_text"][1],
        content::rich_input(&original["paragraph"]["rich_text"][1])
    );
    assert!(
        patch["paragraph"]["rich_text"][0]
            .get("plain_text")
            .is_none()
    );
    assert!(patch.get("id").is_none());
    assert!(content::text_patch(&original, "Всё заменено").is_err());
    assert!(content::text_patch(&json!({"type":"unsupported","unsupported":{}}), "oops").is_err());
    assert!(content::block_patch(&original, json!({"paragraph":{"checked":true}})).is_err());
    let empty = json!({"type":"paragraph","paragraph":{"rich_text":[]}});
    assert_eq!(
        content::text_patch(&empty, "я".repeat(2001).as_str())
            .unwrap_err()
            .code,
        2
    );
    for kind in content::TEXT_BLOCKS {
        assert_eq!(content::new_block(kind, "Текст").unwrap()["type"], *kind);
    }
    assert!(content::new_block("child_page", "bad").is_err());
}

#[tokio::test]
async fn durable_scoped_state_and_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite3");
    let store = Store::open(&path, Scope::new("p", "w", "credential-a")).unwrap();
    let other = Store::open(&path, Scope::new("p", "w", "credential-b")).unwrap();
    store.put_snapshot("block","b",json!({"object":"block","type":"paragraph","paragraph":{"rich_text":[{"text":{"content":"Привет мир"}}]}}),true).await.unwrap();
    store
        .put_draft("b", json!({"old":1}), json!({"new":2}))
        .await
        .unwrap();
    assert!(other.snapshot("block", "b").await.unwrap().is_none());
    assert!(other.drafts().await.unwrap().is_empty());
    assert!(
        other.search("Привет").await.unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.search("Привет").await.unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let guard = store.acquire_write().await.unwrap();
    let operation = store
        .prepare("POST", "pages", Value::Null, json!({"title":"hello"}))
        .await
        .unwrap();
    store
        .set_operation(&operation.id, OperationState::Sending, None)
        .await
        .unwrap();
    let reader = Store::open(&path, Scope::new("p", "w", "credential-a")).unwrap();
    assert_eq!(
        reader.operation(&operation.id).await.unwrap().state,
        OperationState::Sending
    );
    drop(guard);
    let _guard = reader.acquire_write().await.unwrap();
    assert_eq!(
        reader.operation(&operation.id).await.unwrap().state,
        OperationState::Unknown
    );
    reader.clear_cache().await.unwrap();
    assert!(reader.snapshot("block", "b").await.unwrap().is_none());
    assert_eq!(
        reader.draft("b").await.unwrap().unwrap().local,
        json!({"new":2})
    );
    assert_eq!(reader.operations().await.unwrap().len(), 1);
    let reopened = Store::open(&path, Scope::new("p", "w", "credential-a")).unwrap();
    assert!(reopened.draft("b").await.unwrap().is_some());
}

fn mock(responses: Vec<(u16, Value, &'static str)>) -> (String, thread::JoinHandle<Vec<String>>) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    server.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        let mut responses = responses.into_iter();
        let mut deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match server.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut buf = [0; 65536];
                    let len = stream.read(&mut buf).unwrap();
                    requests.push(String::from_utf8_lossy(&buf[..len]).into());
                    if let Some((status, body, headers)) = responses.next() {
                        let body = body.to_string();
                        write!(stream,"HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",body.len()).unwrap();
                    }
                    deadline = Instant::now() + Duration::from_millis(200);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        }
        requests
    });
    (format!("http://{addr}/v1/"), handle)
}
#[tokio::test]
async fn reads_retry_and_writes_never_repeat() {
    let (url, server) = mock(vec![
        (429, json!({}), "Retry-After: 0\r\n"),
        (529, json!({}), "Retry-After: 0\r\n"),
        (200, json!({"ok":true}), ""),
    ]);
    let client = Client::with_base_url("test-token".into(), Duration::from_secs(2), &url).unwrap();
    assert_eq!(client.get("pages/id").await.unwrap()["ok"], true);
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(
        requests[0]
            .to_lowercase()
            .contains("notion-version: 2026-03-11")
    );
    let (url, server) = mock(vec![(503, json!({}), "")]);
    let client = Client::with_base_url("test-token".into(), Duration::from_secs(2), &url).unwrap();
    assert_eq!(
        client
            .write(Method::POST, "pages", json!({}))
            .await
            .unwrap_err()
            .code,
        8
    );
    assert_eq!(server.join().unwrap().len(), 1);
    let (url, server) = mock(vec![(429, json!({}), "Retry-After: 99\r\n")]);
    let client =
        Client::with_base_url("test-token".into(), Duration::from_millis(100), &url).unwrap();
    let start = Instant::now();
    assert_eq!(client.get("pages/id").await.unwrap_err().code, 6);
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(server.join().unwrap().len(), 1);
}
#[tokio::test]
async fn pagination_keeps_cursor_and_partial_failure() {
    let (url, server) = mock(vec![
        (
            200,
            json!({"results":[{"id":"a"}],"has_more":true,"next_cursor":"second"}),
            "",
        ),
        (
            200,
            json!({"results":[{"id":"b"}],"has_more":false,"next_cursor":null}),
            "",
        ),
    ]);
    let client = Client::with_base_url("test-token".into(), Duration::from_secs(2), &url).unwrap();
    let result = client
        .list_get("blocks/id/children", &[], true)
        .await
        .unwrap();
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert_eq!(result["has_more"], false);
    let requests = server.join().unwrap();
    assert!(requests[1].contains("start_cursor=second"));
    let (url, server) = mock(vec![
        (
            200,
            json!({"results":[{"id":"a"}],"has_more":true,"next_cursor":"second"}),
            "",
        ),
        (404, json!({}), ""),
    ]);
    let client = Client::with_base_url("test-token".into(), Duration::from_secs(2), &url).unwrap();
    let error = client
        .list_get("blocks/id/children", &[], true)
        .await
        .unwrap_err();
    assert_eq!(error.code, 8);
    let message = error.message;
    let output = zottion::cli::envelope(error.partial.unwrap(), "p", "w", false, true);
    assert_eq!(output["data"]["results"], json!([{"id":"a"}]));
    assert_eq!(output["meta"]["next_cursor"], "second");
    assert_eq!(output["meta"]["partial"], true);
    assert!(message.contains("Ресурс недоступен"));
    assert_eq!(server.join().unwrap().len(), 2);
}

#[test]
fn concurrent_initial_migration_and_future_schema_guard() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.sqlite3");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                Store::open(&path, Scope::new("p", "w", "c"))
            })
        })
        .collect();
    let stores: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap().unwrap())
        .collect();
    assert_eq!(stores.len(), 4);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    assert!(Store::open(&path, Scope::new("p", "w", "c")).is_err());
}

#[tokio::test]
async fn snapshot_batch_rolls_back_snapshots_and_search_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("batch.sqlite3");
    let store = Store::open(&path, Scope::new("p", "w", "c")).unwrap();
    let old = json!({"object":"block", "type":"paragraph", "paragraph":{"rich_text":[{"text":{"content":"Original"}}]}});
    let new = json!({"object":"block", "type":"paragraph", "paragraph":{"rich_text":[{"text":{"content":"Replacement"}}]}});
    store
        .put_snapshot("resource", "good", old.clone(), true)
        .await
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_bad_snapshot BEFORE INSERT ON snapshots WHEN NEW.id='bad' BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    assert!(
        store
            .put_snapshots(vec![
                ("resource".into(), "good".into(), new.clone(), false),
                ("resource".into(), "bad".into(), new.clone(), true),
            ])
            .await
            .is_err()
    );
    assert_eq!(store.snapshot("resource", "good").await.unwrap(), Some(old));
    assert_eq!(
        store.snapshot_complete("resource", "good").await.unwrap(),
        Some(true)
    );
    assert!(store.snapshot("resource", "bad").await.unwrap().is_none());
    assert_eq!(
        store.search("Original").await.unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        store.search("Replacement").await.unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    connection
        .execute_batch("DROP TRIGGER reject_bad_snapshot;")
        .unwrap();
    store
        .put_snapshots(vec![
            ("resource".into(), "good".into(), new.clone(), false),
            ("resource".into(), "bad".into(), new, true),
        ])
        .await
        .unwrap();
    assert_eq!(
        store.search("Replacement").await.unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let other = Store::open(&path, Scope::new("p", "w", "other")).unwrap();
    assert!(other.snapshot("resource", "good").await.unwrap().is_none());
    assert!(
        other.search("Replacement").await.unwrap()["results"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn confirmation_and_draft_cleanup_roll_back_together() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("confirmation.sqlite3");
    let store = Store::open(&path, Scope::new("p", "w", "c")).unwrap();
    store
        .put_draft("new", json!({"path":"pages"}), json!({"title":"A"}))
        .await
        .unwrap();
    let draft = store.draft("new").await.unwrap().unwrap();
    let op = store
        .prepare(
            "POST",
            "pages",
            json!({"draft_id":"new"}),
            draft.local.clone(),
        )
        .await
        .unwrap();
    store
        .set_operation(&op.id, OperationState::Sending, None)
        .await
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_draft_cleanup BEFORE DELETE ON drafts BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    assert!(
        store
            .confirm_operation(&op.id, Some(json!({"id":"created"})), Some(draft.clone()))
            .await
            .is_err()
    );
    assert_eq!(
        store.operation(&op.id).await.unwrap().state,
        OperationState::Sending
    );
    assert!(store.operation(&op.id).await.unwrap().response.is_none());
    assert!(store.draft("new").await.unwrap().is_some());
    connection
        .execute_batch("DROP TRIGGER reject_draft_cleanup;")
        .unwrap();
    store
        .confirm_operation(&op.id, Some(json!({"id":"created"})), Some(draft))
        .await
        .unwrap();
    assert_eq!(
        store.operation(&op.id).await.unwrap().state,
        OperationState::Confirmed
    );
    assert!(store.draft("new").await.unwrap().is_none());
}

#[tokio::test]
async fn pagination_preserves_incomplete_status_and_rejects_missing_cursor() {
    let status = json!({"type":"incomplete","incomplete_reason":"query_result_limit_reached"});
    let (url, server) = mock(vec![
        (
            200,
            json!({"results":[{"id":"a"}],"has_more":true,"next_cursor":"second","request_status":{"type":"complete"}}),
            "",
        ),
        (
            200,
            json!({"results":[{"id":"b"}],"has_more":false,"next_cursor":null,"request_status":status}),
            "",
        ),
    ]);
    let client = Client::with_base_url("test-token".into(), Duration::from_secs(2), &url).unwrap();
    let error = client.list_get("items", &[], true).await.unwrap_err();
    assert_eq!(error.code, 8);
    let value = error.partial.unwrap();
    assert_eq!(value["results"], json!([{"id":"a"},{"id":"b"}]));
    assert_eq!(value["request_status"], status);
    assert_eq!(server.join().unwrap().len(), 2);

    for all in [false, true] {
        let (url, server) = mock(vec![(
            200,
            json!({"results":[],"has_more":true,"next_cursor":null}),
            "",
        )]);
        let client =
            Client::with_base_url("test-token".into(), Duration::from_secs(2), &url).unwrap();
        let error = client.list_get("items", &[], all).await.unwrap_err();
        assert_eq!(error.code, 8);
        assert_eq!(error.partial.unwrap()["has_more"], true);
        assert_eq!(server.join().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn api_path_cannot_change_origin_after_url_join() {
    let client = Client::with_base_url(
        "test-token".into(),
        Duration::from_millis(20),
        "http://127.0.0.1:2/v1/",
    )
    .unwrap();
    let error = client.get(r"\\127.0.0.1:1/path").await.unwrap_err();
    assert_eq!(error.code, 2);
}
