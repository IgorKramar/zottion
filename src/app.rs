use crate::{
    Error, Result,
    content::{self, BlockId, DataSourceId, DatabaseId, PageId, ViewId},
    notion::{Client, append_list_page, check_list},
    store::{Draft, Operation, OperationState, Store},
};
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Clone)]
pub struct App {
    pub store: Store,
    pub client: Option<Client>,
    pub offline: bool,
}
impl App {
    pub fn new(store: Store, client: Option<Client>, offline: bool) -> Self {
        Self {
            store,
            client,
            offline,
        }
    }
    fn api(&self) -> Result<&Client> {
        if self.offline {
            return Err(Error::new(6, "Сеть отключена флагом --offline"));
        }
        self.client
            .as_ref()
            .ok_or_else(|| Error::new(3, "Выполните auth login или задайте NOTION_TOKEN"))
    }
    pub async fn resource(&self, path: &str) -> Result<Value> {
        if self.offline {
            return self
                .store
                .snapshot("resource", path)
                .await?
                .ok_or_else(|| Error::new(4, "Ресурс ещё не загружен в этом профиле"));
        }
        match self.api()?.get(path).await {
            Ok(value) => {
                self.store
                    .put_snapshot("resource", path, value.clone(), true)
                    .await?;
                Ok(value)
            }
            Err(error) => {
                if error.code == 4 {
                    self.store.remove_snapshot("resource", path).await?;
                }
                Err(error)
            }
        }
    }
    pub async fn page(&self, id: PageId) -> Result<Value> {
        let page = self.resource(&format!("pages/{id}")).await?;
        self.remember("recent", &id.to_string(), &content::page_title(&page))
            .await?;
        Ok(page)
    }
    pub async fn block(&self, id: BlockId) -> Result<Value> {
        self.resource(&format!("blocks/{id}")).await
    }
    pub async fn data_source(&self, id: DataSourceId) -> Result<Value> {
        self.resource(&format!("data_sources/{id}")).await
    }
    pub async fn list(
        &self,
        path: &str,
        body: Option<Value>,
        query: &[(&str, String)],
        all: bool,
    ) -> Result<Value> {
        let key = json!([path, body, query]).to_string();
        if self.offline {
            let value = self
                .store
                .snapshot("list", &key)
                .await?
                .ok_or_else(|| Error::new(4, "Этот запрос ещё не загружен"))?;
            if all && self.store.snapshot_complete("list", &key).await? != Some(true) {
                return Err(Error::new(8, "В кэше только часть запрошенного списка").partial(value));
            }
            return Ok(value);
        }
        let result = if let Some(body) = body {
            self.api()?.query(path, body, all).await
        } else {
            self.api()?.list_get(path, query, all).await
        };
        let value = result?;
        let complete =
            value["has_more"] == false && value["request_status"]["type"] != "incomplete";
        let mut snapshots = vec![("list".to_owned(), key, value.clone(), complete)];
        for item in value["results"].as_array().into_iter().flatten() {
            let kind = match item["object"].as_str() {
                Some("block") => "blocks",
                Some("page") => "pages",
                Some("data_source") => "data_sources",
                _ => continue,
            };
            if let Some(id) = item["id"].as_str() {
                snapshots.push((
                    "resource".to_owned(),
                    format!("{kind}/{id}"),
                    item.clone(),
                    true,
                ));
            }
        }
        self.store.put_snapshots(snapshots).await?;
        Ok(value)
    }
    pub async fn children(&self, id: BlockId, all: bool, cursor: Option<String>) -> Result<Value> {
        let query = cursor
            .map(|cursor| vec![("start_cursor", cursor)])
            .unwrap_or_default();
        self.list(&format!("blocks/{id}/children"), None, &query, all)
            .await
    }
    pub async fn query(&self, id: DataSourceId, body: Value, all: bool) -> Result<Value> {
        self.list(&format!("data_sources/{id}/query"), Some(body), &[], all)
            .await
    }
    pub async fn search(
        &self,
        query: &str,
        local: bool,
        all: bool,
        cursor: Option<String>,
    ) -> Result<Value> {
        if local {
            return self.store.search(query).await;
        }
        let mut body = json!({"query":query});
        if let Some(cursor) = cursor {
            body["start_cursor"] = json!(cursor);
        }
        self.list("search", Some(body), &[], all).await
    }
    pub async fn views(&self, id: DatabaseId, all: bool, cursor: Option<String>) -> Result<Value> {
        let mut query = vec![("database_id", id.to_string())];
        if let Some(cursor) = cursor {
            query.push(("start_cursor", cursor));
        }
        self.list("views", None, &query, all).await
    }
    pub async fn query_view(&self, id: ViewId, all: bool) -> Result<Value> {
        let cache_key = format!("view_query/{id}");
        if self.offline {
            let value = self
                .store
                .snapshot("list", &cache_key)
                .await?
                .ok_or_else(|| Error::new(4, "Представление ещё не загружено"))?;
            if all && value["has_more"] != false {
                return Err(Error::new(8, "В кэше неполное представление").partial(value));
            }
            return Ok(value);
        }
        // A view query creates a short-lived server cache, never document content.
        // Do not retry its POST; subsequent pages use its original query ID.
        let client = self.api()?;
        let deadline = tokio::time::Instant::now() + client.timeout();
        let mut value = client
            .write(
                Method::POST,
                &format!("views/{id}/queries"),
                json!({"page_size":100}),
            )
            .await?;
        let query_id = value["id"]
            .as_str()
            .ok_or_else(|| Error::new(8, "API не вернул ID запроса представления"))?
            .parse::<uuid::Uuid>()
            .map_err(|_| Error::new(8, "Некорректный ID запроса представления"))?;
        let mut cursors = HashSet::new();
        loop {
            check_list(&value)?;
            if !all || value["has_more"] == false {
                break;
            }
            let cursor = value["next_cursor"].as_str().unwrap().to_owned();
            if !cursors.insert(cursor.clone()) {
                return Err(Error::new(8, "API повторил курсор").partial(value));
            }
            let page = tokio::time::timeout_at(
                deadline,
                client.list_get(
                    &format!("views/{id}/queries/{query_id}"),
                    &[("start_cursor", cursor)],
                    false,
                ),
            )
            .await;
            match page {
                Ok(Ok(page)) => append_list_page(&mut value, page)?,
                Ok(Err(error)) => {
                    if let Some(page) = error.partial {
                        append_list_page(&mut value, page)?;
                    }
                    return Err(Error::new(8, error.message).partial(value));
                }
                Err(_) => {
                    return Err(Error::new(8, "Истёк срок чтения представления").partial(value));
                }
            }
        }
        self.store
            .put_snapshot(
                "list",
                &cache_key,
                value.clone(),
                value["has_more"] == false,
            )
            .await?;
        Ok(value)
    }
    pub async fn navigation(&self, kind: &str) -> Result<Value> {
        Ok(self
            .store
            .snapshot("navigation", kind)
            .await?
            .unwrap_or_else(|| json!([])))
    }
    pub async fn remember(&self, kind: &str, id: &str, title: &str) -> Result<Value> {
        let mut entries = self
            .navigation(kind)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default();
        entries.retain(|entry| entry["id"] != id);
        entries.insert(0, json!({"id":id,"title":title}));
        if kind == "recent" {
            entries.truncate(50);
        }
        let value = json!(entries);
        self.store
            .put_snapshot("navigation", kind, value.clone(), true)
            .await?;
        Ok(value)
    }
    pub async fn forget(&self, kind: &str, id: &str) -> Result<Value> {
        let mut entries = self
            .navigation(kind)
            .await?
            .as_array()
            .cloned()
            .unwrap_or_default();
        entries.retain(|entry| entry["id"] != id);
        let value = json!(entries);
        self.store
            .put_snapshot("navigation", kind, value.clone(), true)
            .await?;
        Ok(value)
    }

