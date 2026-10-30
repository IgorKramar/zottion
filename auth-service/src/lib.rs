use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const TTL: Duration = Duration::from_secs(300);
const MAX_SESSIONS: usize = 1024;
const API_VERSION: &str = "2026-03-11";

/// Secrets deliberately have no Debug implementation.
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    pub callback_url: String,
}
#[derive(Clone)]
struct Service {
    inner: Arc<Inner>,
}
struct Inner {
    config: Config,
    http: Client,
    sessions: Mutex<HashMap<String, Session>>,
    oauth_base: Url,
}
struct Session {
    state: String,
    challenge: String,
    expires: Instant,
    phase: Phase,
}
enum Phase {
    Pending,
    Exchanging,
    Ready(Value),
    Failed,
}

pub fn router(config: Config) -> Result<Router, String> {
    build(config, "https://api.notion.com/v1/")
}
fn build(config: Config, base: &str) -> Result<Router, String> {
    if config.client_id.trim().is_empty() || config.client_secret.trim().is_empty() {
        return Err("Требуются NOTION_CLIENT_ID и NOTION_CLIENT_SECRET".into());
    }
    let callback = secure_url(&config.callback_url)?;
    if callback.path() != "/callback" || callback.query().is_some() || callback.fragment().is_some()
    {
        return Err("Callback должен иметь фиксированный путь /callback без query/fragment".into());
    }
    let http = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| "Не удалось создать HTTP-клиент")?;
    let service = Service {
        inner: Arc::new(Inner {
            config,
            http,
            sessions: Mutex::new(HashMap::new()),
            oauth_base: Url::parse(base).map_err(|_| "Некорректный OAuth endpoint")?,
        }),
    };
    Ok(Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route("/sessions", post(start))
        .route("/callback", get(callback_handler))
        .route("/sessions/{id}/result", post(result))
        .route("/refresh", post(refresh))
        .route("/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(no_store))
        .with_state(service))
}
fn secure_url(value: &str) -> Result<Url, String> {
    let url = Url::parse(value).map_err(|_| "Некорректный URL".to_string())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || !(url.scheme() == "https"
            || url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
    {
        return Err("Требуется HTTPS; HTTP разрешён только для loopback".into());
    }
    Ok(url)
}
async fn no_store(request: axum::extract::Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::PRAGMA, "no-cache".parse().unwrap());
    headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error":message}))).into_response()
}
fn random() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}
fn clean(sessions: &mut HashMap<String, Session>) {
    let now = Instant::now();
    sessions.retain(|_, session| session.expires > now);
}
#[derive(Deserialize)]
struct Start {
    challenge: String,
}
async fn start(State(service): State<Service>, Json(input): Json<Start>) -> Response {
    if input.challenge.len() != 64 || !input.challenge.bytes().all(|b| b.is_ascii_hexdigit()) {
        return error(StatusCode::BAD_REQUEST, "Некорректный challenge");
    }
    let id = random();
    let state = random();
    let mut sessions = service.inner.sessions.lock().unwrap();
    clean(&mut sessions);
    if sessions.len() >= MAX_SESSIONS {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "Лимит сеансов; повторите позже",
        );
    }
    sessions.insert(
        id.clone(),
        Session {
            state: state.clone(),
            challenge: input.challenge,
            expires: Instant::now() + TTL,
            phase: Phase::Pending,
        },
    );
    let mut url = Url::parse("https://api.notion.com/v1/oauth/authorize").unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", service.inner.config.client_id.as_str()),
        ("redirect_uri", service.inner.config.callback_url.as_str()),
        ("response_type", "code"),
        ("owner", "user"),
        ("state", state.as_str()),
    ]);
    Json(json!({"id":id,"authorize_url":url.to_string(),"expires_in":TTL.as_secs()}))
        .into_response()
}
#[derive(Deserialize)]
struct Callback {
    state: String,
    code: Option<String>,
    error: Option<String>,
}
async fn callback_handler(
    State(service): State<Service>,
    Query(input): Query<Callback>,
) -> Response {
    let id = {
        let mut sessions = service.inner.sessions.lock().unwrap();
        clean(&mut sessions);
        // ponytail: bounded scan of 1024 ephemeral sessions; add a state index only if that ceiling grows.
        let Some((id, session)) = sessions
            .iter_mut()
            .find(|(_, session)| equal(&session.state, &input.state))
        else {
            return error(
                StatusCode::BAD_REQUEST,
                "Недействительный или истёкший state",
            );
        };
        if !matches!(session.phase, Phase::Pending) {
            return error(StatusCode::CONFLICT, "Callback уже использован");
        }
        session.phase = Phase::Exchanging;
        id.clone()
    };
    if input.error.is_some()
        || input
            .code
            .as_ref()
            .is_none_or(|code| code.is_empty() || code.len() > 4096)
    {
        if let Some(session) = service.inner.sessions.lock().unwrap().get_mut(&id) {
            session.phase = Phase::Failed;
        }
        return error(StatusCode::BAD_REQUEST, "Вход отменён или код не получен");
    }
    let payload = json!({"grant_type":"authorization_code","code":input.code.unwrap(),"redirect_uri":service.inner.config.callback_url});
    let tokens = exchange(&service, "oauth/token", payload)
        .await
        .and_then(tokens);
    let mut sessions = service.inner.sessions.lock().unwrap();
    clean(&mut sessions);
    let Some(session) = sessions.get_mut(&id) else {
        return error(StatusCode::GONE, "Сеанс истёк; начните вход заново");
    };
    match tokens {
        Ok(tokens) => {
            session.phase = Phase::Ready(tokens);
            (StatusCode::OK, "Вход завершён. Вернитесь в терминал.").into_response()
        }
        Err(()) => {
            session.phase = Phase::Failed;
            error(
                StatusCode::BAD_GATEWAY,
                "Обмен кода не завершён; начните вход заново",
            )
        }
    }
}
#[derive(Deserialize)]
struct ResultInput {
    verifier: String,
}
async fn result(
    State(service): State<Service>,
    Path(id): Path<String>,
    Json(input): Json<ResultInput>,
) -> Response {
    if input.verifier.len() < 32 || input.verifier.len() > 256 {
        return error(StatusCode::BAD_REQUEST, "Некорректный verifier");
    }
    let mut sessions = service.inner.sessions.lock().unwrap();
    clean(&mut sessions);
    let Some(session) = sessions.get(&id) else {
        return error(StatusCode::GONE, "Сеанс отсутствует или истёк");
    };
    if !equal(&session.challenge, &hash(&input.verifier)) {
        return error(StatusCode::FORBIDDEN, "Неверный verifier");
    }
    match &session.phase {
        Phase::Pending | Phase::Exchanging => {
            (StatusCode::ACCEPTED, Json(json!({"pending":true}))).into_response()
        }
        Phase::Ready(_) => {
            let session = sessions.remove(&id).unwrap();
            let Phase::Ready(tokens) = session.phase else {
                unreachable!()
            };
            Json(tokens).into_response()
        }
        Phase::Failed => {
            sessions.remove(&id);
            error(StatusCode::UNAUTHORIZED, "Вход не завершён; начните заново")
        }
    }
}
#[derive(Deserialize)]
struct Refresh {
    refresh_token: String,
}
async fn refresh(State(service): State<Service>, Json(input): Json<Refresh>) -> Response {
    if input.refresh_token.is_empty() || input.refresh_token.len() > 8192 {
        return error(StatusCode::BAD_REQUEST, "Некорректный refresh token");
    }
    match exchange(
        &service,
        "oauth/token",
        json!({"grant_type":"refresh_token","refresh_token":input.refresh_token}),
    )
    .await
    .and_then(tokens)
    {
        Ok(value) => Json(value).into_response(),
        Err(()) => error(
            StatusCode::UNAUTHORIZED,
            "Не удалось обновить токены; выполните новый вход",
        ),
    }
}
#[derive(Deserialize)]
struct Revoke {
    token: String,
}
async fn revoke(State(service): State<Service>, Json(input): Json<Revoke>) -> Response {
    if input.token.is_empty() || input.token.len() > 8192 {
        return error(StatusCode::BAD_REQUEST, "Некорректный token");
    }
    match exchange(&service, "oauth/revoke", json!({"token":input.token})).await {
        Ok(_) => Json(json!({"revoked":true})).into_response(),
        Err(()) => error(
            StatusCode::BAD_GATEWAY,
            "Отзыв не подтверждён; повторно проверьте доступ в Notion",
        ),
    }
}
async fn exchange(service: &Service, path: &str, payload: Value) -> Result<Value, ()> {
    // OAuth code exchange/rotation cannot safely be retried after a lost response.
    let response = service
        .inner
        .http
        .post(service.inner.oauth_base.join(path).map_err(|_| ())?)
        .basic_auth(
            &service.inner.config.client_id,
            Some(&service.inner.config.client_secret),
        )
        .header("Notion-Version", API_VERSION)
        .json(&payload)
        .send()
        .await
        .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        return Ok(Value::Null);
    }
    response.json().await.map_err(|_| ())
}
fn tokens(raw: Value) -> Result<Value, ()> {
    let access = raw["access_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or(())?;
    let refresh = raw["refresh_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or(())?;
    // Refresh responses can omit workspace/bot metadata; the client retains the prior identity.
    Ok(
        json!({"access_token":access,"refresh_token":refresh,"workspace_id":raw["workspace_id"],"bot_id":raw["bot_id"]}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    fn config() -> Config {
        Config {
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            callback_url: "http://127.0.0.1:9393/callback".into(),
        }
    }
    async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 65536).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    #[tokio::test]
    async fn health_contains_no_credentials_or_sessions() {
        let app = router(config()).unwrap();
        let (status, body) = call(&app, "GET", "/health", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"status":"ok"}));
    }
    #[tokio::test]
    async fn real_callback_handoff_rejects_wrong_state_verifier_and_replay() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let provider=Router::new().route("/oauth/token",post(|Json(input):Json<Value>|async move{assert_eq!(input["grant_type"],"authorization_code");Json(json!({"access_token":"test-access","refresh_token":"test-refresh","workspace_id":"workspace","bot_id":"bot"}))}));
        let task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let app = build(config(), &format!("http://{address}/")).unwrap();
        let verifier = random();
        let (_, session) = call(
            &app,
            "POST",
            "/sessions",
            json!({"challenge":hash(&verifier)}),
        )
        .await;
        let id = session["id"].as_str().unwrap();
        let state = Url::parse(session["authorize_url"].as_str().unwrap())
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(
            call(&app, "GET", "/callback?state=wrong&code=test", Value::Null)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let path = format!("/sessions/{id}/result");
        assert_eq!(
            call(&app, "POST", &path, json!({"verifier":random()}))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&app, "POST", &path, json!({"verifier":verifier}))
                .await
                .0,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            call(
                &app,
                "GET",
                &format!("/callback?state={state}&code=test"),
                Value::Null
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                "GET",
                &format!("/callback?state={state}&code=test"),
                Value::Null
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let (status, tokens) = call(&app, "POST", &path, json!({"verifier":verifier})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(tokens["access_token"], "test-access");
        assert_eq!(
            call(&app, "POST", &path, json!({"verifier":verifier}))
                .await
                .0,
            StatusCode::GONE
        );
        task.abort();
    }
    #[tokio::test]
    async fn refresh_and_revoke_use_provider_without_retries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let provider = Router::new()
            .route(
                "/oauth/token",
                post(move |Json(input): Json<Value>| {
                    let observed = observed.clone();
                    async move {
                        observed.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(input["grant_type"], "refresh_token");
                        if input["refresh_token"] == "revoked" {
                            return (StatusCode::UNAUTHORIZED, Json(json!({"error":"revoked"})));
                        }
                        (
                            StatusCode::OK,
                            Json(
                                json!({"access_token":"new-access","refresh_token":"new-refresh"}),
                            ),
                        )
                    }
                }),
            )
            .route(
                "/oauth/revoke",
                post(|Json(input): Json<Value>| async move {
                    assert_eq!(input["token"], "new-access");
                    StatusCode::NO_CONTENT
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let app = build(config(), &format!("http://{address}/")).unwrap();
        let (status, rotated) = call(
            &app,
            "POST",
            "/refresh",
            json!({"refresh_token":"old-refresh"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rotated["refresh_token"], "new-refresh");
        assert_eq!(
            call(&app, "POST", "/refresh", json!({"refresh_token":"revoked"}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            call(&app, "POST", "/revoke", json!({"token":"new-access"}))
                .await
                .1["revoked"],
            true
        );
        task.abort();
    }
    #[tokio::test]
    async fn expiry_and_capacity_are_enforced() {
        let service = Service {
            inner: Arc::new(Inner {
                config: config(),
                http: Client::new(),
                sessions: Mutex::new(HashMap::new()),
                oauth_base: Url::parse("https://api.notion.com/v1/").unwrap(),
            }),
        };
        service.inner.sessions.lock().unwrap().insert(
            "expired".into(),
            Session {
                state: "state".into(),
                challenge: hash(&"v".repeat(64)),
                expires: Instant::now() - Duration::from_secs(1),
                phase: Phase::Ready(json!({"access_token":"never-released"})),
            },
        );
        assert_eq!(
            result(
                State(service.clone()),
                Path("expired".into()),
                Json(ResultInput {
                    verifier: "v".repeat(64)
                })
            )
            .await
            .status(),
            StatusCode::GONE
        );
        {
            let mut sessions = service.inner.sessions.lock().unwrap();
            for i in 0..MAX_SESSIONS {
                sessions.insert(
                    i.to_string(),
                    Session {
                        state: i.to_string(),
                        challenge: "f".repeat(64),
                        expires: Instant::now() + TTL,
                        phase: Phase::Pending,
                    },
                );
            }
        }
        assert_eq!(
            start(
                State(service),
                Json(Start {
                    challenge: "f".repeat(64)
                })
            )
            .await
            .status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert!(
            router(Config {
                callback_url: "http://evil.test/callback".into(),
                ..config()
            })
            .is_err()
        );
    }
}
