use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};
use zottion::{
    app::{App, matches_payload, unchanged, validate_property},
    content::BlockId,
    notion::Client,
    store::{OperationState, Scope, Store},
};
const ID: &str = "b55c9c91-384d-452b-81db-d1ef79372b75";
fn block(text: &str) -> Value {
    json!({"object":"block","id":ID,"last_edited_time":"before","type":"paragraph","paragraph":{"rich_text":[{"type":"text","text":{"content":text}}]}})
}
fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(&dir.path().join("test.sqlite3"), Scope::new("p", "w", "id")).unwrap()
}
fn mock(replies: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = server.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        replies.into_iter().map(|(status,body)|{let(mut stream,_)=server.accept().unwrap();stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();let mut bytes=[0;65536];let size=stream.read(&mut bytes).unwrap();let body=body.to_string();write!(stream,"HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();String::from_utf8_lossy(&bytes[..size]).to_string()}).collect()
    });
    (format!("http://{addr}/v1/"), handle)
}
#[tokio::test]
async fn draft_survives_restart_and_cache_clear() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(store(&dir), None, true);
    let draft = app
        .stage_block(block("old"), "Новый текст".into())
        .await
        .unwrap();
    let saved = app.apply_draft(&draft.id).await.unwrap();
    assert_eq!(saved["published"], false);
    app.store.clear_cache().await.unwrap();
    let reopened = App::new(store(&dir), None, true);
    assert_eq!(
        reopened
            .store
            .draft(&draft.id)
            .await
            .unwrap()
            .unwrap()
            .local["text"],
        "Новый текст"
    );
    assert!(reopened.store.operations().await.unwrap().is_empty());
}
#[tokio::test]
async fn conflict_keeps_all_versions_and_never_sends_patch() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock(vec![(200, block("remote"))]);
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let draft = app
        .stage_block(block("base"), "local".into())
        .await
        .unwrap();
    assert_eq!(app.apply_draft(&draft.id).await.unwrap_err().code, 5);
    assert_eq!(
        app.store.operations().await.unwrap()[0].state,
        OperationState::Conflict
    );
    assert_eq!(
        app.store.draft(&draft.id).await.unwrap().unwrap().local["text"],
        "local"
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].starts_with("GET"));
}
#[tokio::test]
async fn unknown_write_is_not_repeated_and_can_be_verified() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock(vec![
        (200, block("base")),
        (503, json!({})),
        (200, block("local")),
    ]);
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let draft = app
        .stage_block(block("base"), "local".into())
        .await
        .unwrap();
    assert_eq!(app.apply_draft(&draft.id).await.unwrap_err().code, 8);
    assert_eq!(app.apply_draft(&draft.id).await.unwrap_err().code, 8);
    let op = app.store.operations().await.unwrap().remove(0);
    assert_eq!(op.state, OperationState::Unknown);
    let check = app.check_operation(&op.id).await.unwrap();
    assert_eq!(check["matches_local"], true);
    assert_eq!(
        app.store.operation(&op.id).await.unwrap().state,
        OperationState::Confirmed
    );
    assert!(app.store.draft(&draft.id).await.unwrap().is_none());
    let requests = server.join().unwrap();
    assert_eq!(
        requests.iter().filter(|r| r.starts_with("PATCH")).count(),
        1
    );
}
#[test]
fn narrow_conflicts_and_property_types() {
    let base = block("old");
    let mut remote = base.clone();
    remote["paragraph"]["color"] = json!("blue");
    let payload = json!({"paragraph":{"rich_text":[{"type":"text","text":{"content":"old"}}]}});
    assert!(unchanged(&base, &remote, &payload));
    assert!(matches_payload(&remote, &payload));
    remote["paragraph"]["rich_text"][0]["text"]["content"] = json!("different");
    assert!(!unchanged(&base, &remote, &payload));
    assert!(!matches_payload(&remote, &payload));
    assert!(validate_property("formula", &json!({"formula":1})).is_err());
    assert!(validate_property("checkbox", &json!({"checkbox":"yes"})).is_err());
    assert!(validate_property("relation", &json!({"relation":[{"id":"bad"}]})).is_err());
    assert!(validate_property("status", &json!({"status":{"name":"Done"}})).is_ok());
    assert!(ID.parse::<BlockId>().is_ok());
}