    /// Persist editor text even if a rich-text boundary prevents publishing it.
    pub async fn stage_block(&self, block: Value, text: String) -> Result<Draft> {
        if !content::TEXT_BLOCKS.contains(&content::block_type(&block)) {
            return Err(Error::unsupported(
                "Этот блок защищён от текстового редактирования",
            ));
        }
        let id = block["id"]
            .as_str()
            .ok_or_else(|| Error::input("У блока нет ID"))?
            .parse::<BlockId>()?;
        let path = format!("blocks/{id}");
        let base = if let Some(draft) = self.store.draft(&path).await? {
            if draft.base["kind"] != "block_text" {
                return Err(Error::new(
                    5,
                    "Сначала отправьте или явно удалите существующий черновик оформления/свойств блока",
                ));
            }
            draft.base
        } else {
            json!({"kind":"block_text","method":"PATCH","path":path,"read_path":path,"resource":block})
        };
        let local = json!({"text":text});
        self.store
            .put_draft(&path, base.clone(), local.clone())
            .await?;
        Ok(Draft {
            id: path,
            base,
            local,
        })
    }
    pub async fn save_block(&self, block: Value, text: String) -> Result<Value> {
        let draft = self.stage_block(block, text).await?;
        self.apply_draft(&draft.id).await
    }
    pub async fn stage(
        &self,
        method: &str,
        path: &str,
        read_path: Option<&str>,
        base: Value,
        payload: Value,
    ) -> Result<String> {
        let id = if method == "POST" || path.ends_with("/children") {
            format!("{path}/new-{}", uuid::Uuid::new_v4())
        } else {
            path.to_owned()
        };
        if let Some(existing) = self.store.draft(&id).await? {
            if existing.base["kind"] == "patch" && existing.local == payload {
                return Ok(id);
            }
            return Err(Error::new(
                5,
                "У ресурса уже есть другой локальный черновик; используйте draft show/apply/drop",
            ));
        }
        let context = json!({"kind":"patch","method":method,"path":path,"read_path":read_path,"resource":base});
        self.store.put_draft(&id, context, payload).await?;
        Ok(id)
    }
    pub async fn mutate(
        &self,
        method: &str,
        path: &str,
        read_path: Option<&str>,
        base: Value,
        payload: Value,
    ) -> Result<Value> {
        let id = self.stage(method, path, read_path, base, payload).await?;
        self.apply_draft(&id).await
    }
    pub async fn draft_versions(&self, id: &str) -> Result<Value> {
        let draft = self
            .store
            .draft(id)
            .await?
            .ok_or_else(|| Error::new(4, "Черновик не найден"))?;
        let remote = if let Some(path) = draft.base["read_path"].as_str() {
            let mut value = self.resource(path).await?;
            self.hydrate_properties(&mut value, &draft.local).await?;
            Some(value)
        } else {
            None
        };
        Ok(json!({"id":id,"base":draft.base,"local":draft.local,"remote":remote}))
    }
    pub async fn resolve_draft(&self, id: &str, choice: &str) -> Result<Value> {
        let mut draft = self
            .store
            .draft(id)
            .await?
            .ok_or_else(|| Error::new(4, "Черновик не найден"))?;
        match choice {
            "remote" => {
                self.store.delete_draft(id).await?;
                Ok(json!({"resolution":"remote","draft_removed":true}))
            }
            "local" => {
                if let Some(path) = draft.base["read_path"].as_str() {
                    let mut remote = self.resource(path).await?;
                    self.hydrate_properties(&mut remote, &draft.local).await?;
                    draft.base["resource"] = remote;
                }
                self.store.put_draft(id, draft.base, draft.local).await?;
                self.apply_draft(id).await
            }
            _ => Err(Error::input(
                "Выберите local или remote после просмотра draft show",
            )),
        }
    }
    pub async fn apply_draft(&self, id: &str) -> Result<Value> {
        let _guard = if self.offline {
            None
        } else {
            Some(self.store.acquire_write().await?)
        };
        let draft = self
            .store
            .draft(id)
            .await?
            .ok_or_else(|| Error::new(4, "Черновик не найден"))?;
        if draft.base["editor_invalid"]
            .as_object()
            .is_some_and(|fields| fields.values().any(|value| value == true))
        {
            return Err(Error::input(
                "В черновике есть незавершённые значения свойств",
            ));
        }
        if self.offline {
            return Ok(json!({"draft_id":id,"saved_locally":true,"published":false}));
        }
        let path = draft.base["path"]
            .as_str()
            .ok_or_else(|| Error::input("Некорректный план черновика"))?;
        let method = draft.base["method"]
            .as_str()
            .ok_or_else(|| Error::input("Некорректный метод черновика"))?;
        let operations = self.store.operations().await?;
        for op in operations
            .iter()
            .filter(|op| op.state == OperationState::Confirmed && op.base["draft_id"] == id)
        {
            if draft_matches(op, &draft) {
                self.store
                    .confirm_operation(&op.id, op.response.clone(), Some(draft.clone()))
                    .await?;
                return Ok(op
                    .response
                    .clone()
                    .unwrap_or_else(|| json!({"operation_id":op.id,"confirmed":true})));
            }
            if method == "POST" || path.ends_with("/children") {
                return Err(Error::new(
                    5,
                    "Создание уже подтверждено; новый черновик сохранён, проверьте его через draft show",
                ));
            }
        }
        let payload = draft_payload(&draft)?;
        if path.starts_with("pages/") && method == "PATCH" {
            validate_page_patch(&payload)?;
            if let Some(properties) = payload.get("properties") {
                self.validate_properties(&draft.base["resource"], properties)
                    .await?;
            }
        } else if path.starts_with("blocks/") && !path.ends_with("/children") {
            content::block_patch(&draft.base["resource"], payload.clone())?;
        }
        if let Some(pending) = operations.into_iter().find(|op| {
            op.path == path
                && matches!(
                    op.state,
                    OperationState::Unknown | OperationState::Sending | OperationState::Waiting
                )
        }) {
            return Err(Error::new(
                8,
                format!(
                    "Сначала проверьте исход операции {} через operation show/check",
                    pending.id
                ),
            ));
        }
        let mut context = draft.base.clone();
        context["draft_id"] = json!(id);
        context["draft"] = serde_json::to_value(&draft)?;
        let operation = self
            .store
            .prepare(method, path, context, payload.clone())
            .await?;
        if let Some(read_path) = draft.base["read_path"].as_str() {
            let mut remote = match self.api()?.get(read_path).await {
                Ok(value) => value,
                Err(error) => {
                    self.store
                        .set_operation(
                            &operation.id,
                            OperationState::Failed,
                            Some(json!({"error":error})),
                        )
                        .await?;
                    return Err(error);
                }
            };
            self.hydrate_properties(&mut remote, &payload).await?;
            if !unchanged(&draft.base["resource"], &remote, &payload) {
                let versions = json!({"draft_id":id,"operation_id":operation.id,"base":draft.base["resource"],"local":payload,"remote":remote});
                self.store
                    .set_operation(
                        &operation.id,
                        OperationState::Conflict,
                        Some(versions.clone()),
                    )
                    .await?;
                return Err(Error::new(
                    5,
                    "Цель изменена в Notion; сохранены базовая, локальная и удалённая версии",
                )
                .partial(versions));
            }
        }
        self.store
            .set_operation(&operation.id, OperationState::Sending, None)
            .await?;
        let method = Method::from_bytes(method.as_bytes())
            .map_err(|_| Error::input("Некорректный метод записи"))?;
        match self.api()?.write(method, path, payload).await {
            Ok(value) => {
                if value["object"] == "task" || value["object"] == "async_task" {
                    self.store
                        .set_operation(&operation.id, OperationState::Waiting, Some(value.clone()))
                        .await?;
                    return Err(Error::new(
                        8,
                        format!(
                            "Серверная задача принята; проверьте operation {}",
                            operation.id
                        ),
                    )
                    .partial(value));
                }
                let expected = if path.ends_with("/children") {
                    value["object"] == "list" && value["results"].is_array()
                } else if path.starts_with("blocks/") {
                    value["object"] == "block"
                        && value["id"]
                            .as_str()
                            .is_some_and(|v| v.parse::<BlockId>().is_ok())
                } else {
                    value["object"] == "page"
                        && value["id"]
                            .as_str()
                            .is_some_and(|v| v.parse::<PageId>().is_ok())
                };
                if !expected {
                    self.store
                        .set_operation(&operation.id, OperationState::Unknown, Some(value.clone()))
                        .await?;
                    return Err(Error::new(
                        8,
                        format!(
                            "Ответ не подтверждает запись; проверьте операцию {}",
                            operation.id
                        ),
                    )
                    .partial(value));
                }
                self.store
                    .confirm_operation(&operation.id, Some(value.clone()), Some(draft.clone()))
                    .await.map_err(|_|Error::new(8,format!("Ответ Notion получен, но журнал не обновлён; проверьте операцию {}",operation.id)).partial(value.clone()))?;
                if let Some(resource_id) = value["id"].as_str() {
                    let kind = match value["object"].as_str() {
                        Some("block") => Some("blocks"),
                        Some("page") => Some("pages"),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        let _ = self
                            .store
                            .put_snapshot(
                                "resource",
                                &format!("{kind}/{resource_id}"),
                                value.clone(),
                                true,
                            )
                            .await;
                    }
                }
                Ok(value)
            }
            Err(mut error) => {
                let state = if error.code == 8
                    && error.partial.as_ref().is_some_and(|value| {
                        matches!(value["object"].as_str(), Some("task" | "async_task"))
                    }) {
                    OperationState::Waiting
                } else if error.code == 8 {
                    OperationState::Unknown
                } else if error.code == 5 {
                    OperationState::Conflict
                } else {
                    OperationState::Failed
                };
                self.store
                    .set_operation(&operation.id, state, Some(json!({"error":error})))
                    .await
                    .map_err(|_| {
                        Error::new(
                            8,
                            format!(
                                "Не удалось сохранить исход записи; проверьте операцию {}",
                                operation.id
                            ),
                        )
                    })?;
                error.message = format!("{}; операция {}", error.message, operation.id);
                Err(error)
            }
        }
    }
    pub async fn check_operation(&self, id: &str) -> Result<Value> {
        let _guard = self.store.acquire_write().await?;
        let op = self.store.operation(id).await?;
        let Some(path) = op.base["read_path"].as_str() else {
            return Ok(
                json!({"operation":op,"requires_manual_verification":true,"reason":"Для создания проверьте родителя в Notion; автоматический повтор запрещён"}),
            );
        };
        let mut remote = self.api()?.get(path).await?;
        self.hydrate_properties(&mut remote, &op.local).await?;
        let matches = matches_payload(&remote, &op.local);
        if matches && matches!(op.state, OperationState::Unknown | OperationState::Waiting) {
            let draft = self.operation_draft(&op).await?;
            self.store
                .confirm_operation(id, Some(remote.clone()), draft)
                .await?;
        }
        Ok(
            json!({"operation":self.store.operation(id).await?,"remote":remote,"matches_local":matches,"requires_manual_verification":!matches}),
        )
    }
    pub async fn acknowledge_operation(&self, id: &str, confirmed: bool) -> Result<Value> {
        let _guard = self.store.acquire_write().await?;
        let op = self.store.operation(id).await?;
        if !matches!(op.state, OperationState::Unknown | OperationState::Waiting) {
            return Err(Error::input(
                "Ручная сверка нужна только для unknown/waiting",
            ));
        }
        let response = Some(json!({"manually_verified":true}));
        if confirmed {
            let draft = self.operation_draft(&op).await?;
            self.store.confirm_operation(id, response, draft).await?;
        } else {
            self.store
                .set_operation(id, OperationState::Failed, response)
                .await?;
        }
        Ok(serde_json::to_value(self.store.operation(id).await?)?)
    }
    async fn operation_draft(&self, op: &Operation) -> Result<Option<Draft>> {
        let Some(id) = op.base["draft_id"].as_str() else {
            return Ok(None);
        };
        Ok(self
            .store
            .draft(id)
            .await?
            .filter(|draft| draft_matches(op, draft)))
    }
    pub async fn append(&self, id: BlockId, blocks: Vec<Value>) -> Result<Value> {
        if blocks.is_empty() || blocks.len() > 100 {
            return Err(Error::input("За один запрос допустимо 1–100 блоков"));
        }
        for block in &blocks {
            validate_new_block(block)?;
        }
        let path = format!("blocks/{id}/children");
        let base = self.block(id).await?;
        self.mutate(
            "PATCH",
            &path,
            Some(&format!("blocks/{id}")),
            base,
            json!({"children":blocks}),
        )
        .await
    }
    pub async fn patch_block(&self, id: BlockId, patch: Value) -> Result<Value> {
        let base = self.block(id).await?;
        let patch = content::block_patch(&base, patch)?;
        let path = format!("blocks/{id}");
        self.mutate("PATCH", &path, Some(&path), base, patch).await
    }
    pub async fn patch_page(&self, id: PageId, patch: Value) -> Result<Value> {
        let mut base = self.page(id).await?;
        self.hydrate_properties(&mut base, &patch).await?;
        validate_page_patch(&patch)?;
        if let Some(properties) = patch.get("properties") {
            self.validate_properties(&base, properties).await?;
        }
        let path = format!("pages/{id}");
        self.mutate("PATCH", &path, Some(&path), base, patch).await
    }
    pub async fn patch_page_from(&self, base: Value, patch: Value) -> Result<Value> {
        validate_page_patch(&patch)?;
        if let Some(properties) = patch.get("properties") {
            self.validate_properties(&base, properties).await?;
        }
        let id = base["id"]
            .as_str()
            .ok_or_else(|| Error::input("У страницы нет ID"))?
            .parse::<PageId>()?;
        let path = format!("pages/{id}");
        self.mutate("PATCH", &path, Some(&path), base, patch).await
    }
    pub async fn move_page(&self, id: PageId, parent: PageId) -> Result<Value> {
        let base = self.page(id).await?;
        self.mutate(
            "POST",
            &format!("pages/{id}/move"),
            Some(&format!("pages/{id}")),
            base,
            json!({"parent":{"type":"page_id","page_id":parent.to_string()}}),
        )
        .await
    }
    pub async fn create_page(
        &self,
        parent: PageId,
        title: &str,
        text: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({"parent":{"page_id":parent.to_string()},"properties":{"title":{"title":text_runs(title)}}});
        if let Some(text) = text {
            body["children"] = json!(
                text.split("\n\n")
                    .map(|text| content::new_block("paragraph", text))
                    .collect::<Result<Vec<_>>>()?
            );
        }
        self.mutate("POST", "pages", None, Value::Null, body).await
    }
    pub async fn create_task(
        &self,
        source: DataSourceId,
        title: &str,
        properties: Value,
    ) -> Result<Value> {
        let schema = self.data_source(source).await?;
        let title_key = schema["properties"]
            .as_object()
            .and_then(|props| {
                props
                    .iter()
                    .find(|(_, prop)| prop["type"] == "title")
                    .map(|(name, _)| name.clone())
            })
            .ok_or_else(|| Error::input("В data source отсутствует title"))?;
        let mut props = properties
            .as_object()
            .cloned()
            .ok_or_else(|| Error::input("Свойства должны быть объектом"))?;
        props.insert(title_key, json!({"title":text_runs(title)}));
        validate_against_schema(&schema, &json!(props))?;
        self.mutate("POST","pages",None,Value::Null,json!({"parent":{"type":"data_source_id","data_source_id":source.to_string()},"properties":props})).await
    }
    async fn validate_properties(&self, page: &Value, properties: &Value) -> Result<()> {
        if let Some(id) = page["parent"]["data_source_id"].as_str() {
            let schema = self.data_source(id.parse()?).await?;
            validate_against_schema(&schema, properties)
        } else {
            let props = properties
                .as_object()
                .ok_or_else(|| Error::input("Свойства должны быть объектом"))?;
            for (name, value) in props {
                if name != "title" && page["properties"][name]["type"] != "title" {
                    return Err(Error::unsupported(
                        "Обычная страница поддерживает только title",
                    ));
                }
                validate_property("title", value)?;
            }
            Ok(())
        }
    }
    async fn hydrate_properties(&self, page: &mut Value, payload: &Value) -> Result<()> {
        let Some(properties) = payload["properties"].as_object() else {
            return Ok(());
        };
        let Some(page_id) = page["id"].as_str().and_then(|id| id.parse::<PageId>().ok()) else {
            return Ok(());
        };
        for key in properties.keys() {
            let property = property_at(page, key);
            let kind = property["type"].as_str().unwrap_or("");
            if !matches!(kind, "title" | "rich_text" | "people" | "relation") {
                continue;
            }
            let id = property["id"].as_str().unwrap_or(key).to_owned();
            let kind = kind.to_owned();
            let response = self.property(page_id, &id, true, None).await?;
            let full = if response["object"] == "list" {
                json!(
                    response["results"]
                        .as_array()
                        .ok_or_else(|| Error::new(8, "Неполное значение свойства"))?
                        .iter()
                        .map(|item| item[&kind].clone())
                        .collect::<Vec<_>>()
                )
            } else {
                response[&kind].clone()
            };
            let name = page["properties"]
                .as_object()
                .and_then(|p| {
                    p.iter()
                        .find(|(name, value)| name.as_str() == key || value["id"] == *key)
                        .map(|(name, _)| name.clone())
                })
                .unwrap_or_else(|| key.clone());
            page["properties"][&name][&kind] = full;
        }
        Ok(())
    }
    pub async fn property(
        &self,
        page: PageId,
        property: &str,
        all: bool,
        cursor: Option<String>,
    ) -> Result<Value> {
        if property.is_empty()
            || property
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '/' | '?' | '#' | '\\'))
        {
            return Err(Error::input("Некорректный ID свойства"));
        }
        let path = format!("pages/{page}/properties/{property}");
        if let Some(cursor) = cursor {
            return self
                .list(&path, None, &[("start_cursor", cursor)], all)
                .await;
        }
        let mut value = self.resource(&path).await?;
        if value["object"] != "list" {
            return Ok(value);
        }
        check_list(&value)?;
        if !all {
            return Ok(value);
        }
        if value["has_more"] == true {
            let cursor = value["next_cursor"].as_str().unwrap().to_owned();
            match self
                .list(&path, None, &[("start_cursor", cursor)], true)
                .await
            {
                Ok(rest) => append_list_page(&mut value, rest)?,
                Err(error) => {
                    if let Some(rest) = error.partial {
                        append_list_page(&mut value, rest)?;
                    }
                    return Err(Error::new(8, error.message).partial(value));
                }
            }
        }
        check_list(&value)?;
        self.store
            .put_snapshot("resource", &path, value.clone(), true)
            .await?;
        Ok(value)
    }
}

