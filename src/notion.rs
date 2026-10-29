use crate::{Error, Result};
use reqwest::{Client as HttpClient, Method, Url};
use serde_json::{Value, json};
use std::{collections::HashSet, time::Duration};
use tokio::time::{Instant, sleep, timeout};

pub const API_VERSION: &str = "2026-03-11";
#[derive(Clone)]
pub struct Client {
    http: HttpClient,
    base: Url,
    token: String,
    timeout: Duration,
}
impl Client {
    pub fn new(token: String, timeout: Duration) -> Result<Self> {
        Self::with_base_url(token, timeout, "https://api.notion.com/v1/")
    }
    pub fn with_base_url(token: String, timeout: Duration, base: &str) -> Result<Self> {
        if token.trim().is_empty() {
            return Err(Error::new(3, "Токен Notion отсутствует"));
        }
        if timeout.is_zero() {
            return Err(Error::input("Срок запроса должен быть больше нуля"));
        }
        let base = Url::parse(base).map_err(|_| Error::input("Некорректный URL API"))?;
        if base.scheme() != "https"
            && !(base.scheme() == "http"
                && base
                    .host_str()
                    .is_some_and(|host| matches!(host, "127.0.0.1" | "localhost" | "[::1]")))
        {
            return Err(Error::input("Для API необходим HTTPS"));
        }
        let http = HttpClient::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::new(1, "Не удалось создать HTTP-клиент"))?;
        Ok(Self {
            http,
            base,
            token,
            timeout,
        })
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
    pub async fn get(&self, path: &str) -> Result<Value> {
        self.request(
            Method::GET,
            path,
            None,
            &[],
            true,
            Instant::now() + self.timeout,
        )
        .await
    }
    pub async fn write(&self, method: Method, path: &str, body: Value) -> Result<Value> {
        self.request(
            method,
            path,
            Some(body),
            &[],
            false,
            Instant::now() + self.timeout,
        )
        .await
    }
    pub async fn query(&self, path: &str, body: Value, all: bool) -> Result<Value> {
        self.list(Method::POST, path, Some(body), &[], all).await
    }
    pub async fn list_get(&self, path: &str, query: &[(&str, String)], all: bool) -> Result<Value> {
        self.list(Method::GET, path, None, query, all).await
    }