#[test]
fn formatting_and_property_ids_preserve_identity() {
    let base = json!({"id":ID,"type":"paragraph","paragraph":{"rich_text":[{"type":"text","text":{"content":"Привет мир","link":{"url":"https://example.com"}},"annotations":{"italic":true}},{"type":"mention","plain_text":"Игорь","mention":{"type":"user","user":{"id":ID}}}]}});
    let patch = zottion::content::format_patch(&base, 0, Some(6), "bold", json!(true)).unwrap();
    assert_eq!(
        patch["paragraph"]["rich_text"][0]["annotations"]["italic"],
        true
    );
    assert_eq!(
        patch["paragraph"]["rich_text"][0]["annotations"]["bold"],
        true
    );
    assert_eq!(
        patch["paragraph"]["rich_text"][0]["text"]["link"],
        base["paragraph"]["rich_text"][0]["text"]["link"]
    );
    assert_eq!(
        patch["paragraph"]["rich_text"][2]["mention"],
        base["paragraph"]["rich_text"][1]["mention"]
    );
    assert!(
        zottion::content::format_patch(&base, 11, Some(12), "link", json!("https://example.com"))
            .is_err()
    );
    let page = json!({"properties":{"Статус":{"id":"statusId","type":"status","status":{"id":"todo","name":"Todo"}}}});
    let payload = json!({"properties":{"statusId":{"status":{"name":"Todo"}}}});
    assert!(matches_payload(&page, &payload));
    let mut remote = page.clone();
    remote["properties"]["Статус"]["status"]["name"] = json!("Done");
    assert!(!unchanged(&page, &remote, &payload));
}

#[tokio::test]
async fn incompatible_and_invalid_drafts_never_publish_or_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let app = App::new(store(&dir), None, true);
    let protected = json!({"object":"block","id":ID,"type":"bookmark","bookmark":{"url":"https://example.com"}});
    assert_eq!(
        app.save_block(protected.clone(), "blocked".into())
            .await
            .unwrap_err()
            .code,
        7
    );
    assert!(app.store.drafts().await.unwrap().is_empty());
    assert!(app.store.operations().await.unwrap().is_empty());
    let path = format!("blocks/{ID}");
    let draft = app
        .stage_block(block("base"), "local".into())
        .await
        .unwrap();
    assert_eq!(
        app.stage(
            "PATCH",
            &path,
            Some(&path),
            block("base"),
            json!({"paragraph":{"color":"red"}})
        )
        .await
        .unwrap_err()
        .code,
        5
    );
    assert_eq!(
        app.store.draft(&draft.id).await.unwrap().unwrap().local["text"],
        "local"
    );
    assert_eq!(
        app.stage_block(protected, "blocked".into())
            .await
            .unwrap_err()
            .code,
        7
    );
    assert_eq!(
        app.store.draft(&draft.id).await.unwrap().unwrap().local["text"],
        "local"
    );
    app.store
        .put_draft(
            "pages/invalid",
            json!({"editor_invalid":{"Tags":true}}),
            json!({}),
        )
        .await
        .unwrap();
    assert_eq!(app.apply_draft("pages/invalid").await.unwrap_err().code, 2);
}