pub fn text_runs(text: &str) -> Value {
    let chars: Vec<char> = text.chars().collect();
    json!(
        chars
            .chunks(2000)
            .map(|chunk| json!({"type":"text","text":{"content":chunk.iter().collect::<String>()}}))
            .collect::<Vec<_>>()
    )
}
fn draft_payload(draft: &Draft) -> Result<Value> {
    if draft.base["kind"] == "block_text" {
        content::text_patch(
            &draft.base["resource"],
            draft.local["text"]
                .as_str()
                .ok_or_else(|| Error::input("У черновика нет текста"))?,
        )
    } else {
        Ok(draft.local.clone())
    }
}
fn draft_matches(op: &Operation, draft: &Draft) -> bool {
    if let Some(snapshot) = op.base.get("draft") {
        return snapshot == &json!({"id":draft.id,"base":draft.base,"local":draft.local});
    }
    // Совместимость с журналом до сохранения снимка черновика.
    let mut base = op.base.clone();
    if let Some(fields) = base.as_object_mut() {
        fields.remove("draft_id");
    }
    base == draft.base && draft_payload(draft).is_ok_and(|payload| payload == op.local)
}

// Compare only fields this PATCH changes. Append/move still compare the parent's
// last_edited_time because children are absent from the block GET response.
pub fn unchanged(base: &Value, remote: &Value, payload: &Value) -> bool {
    if base.is_null() {
        return true;
    }
    if payload.get("children").is_some() || payload.get("parent").is_some() {
        return base["last_edited_time"] == remote["last_edited_time"]
            && base["id"] == remote["id"];
    }
    payload.as_object().is_some_and(|fields| {
        fields.iter().all(|(key, value)| {
            if key == "properties" {
                value.as_object().is_some_and(|props| {
                    props.keys().all(|name| {
                        property_unchanged(property_at(base, name), property_at(remote, name))
                    })
                })
            } else if let Some(fields) = value.as_object() {
                fields
                    .keys()
                    .all(|field| base[key][field] == remote[key][field])
            } else {
                base[key] == remote[key]
            }
        })
    })
}
pub fn matches_payload(remote: &Value, payload: &Value) -> bool {
    if payload.get("children").is_some() || payload.get("parent").is_some() {
        return false;
    }
    match payload {
        Value::Object(fields) => {
            !fields.is_empty()
                && fields.iter().all(|(key, value)| {
                    if key == "properties" {
                        value.as_object().is_some_and(|props| {
                            props
                                .iter()
                                .all(|(name, value)| subset(property_at(remote, name), value))
                        })
                    } else {
                        subset(&remote[key], value)
                    }
                })
        }
        _ => false,
    }
}
fn subset(remote: &Value, payload: &Value) -> bool {
    match payload {
        Value::Object(fields) => fields
            .iter()
            .all(|(key, value)| subset(&remote[key], value)),
        Value::Array(items) => remote.as_array().is_some_and(|actual| {
            actual.len() == items.len() && actual.iter().zip(items).all(|(a, b)| subset(a, b))
        }),
        _ => remote == payload,
    }
}
pub fn validate_page_patch(patch: &Value) -> Result<()> {
    let obj = patch
        .as_object()
        .ok_or_else(|| Error::input("Ожидается объект PATCH"))?;
    if obj.is_empty()
        || obj
            .keys()
            .any(|k| !matches!(k.as_str(), "properties" | "in_trash"))
    {
        return Err(Error::input("Разрешены properties и in_trash"));
    }
    if obj.get("in_trash").is_some_and(|flag| !flag.is_boolean()) {
        return Err(Error::input("in_trash должен быть boolean"));
    }
    Ok(())
}
pub fn validate_against_schema(schema: &Value, properties: &Value) -> Result<()> {
    let schema = schema["properties"]
        .as_object()
        .ok_or_else(|| Error::input("Некорректная схема data source"))?;
    for (name, value) in properties
        .as_object()
        .ok_or_else(|| Error::input("Свойства должны быть объектом"))?
    {
        let property = schema
            .get(name)
            .or_else(|| schema.values().find(|p| p["id"] == *name))
            .ok_or_else(|| Error::input(format!("Свойство {name} отсутствует в схеме")))?;
        validate_property(property["type"].as_str().unwrap_or(""), value)?;
        let kind = property["type"].as_str().unwrap_or("");
        if matches!(kind, "status" | "select" | "multi_select") && !value[kind].is_null() {
            let options = property[kind]["options"]
                .as_array()
                .ok_or_else(|| Error::input("Схема не содержит вариантов свойства"))?;
            let selected = if kind == "multi_select" {
                value[kind].as_array().cloned().unwrap_or_default()
            } else {
                vec![value[kind].clone()]
            };
            for item in selected {
                if !options.iter().any(|option| {
                    item["id"].as_str().is_some_and(|id| option["id"] == id)
                        || item["name"]
                            .as_str()
                            .is_some_and(|name| option["name"] == name)
                }) {
                    return Err(Error::unsupported(
                        "Новый вариант изменил бы схему; выберите существующий option",
                    ));
                }
            }
        }
    }
    Ok(())
}
pub fn validate_property(kind: &str, value: &Value) -> Result<()> {
    let fields = value
        .as_object()
        .ok_or_else(|| Error::input("Значение свойства должно быть объектом"))?;
    if fields.len() != 1 || !fields.contains_key(kind) {
        return Err(Error::input(format!("Ожидается свойство типа {kind}")));
    }
    let v = &value[kind];
    let valid = match kind {
        "title" | "rich_text" => v.is_array(),
        "checkbox" => v.is_boolean(),
        "number" => v.is_number() || v.is_null(),
        "url" => v.is_string() || v.is_null(),
        "status" | "select" => v.is_null() || v["name"].is_string() || v["id"].is_string(),
        "multi_select" => v.as_array().is_some_and(|vs| {
            vs.iter()
                .all(|v| v["name"].is_string() || v["id"].is_string())
        }),
        "date" => v.is_null() || v["start"].is_string(),
        "people" | "relation" => v.as_array().is_some_and(|vs| {
            vs.iter().all(|v| {
                v["id"]
                    .as_str()
                    .is_some_and(|id| id.parse::<uuid::Uuid>().is_ok())
            })
        }),
        "formula" | "rollup" | "created_time" | "last_edited_time" | "created_by"
        | "last_edited_by" | "unique_id" => {
            return Err(Error::unsupported(
                "Вычисляемое свойство доступно только для чтения",
            ));
        }
        _ => {
            return Err(Error::unsupported(format!(
                "Свойство типа {kind} не поддерживается"
            )));
        }
    };
    if !valid {
        return Err(Error::input(format!("Некорректное значение {kind}")));
    }
    Ok(())
}
fn validate_new_block(block: &Value) -> Result<()> {
    let kind = content::block_type(block);
    if !content::TEXT_BLOCKS.contains(&kind) && kind != "divider" {
        return Err(Error::unsupported(
            "Создание этого типа блока не поддерживается",
        ));
    }
    if block.get("id").is_some() {
        return Err(Error::input("Новый блок не должен содержать ID"));
    }
    let mut fields = block[kind].clone();
    if let Some(fields) = fields.as_object_mut() {
        fields.remove("children");
    }
    content::block_patch(block, json!({kind:fields}))?;
    if let Some(children) = block[kind].get("children") {
        for child in children
            .as_array()
            .ok_or_else(|| Error::input("children должны быть массивом"))?
        {
            validate_new_block(child)?;
        }
    }
    Ok(())
}

fn property_at<'a>(page: &'a Value, name: &str) -> &'a Value {
    if let Some(value) = page["properties"].get(name) {
        return value;
    }
    page["properties"]
        .as_object()
        .and_then(|props| props.values().find(|v| v["id"] == name))
        .unwrap_or(&Value::Null)
}

fn property_unchanged(base: &Value, remote: &Value) -> bool {
    let kind = base["type"].as_str().unwrap_or("");
    if remote["type"] != kind {
        return false;
    }
    if matches!(kind, "people" | "relation") {
        let ids = |value: &Value| {
            value[kind].as_array().map(|items| {
                items
                    .iter()
                    .map(|item| item["id"].clone())
                    .collect::<Vec<_>>()
            })
        };
        return ids(base) == ids(remote);
    }
    base[kind] == remote[kind]
}