    async fn list(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        query: &[(&str, String)],
        all: bool,
    ) -> Result<Value> {
        let deadline = Instant::now() + self.timeout;
        let mut payload = body;
        let mut params = query.to_vec();
        let mut result: Option<Value> = None;
        let mut cursors = HashSet::new();
        loop {
            let page = match self
                .request(
                    method.clone(),
                    path,
                    payload.clone(),
                    &params,
                    true,
                    deadline,
                )
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    return Err(if let Some(value) = result {
                        Error::new(
                            8,
                            format!("Получена только часть списка: {}", error.message),
                        )
                        .partial(value)
                    } else {
                        error
                    });
                }
            };
            if let Some(value) = result.as_mut() {
                append_list_page(value, page)?;
            } else {
                result = Some(page);
            }
            let value = result.as_ref().unwrap();
            check_list(value)?;
            if !all || value["has_more"] == false {
                return Ok(result.unwrap());
            }
            let cursor = value["next_cursor"].as_str().unwrap().to_owned();
            if !cursors.insert(cursor.clone()) {
                return Err(Error::new(8, "API повторил курсор").partial(result.unwrap()));
            }
            if let Some(body) = payload.as_mut() {
                body["start_cursor"] = json!(cursor);
            } else {
                params.retain(|(key, _)| *key != "start_cursor");
                params.push(("start_cursor", cursor));
            }
        }
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        query: &[(&str, String)],
        read: bool,
        deadline: Instant,
    ) -> Result<Value> {
        if path.starts_with('/') || path.contains("..") || path.contains("://") {
            return Err(Error::input("Некорректный путь API"));
        }
        let url = self
            .base
            .join(path)
            .map_err(|_| Error::input("Некорректный путь API"))?;
        if url.origin() != self.base.origin() {
            return Err(Error::input("Путь API меняет origin"));
        }
        let mut attempts = 0;
        loop {
            attempts += 1;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::new(if read { 6 } else { 8 }, "Истёк срок запроса"));
            }
            let request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(&self.token)
                .header("Notion-Version", API_VERSION)
                .query(query);
            let request = if let Some(body) = &body {
                request.json(body)
            } else {
                request
            };
            let response = timeout(remaining, request.send()).await;
            let response = match response {
                Ok(Ok(value)) => value,
                _ if read && attempts < 4 => {
                    self.delay(Duration::from_millis(100 * attempts), deadline)
                        .await?;
                    continue;
                }
                _ => {
                    return Err(Error::new(
                        if read { 6 } else { 8 },
                        if read {
                            "Сеть недоступна или срок запроса истёк"
                        } else {
                            "Результат записи неизвестен; проверьте операцию перед повтором"
                        },
                    ));
                }
            };
            let status = response.status().as_u16();
            if read && matches!(status, 429 | 500..=599) && attempts < 4 {
                let delay = response
                    .headers()
                    .get("Retry-After")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_millis(100 * attempts));
                self.delay(delay, deadline).await?;
                continue;
            }
            if !(200..300).contains(&status) {
                let (code, msg) = match status {
                    401 => (3, "Нет доступа: проверьте вход"),
                    403 => (4, "Ресурс недоступен для интеграции"),
                    404 => (4, "Ресурс недоступен"),
                    409 => (5, "Конфликт записи"),
                    400 | 422 => (2, "API отклонил параметры запроса"),
                    429 => (6, "Лимит запросов API"),
                    500..=599 if !read => (8, "Результат записи неизвестен; повтор запрещён"),
                    500..=599 => (6, "Сервис Notion временно недоступен"),
                    _ => (1, "API отклонил запрос"),
                };
                return Err(Error::new(code, msg));
            }
            if status == 204 {
                return Ok(Value::Null);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            return match timeout(remaining, response.json::<Value>()).await {
                Ok(Ok(value)) if !read && status == 202 => Err(Error::new(
                    8,
                    "Сервер принял задачу; результат записи требует проверки",
                )
                .partial(value)),
                Ok(Ok(value)) => Ok(value),
                _ => Err(Error::new(
                    if read { 6 } else { 8 },
                    "Ответ не получен полностью или содержит некорректный JSON",
                )),
            };
        }
    }
    async fn delay(&self, delay: Duration, deadline: Instant) -> Result<()> {
        if delay >= deadline.saturating_duration_since(Instant::now()) {
            return Err(Error::new(
                6,
                "Retry-After превышает оставшийся срок запроса",
            ));
        }
        sleep(delay).await;
        Ok(())
    }
}

pub(crate) fn check_list(value: &Value) -> Result<()> {
    if !value["results"].is_array() || !value["has_more"].is_boolean() {
        return Err(Error::new(1, "Некорректный ответ списка"));
    }
    if value["request_status"]["type"] == "incomplete" {
        return Err(Error::new(8, "API вернул неполный набор записей").partial(value.clone()));
    }
    if value["has_more"] == true
        && value["next_cursor"]
            .as_str()
            .is_none_or(|cursor| cursor.is_empty())
    {
        return Err(Error::new(8, "Отсутствует курсор неполного списка").partial(value.clone()));
    }
    Ok(())
}

pub(crate) fn append_list_page(value: &mut Value, mut page: Value) -> Result<()> {
    if !value["results"].is_array() || !page["results"].is_array() || !page["has_more"].is_boolean()
    {
        return Err(Error::new(8, "Некорректная следующая страница списка").partial(value.clone()));
    }
    value["results"]
        .as_array_mut()
        .unwrap()
        .append(page["results"].as_array_mut().unwrap());
    value["has_more"] = page["has_more"].clone();
    value["next_cursor"] = page["next_cursor"].clone();
    if let Some(status) = page.get("request_status") {
        value["request_status"] = status.clone();
    }
    Ok(())
}