#[tokio::test]
async fn complete_relation_preflight_does_not_conflict_with_truncated_page() {
    let dir = tempfile::tempdir().unwrap();
    let relations = vec![
        json!({"id":ID}),
        json!({"id":"c02fc1d3-db8b-45c5-a222-27595b15aea7"}),
    ];
    let base = json!({"object":"page","id":ID,"parent":{"data_source_id":ID},"properties":{"Links":{"id":"r","type":"relation","has_more":false,"relation":relations}}});
    let mut truncated = base.clone();
    truncated["properties"]["Links"]["relation"] = json!([relations[0]]);
    truncated["properties"]["Links"]["has_more"] = json!(true);
    let schema = json!({"properties":{"Links":{"id":"r","type":"relation","relation":{}}}});
    let full = json!({"object":"list","results":relations.iter().map(|v|json!({"object":"property_item","type":"relation","relation":v})).collect::<Vec<_>>(),"has_more":false,"next_cursor":null});
    let (url, server) = mock(vec![
        (200, schema),
        (200, truncated),
        (200, full),
        (200, base.clone()),
    ]);
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let path = format!("pages/{ID}");
    let value = app
        .mutate(
            "PATCH",
            &path,
            Some(&path),
            base,
            json!({"properties":{"Links":{"relation":relations}}}),
        )
        .await
        .unwrap();
    assert_eq!(value["object"], "page");
    assert!(app.store.drafts().await.unwrap().is_empty());
    assert_eq!(
        app.store.operations().await.unwrap()[0].state,
        OperationState::Confirmed
    );
    assert_eq!(
        server
            .join()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("PATCH"))
            .count(),
        1
    );
}

#[tokio::test]
async fn old_unknown_confirmation_preserves_new_block_draft() {
    for manual in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut replies = vec![(200, block("base")), (503, json!({}))];
        if !manual {
            replies.push((200, block("A")));
        }
        let (url, server) = mock(replies);
        let app = App::new(
            store(&dir),
            Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
            false,
        );
        let draft = app.stage_block(block("base"), "A".into()).await.unwrap();
        assert_eq!(app.apply_draft(&draft.id).await.unwrap_err().code, 8);
        let op = app.store.operations().await.unwrap().remove(0);
        app.stage_block(block("base"), "B".into()).await.unwrap();
        if manual {
            app.acknowledge_operation(&op.id, true).await.unwrap();
        } else {
            assert_eq!(
                app.check_operation(&op.id).await.unwrap()["matches_local"],
                true
            );
        }
        assert_eq!(
            app.store.operation(&op.id).await.unwrap().state,
            OperationState::Confirmed
        );
        assert_eq!(
            app.store.draft(&draft.id).await.unwrap().unwrap().local["text"],
            "B"
        );
        assert_eq!(
            server
                .join()
                .unwrap()
                .iter()
                .filter(|r| r.starts_with("PATCH"))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn ordinary_confirmation_preserves_new_block_draft() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let (started, received) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut started = Some(started);
        for (index, body) in [block("base"), block("A")].into_iter().enumerate() {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = [0; 65536];
            assert!(stream.read(&mut bytes).unwrap() > 0);
            if index == 1 {
                started.take().unwrap().send(()).unwrap();
                wait.recv_timeout(Duration::from_secs(3)).unwrap();
            }
            let body = body.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let draft = app.stage_block(block("base"), "A".into()).await.unwrap();
    let writer = app.clone();
    let id = draft.id.clone();
    let applying = tokio::spawn(async move { writer.apply_draft(&id).await });
    received.await.unwrap();
    app.stage_block(block("base"), "B".into()).await.unwrap();
    release.send(()).unwrap();
    applying.await.unwrap().unwrap();
    server.join().unwrap();
    assert_eq!(
        app.store.draft(&draft.id).await.unwrap().unwrap().local["text"],
        "B"
    );
}

fn creation_mock() -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let mut deadline = std::time::Instant::now() + Duration::from_millis(400);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let mut bytes = [0; 65536];
                    let size = stream.read(&mut bytes).unwrap();
                    requests.push(String::from_utf8_lossy(&bytes[..size]).to_string());
                    let body = json!({"object":"page", "id":ID}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    deadline = std::time::Instant::now() + Duration::from_millis(200);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("{error}"),
            }
        }
        requests
    });
    (url, server)
}

#[tokio::test]
async fn concurrent_creation_applies_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = creation_mock();
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let other = App::new(store(&dir), app.client.clone(), false);
    let id = app
        .stage("POST", "pages", None, Value::Null, json!({"title":"A"}))
        .await
        .unwrap();
    let guard = app.store.acquire_write().await.unwrap();
    let first = app.clone();
    let first_id = id.clone();
    let one = tokio::spawn(async move { first.apply_draft(&first_id).await });
    let two = tokio::spawn(async move { other.apply_draft(&id).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(guard);
    let results = [one.await.unwrap(), two.await.unwrap()];
    let requests = server.join().unwrap();
    assert_eq!(requests.iter().filter(|r| r.starts_with("POST")).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .next()
            .unwrap()
            .code,
        4
    );
}

#[tokio::test]
async fn legacy_confirmed_creation_is_cleaned_without_repeating_post() {
    for newer in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (url, server) = creation_mock();
        let app = App::new(
            store(&dir),
            Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
            false,
        );
        let id = app
            .stage("POST", "pages", None, Value::Null, json!({"title":"A"}))
            .await
            .unwrap();
        let draft = app.store.draft(&id).await.unwrap().unwrap();
        let mut context = draft.base.clone();
        context["draft_id"] = json!(id);
        let op = app
            .store
            .prepare("POST", "pages", context, draft.local.clone())
            .await
            .unwrap();
        app.store
            .set_operation(
                &op.id,
                OperationState::Confirmed,
                Some(json!({"object":"page", "id":ID})),
            )
            .await
            .unwrap();
        if newer {
            app.store
                .put_draft(&id, draft.base.clone(), json!({"title":"B"}))
                .await
                .unwrap();
            assert_eq!(app.apply_draft(&id).await.unwrap_err().code, 5);
            assert_eq!(
                app.store.draft(&id).await.unwrap().unwrap().local["title"],
                "B"
            );
        } else {
            assert_eq!(app.apply_draft(&id).await.unwrap()["id"], ID);
            assert!(app.store.draft(&id).await.unwrap().is_none());
        }
        assert!(server.join().unwrap().is_empty());
    }
}

#[tokio::test]
async fn failed_confirmation_is_partial_and_does_not_repeat_creation() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = creation_mock();
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let id = app
        .stage("POST", "pages", None, Value::Null, json!({"title":"A"}))
        .await
        .unwrap();
    let connection = rusqlite::Connection::open(dir.path().join("test.sqlite3")).unwrap();
    connection.execute_batch("CREATE TRIGGER reject_cleanup BEFORE DELETE ON drafts BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    let error = app.apply_draft(&id).await.unwrap_err();
    assert_eq!(error.code, 8);
    assert_eq!(error.partial.unwrap()["id"], ID);
    assert_eq!(
        app.store.operations().await.unwrap()[0].state,
        OperationState::Sending
    );
    assert!(app.store.draft(&id).await.unwrap().is_some());
    assert_eq!(app.apply_draft(&id).await.unwrap_err().code, 8);
    assert_eq!(
        server
            .join()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("POST"))
            .count(),
        1
    );
}

#[tokio::test]
async fn property_partial_keeps_all_received_results_and_resume_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock(vec![
        (
            200,
            json!({"object":"list","results":[{"id":"a"}],"has_more":true,"next_cursor":"second"}),
        ),
        (
            200,
            json!({"object":"list","results":[{"id":"b"}],"has_more":true,"next_cursor":"third"}),
        ),
        (404, json!({})),
    ]);
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    let error = app
        .property(ID.parse().unwrap(), "r", true, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, 8);
    let value = error.partial.unwrap();
    assert_eq!(value["object"], "list");
    assert_eq!(value["results"], json!([{"id":"a"},{"id":"b"}]));
    assert_eq!(value["next_cursor"], "third");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("start_cursor=second"));
    assert!(requests[2].contains("start_cursor=third"));
}

#[tokio::test]
async fn property_preserves_incomplete_status_and_validates_single_page() {
    let status = json!({"type":"incomplete","incomplete_reason":"query_result_limit_reached"});
    for replies in [
        vec![
            (
                200,
                json!({"object":"list","results":[{"id":"a"}],"has_more":true,"next_cursor":"second"}),
            ),
            (
                200,
                json!({"object":"list","results":[{"id":"b"}],"has_more":false,"next_cursor":null,"request_status":status}),
            ),
        ],
        vec![(
            200,
            json!({"object":"list","results":[{"id":"a"},{"id":"b"}],"has_more":false,"next_cursor":null,"request_status":status}),
        )],
    ] {
        let all = replies.len() > 1;
        let dir = tempfile::tempdir().unwrap();
        let (url, server) = mock(replies);
        let app = App::new(
            store(&dir),
            Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
            false,
        );
        let error = app
            .property(ID.parse().unwrap(), "r", all, None)
            .await
            .unwrap_err();
        assert_eq!(error.code, 8);
        let value = error.partial.unwrap();
        assert_eq!(value["results"], json!([{"id":"a"},{"id":"b"}]));
        assert_eq!(value["request_status"], status);
        server.join().unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = mock(vec![(
        200,
        json!({"object":"list","results":[],"has_more":true,"next_cursor":null}),
    )]);
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
        false,
    );
    assert_eq!(
        app.property(ID.parse().unwrap(), "r", false, None)
            .await
            .unwrap_err()
            .code,
        8
    );
    server.join().unwrap();
}

#[tokio::test]
async fn view_query_uses_original_query_id_and_keeps_incomplete_page() {
    const QUERY_ID: &str = "c02fc1d3-db8b-45c5-a222-27595b15aea7";
    for incomplete in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut page = json!({"object":"view_query","results":[{"id":"b"}],"has_more":false,"next_cursor":null});
        if incomplete {
            page["request_status"] =
                json!({"type":"incomplete","incomplete_reason":"query_result_limit_reached"});
        }
        let status = page["request_status"].clone();
        let (url, server) = mock(vec![
            (
                200,
                json!({"object":"view_query","id":QUERY_ID,"results":[{"id":"a"}],"has_more":true,"next_cursor":"second"}),
            ),
            (200, page),
        ]);
        let app = App::new(
            store(&dir),
            Some(Client::with_base_url("test".into(), Duration::from_secs(3), &url).unwrap()),
            false,
        );
        let result = app.query_view(ID.parse().unwrap(), true).await;
        let value = if incomplete {
            let error = result.unwrap_err();
            assert_eq!(error.code, 8);
            let value = error.partial.unwrap();
            assert_eq!(value["request_status"], status);
            value
        } else {
            result.unwrap()
        };
        assert_eq!(value["id"], QUERY_ID);
        assert_eq!(value["results"], json!([{"id":"a"},{"id":"b"}]));
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].starts_with(&format!("POST /v1/views/{ID}/queries ")));
        assert!(requests[1].starts_with(&format!(
            "GET /v1/views/{ID}/queries/{QUERY_ID}?start_cursor=second "
        )));
    }
}

#[tokio::test]
async fn view_query_deadline_includes_post_and_returns_partial() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1/", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for body in [
            json!({"object":"view_query","id":ID,"results":[{"id":"a"}],"has_more":true,"next_cursor":"second"}),
            json!({"object":"view_query","results":[{"id":"b"}],"has_more":false,"next_cursor":null}),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 65536];
            let size = stream.read(&mut bytes).unwrap();
            requests.push(String::from_utf8_lossy(&bytes[..size]).to_string());
            std::thread::sleep(Duration::from_millis(150));
            let body = body.to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
        requests
    });
    let app = App::new(
        store(&dir),
        Some(Client::with_base_url("test".into(), Duration::from_millis(250), &url).unwrap()),
        false,
    );
    let error = app.query_view(ID.parse().unwrap(), true).await.unwrap_err();
    assert_eq!(error.code, 8);
    let value = error.partial.unwrap();
    assert_eq!(value["id"], ID);
    assert_eq!(value["results"], json!([{"id":"a"}]));
    assert_eq!(value["next_cursor"], "second");
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].starts_with(&format!(
        "GET /v1/views/{ID}/queries/{ID}?start_cursor=second "
    )));
}
