use crate::{
    Error, Result,
    app::{App, validate_against_schema, validate_property},
    content::{self, BlockId, DataSourceId, PageId, ViewId},
};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
};
use edtui::{EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Lines};
use futures_util::StreamExt;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
        Wrap,
    },
};
use serde_json::{Value, json};
use std::{collections::HashSet, future::Future, io, time::Duration};
use tokio::sync::mpsc;

const HELP: &str = "Страницы и поиск\n  Ctrl+P или /   найти страницу, вставить ссылку, выбрать действие\n  Tab / Ctrl+W   переключить дерево и документ\n  Enter          открыть страницу или редактор\n  h/l            свернуть / загрузить вложенность\n\nЧтение и редактирование\n  j/k, ↑↓        выбрать блок или запись\n  PgUp/PgDn      прокрутить документ; Ctrl+U/D — полэкрана\n  i              писать в выбранном блоке\n  Esc            Insert → Normal → просмотр\n  Пробел         отметить задачу локально\n\nСохранение\n  Ctrl+S / :w    отправить черновики текущего экрана\n  Ctrl+Q / :q    выйти; неотправленные черновики сохраняются\n  Ctrl+R         обновить страницу\n  F2             показать свойства\n\nТаблица: h/l ячейка, e свойство. Доска: H/L сменить статус.\n: открывает строку команд. Ctrl+P показывает действия.";
const PALETTE: &[(&str, &str)] = &[
    ("Открыть страницу", "open "),
    ("Таблица задач", "table "),
    ("Доска задач", "board "),
    ("Представление Notion", "view "),
    ("Поиск названий", "search "),
    ("Локальный поиск", "local "),
    ("Добавить блок", "append paragraph "),
    ("Новая страница", "new "),
    ("Новая задача", "task "),
    ("Изменения", "diff"),
    ("Отправить", "w"),
    ("Справка", "help"),
];

#[derive(Clone)]
struct Node {
    raw: Value,
    depth: usize,
    expanded: bool,
    loaded: bool,
}
#[derive(Clone)]
struct NavItem {
    id: Option<PageId>,
    source: Option<DataSourceId>,
    title: String,
    depth: usize,
    expanded: bool,
    loaded: bool,
}
#[derive(Clone)]
struct Grid {
    source: DataSourceId,
    schema: Value,
    rows: Vec<Value>,
    columns: Vec<String>,
    row: usize,
    column: usize,
    board: bool,
    group: String,
    by_group: bool,
    filter: Value,
    sorts: Value,
    view: Option<ViewId>,
    name: String,
}
#[derive(Clone)]
struct BoardColumn {
    name: String,
    ids: Vec<String>,
}
#[derive(Clone)]
enum Target {
    Block(Value),
    Property {
        base: Value,
        name: String,
        kind: String,
    },
}
struct Edit {
    state: EditorState,
    handler: EditorEventHandler,
    target: Target,
    draft_id: String,
    last_saved: String,
}
impl Edit {
    fn text(&self) -> String {
        self.state.lines.to_string()
    }
}
#[derive(Clone)]
enum Confirm {
    Page(Value),
    Block(Value),
    Resolve { id: String, choice: String },
}
enum Overlay {
    Help,
    Palette {
        selected: usize,
        query: String,
    },
    Text {
        title: String,
        text: String,
        scroll: u16,
    },
    Versions {
        id: String,
        value: Value,
        scroll: u16,
    },
    Confirm(Confirm),
}
enum Payload {
    Document {
        page: Value,
        blocks: Vec<Value>,
    },
    Children {
        id: String,
        blocks: Vec<Value>,
    },
    NavChildren {
        id: PageId,
        index: usize,
        items: Vec<NavItem>,
    },
    Grid(Grid),
    Property {
        target: Target,
        text: String,
        draft_id: String,
    },
    Search(Vec<NavItem>),
    Info {
        title: String,
        value: Value,
    },
    Saved {
        results: Vec<(String, Value)>,
        quit: bool,
        offline: bool,
        error: Option<Error>,
    },
    Mutation {
        value: Value,
        generation: u64,
    },
}
struct Message {
    generation: Option<u64>,
    write: bool,
    result: Result<Payload>,
}
struct Ui {
    generation: u64,
    page: Option<Value>,
    nodes: Vec<Node>,
    selected: usize,
    nav: Vec<NavItem>,
    nav_selected: usize,
    nav_focus: bool,
    nav_visible: bool,
    properties: bool,
    grid: Option<Grid>,
    editor: Option<Edit>,
    command: Option<String>,
    overlay: Option<Overlay>,
    status: String,
    pending: usize,
    saving: bool,
    quit_after_save: bool,
    drafts: HashSet<String>,
    local_text: std::collections::HashMap<String, String>,
    local_checks: std::collections::HashMap<String, bool>,
    quit: bool,
    cancelled: bool,
    theme: String,
    width: u16,
    nav_area: Rect,
    content_area: Rect,
    document_scroll: usize,
    document_limit: usize,
    follow_selection: bool,
    document_rows: Vec<(usize, Rect)>,
    nav_state: ListState,
    table_state: TableState,
}
impl Ui {
    fn new(drafts: HashSet<String>) -> Self {
        Self {
            generation: 0,
            page: None,
            nodes: Vec::new(),
            selected: 0,
            nav: Vec::new(),
            nav_selected: 0,
            nav_focus: false,
            nav_visible: true,
            properties: false,
            grid: None,
            editor: None,
            command: None,
            overlay: None,
            status: "Ctrl+P — найти страницу или действие".into(),
            pending: 0,
            saving: false,
            quit_after_save: false,
            drafts,
            local_text: std::collections::HashMap::new(),
            local_checks: std::collections::HashMap::new(),
            quit: false,
            cancelled: false,
            theme: "dark".into(),
            width: 22,
            nav_area: Rect::default(),
            content_area: Rect::default(),
            document_scroll: 0,
            document_limit: 0,
            follow_selection: true,
            document_rows: Vec::new(),
            nav_state: ListState::default(),
            table_state: TableState::default(),
        }
    }
    fn spawn<F>(&mut self, tx: &mpsc::UnboundedSender<Message>, generation: Option<u64>, future: F)
    where
        F: Future<Output = Result<Payload>> + Send + 'static,
    {
        self.pending += 1;
        let tx = tx.clone();
        let write = self.saving;
        tokio::spawn(async move {
            let result = future.await;
            let _ = tx.send(Message {
                generation,
                write,
                result,
            });
        });
    }
    fn open_page(&mut self, app: &App, tx: &mpsc::UnboundedSender<Message>, id: PageId) {
        self.generation += 1;
        let generation = self.generation;
        self.editor = None;
        self.grid = None;
        self.overlay = None;
        self.status = "Загрузка страницы…".into();
        let app = app.clone();
        self.spawn(tx, Some(generation), async move {
            let page = app.page(id).await?;
            let value = app.children(BlockId(id.0), true, None).await?;
            let blocks = array_results(&value)?;
            Ok(Payload::Document { page, blocks })
        });
    }
    fn open_grid(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        source: DataSourceId,
        board: bool,
        group: Option<String>,
        query: (Value, Value),
    ) {
        let (filter, sorts) = query;
        self.generation += 1;
        self.editor = None;
        self.overlay = None;
        let generation = self.generation;
        let app = app.clone();
        self.status = "Загрузка data source…".into();
        self.spawn(tx, Some(generation), async move {
            let schema = app.data_source(source).await?;
            let mut body = json!({});
            if !filter.is_null() {
                body["filter"] = filter.clone();
            }
            if !sorts.is_null() {
                body["sorts"] = sorts.clone();
            }
            let rows = array_results(&app.query(source, body, true).await?)?;
            Ok(Payload::Grid(make_grid(
                source,
                schema,
                rows,
                board,
                group,
                (filter, sorts),
                None,
            )?))
        });
    }
    fn open_view(&mut self, app: &App, tx: &mpsc::UnboundedSender<Message>, id: ViewId) {
        self.generation += 1;
        self.editor = None;
        self.overlay = None;
        let generation = self.generation;
        let app = app.clone();
        self.status = "Загрузка представления Notion…".into();
        self.spawn(tx, Some(generation), async move {
            let view = app.resource(&format!("views/{id}")).await?;
            let kind = view["type"].as_str().unwrap_or("");
            if !matches!(kind, "table" | "board") {
                return Err(Error::unsupported(
                    "В v0.1 представлены table и board; другие виды защищены от изменений",
                ));
            }
            let source: DataSourceId = view["data_source_id"]
                .as_str()
                .ok_or_else(|| Error::unsupported("У представления нет data source"))?
                .parse()?;
            let schema = app.data_source(source).await?;
            let refs = array_results(&app.query_view(id, true).await?)?;
            let mut rows = Vec::with_capacity(refs.len());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            for page in refs {
                if page["properties"].is_object() {
                    rows.push(page);
                } else {
                    let id: PageId = page["id"]
                        .as_str()
                        .ok_or_else(|| Error::new(8, "Нет ID записи представления"))?
                        .parse()?;
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    let row = tokio::time::timeout(remaining, app.resource(&format!("pages/{id}")))
                        .await
                        .map_err(|_| {
                            Error::new(8, "Истёк срок загрузки карточек представления")
                                .partial(json!({"loaded_rows":rows.clone()}))
                        })??;
                    rows.push(row);
                }
            }
            let property_id = view["configuration"]["group_by"]["property_id"]
                .as_str()
                .map(str::to_owned);
            let mut grid = make_grid(
                source,
                schema,
                rows,
                kind == "board",
                property_id,
                (view["filter"].clone(), view["sorts"].clone()),
                Some((
                    id,
                    view["name"].as_str().unwrap_or("Представление").to_owned(),
                )),
            )?;
            grid.by_group = view["configuration"]["group_by"]["group_by"] == "group";
            if let Some(properties) = view["configuration"]["properties"].as_array() {
                let mut columns = Vec::new();
                for property in properties {
                    if property["visible"] == false {
                        continue;
                    }
                    if let Some(id) = property["property_id"].as_str()
                        && let Some(name) = schema_name(&grid.schema, id)
                    {
                        columns.push(name);
                    }
                }
                if !columns.is_empty() {
                    grid.columns = columns;
                }
            }
            Ok(Payload::Grid(grid))
        });
    }
    fn refresh(&mut self, app: &App, tx: &mpsc::UnboundedSender<Message>) {
        if let Some(grid) = self.grid.as_ref() {
            if let Some(view) = grid.view {
                self.open_view(app, tx, view);
            } else {
                self.open_grid(
                    app,
                    tx,
                    grid.source,
                    grid.board,
                    Some(grid.group.clone()),
                    (grid.filter.clone(), grid.sorts.clone()),
                );
            }
        } else if let Some(id) = self
            .page
            .as_ref()
            .and_then(|p| p["id"].as_str())
            .and_then(|id| id.parse().ok())
        {
            self.open_page(app, tx, id);
        }
    }
    fn update_resource(&mut self, value: &Value) {
        if value["object"] == "block" {
            for node in &mut self.nodes {
                if node.raw["id"] == value["id"] {
                    node.raw = value.clone();
                }
            }
            if let Some(edit) = &mut self.editor
                && let Target::Block(base) = &mut edit.target
                && base["id"] == value["id"]
            {
                *base = value.clone();
            }
        } else if value["object"] == "page" {
            if let Some(page) = &mut self.page
                && page["id"] == value["id"]
            {
                *page = value.clone();
            }
            if let Some(grid) = &mut self.grid {
                for row in &mut grid.rows {
                    if row["id"] == value["id"] {
                        *row = value.clone();
                    }
                }
            }
            if let Some(edit) = &mut self.editor
                && let Target::Property { base, .. } = &mut edit.target
                && base["id"] == value["id"]
            {
                *base = value.clone();
            }
        }
    }
    async fn receive(&mut self, app: &App, message: Message) -> Result<()> {
        let reconcile = matches!(
            &message.result,
            Ok(Payload::Mutation { .. } | Payload::Saved { .. })
        ) || matches!(&message.result, Ok(Payload::Info { value, .. })
                if value["operation"]["state"] == "confirmed");
        if self.accept(message) && reconcile {
            let drafts = app.store.drafts().await?;
            self.local_checks = drafts
                .iter()
                .filter_map(|draft| {
                    draft.local["to_do"]["checked"]
                        .as_bool()
                        .map(|checked| (draft.id.clone(), checked))
                })
                .collect();
            self.local_text = drafts
                .iter()
                .filter_map(|draft| {
                    draft.local["text"]
                        .as_str()
                        .map(|text| (draft.id.clone(), text.to_owned()))
                })
                .collect();
            self.drafts = drafts.into_iter().map(|draft| draft.id).collect();
        }
        Ok(())
    }
    fn accept(&mut self, message: Message) -> bool {
        self.pending = self.pending.saturating_sub(1);
        if message
            .generation
            .is_some_and(|generation| generation != self.generation)
        {
            return false;
        }
        match message.result {
            Err(error) => {
                if message.write {
                    self.saving = false;
                    self.quit_after_save = false;
                }
                for node in &mut self.nodes {
                    if !node.loaded {
                        node.expanded = false;
                    }
                }
                for node in &mut self.nav {
                    if !node.loaded {
                        node.expanded = false;
                    }
                }
                self.status = format!("{}: {}", error.code, content::terminal_text(&error.message));
                if error.code == 5
                    && let Some(value) = error.partial
                {
                    let id = value["draft_id"].as_str().unwrap_or("").to_owned();
                    self.overlay = Some(Overlay::Versions {
                        id,
                        value,
                        scroll: 0,
                    });
                }
            }
            Ok(Payload::Document { page, blocks }) => {
                self.page = Some(page);
                self.nodes = blocks
                    .into_iter()
                    .map(|raw| Node {
                        raw,
                        depth: 0,
                        expanded: false,
                        loaded: false,
                    })
                    .collect();
                self.selected = 0;
                self.document_scroll = 0;
                self.follow_selection = true;
                self.grid = None;
                self.nav_focus = false;
                self.status = "Страница загружена".into();
            }
            Ok(Payload::Children { id, blocks }) => {
                if let Some(index) = self
                    .nodes
                    .iter()
                    .position(|n| n.raw["id"] == id && n.expanded && !n.loaded)
                {
                    let depth = self.nodes[index].depth + 1;
                    self.nodes[index].loaded = true;
                    self.nodes[index].expanded = true;
                    let children = blocks.into_iter().map(|raw| Node {
                        raw,
                        depth,
                        expanded: false,
                        loaded: false,
                    });
                    self.nodes.splice(index + 1..index + 1, children);
                }
                self.status = "Вложенность загружена".into();
            }
            Ok(Payload::NavChildren { id, index, items }) => {
                if self
                    .nav
                    .get(index)
                    .is_some_and(|n| n.id == Some(id) && n.expanded && !n.loaded)
                {
                    self.nav[index].loaded = true;
                    self.nav[index].expanded = true;
                    self.nav.splice(index + 1..index + 1, items);
                }
            }
            Ok(Payload::Grid(grid)) => {
                self.grid = Some(grid);
                self.page = None;
                self.nodes.clear();
                self.nav_focus = false;
                self.status =
                    "j/k строки · h/l ячейка/колонка · e свойство · H/L сменить статус".into();
            }
            Ok(Payload::Property {
                target,
                text,
                draft_id,
            }) => {
                if self.editor.is_some() {
                    return false;
                }
                if let Target::Property { base, name, .. } = &target
                    && self.grid.as_ref().is_none_or(|grid| {
                        grid.rows
                            .get(grid.row)
                            .is_none_or(|row| row["id"] != base["id"])
                            || grid.columns.get(grid.column) != Some(name)
                    })
                {
                    return false;
                }
                self.editor = Some(Edit {
                    state: EditorState::new(Lines::from(content::terminal_text(&text).as_str())),
                    handler: EditorEventHandler::default(),
                    target,
                    draft_id,
                    last_saved: text,
                });
                self.status =
                    "Свойство · i ввод · Ctrl+S отправить; JSON сохраняет атрибуты".into();
            }
            Ok(Payload::Search(items)) => {
                self.nav = items;
                self.nav_selected = 0;
                self.nav_focus = true;
                self.nav_visible = true;
                self.status =
                    "Результаты поиска; Enter — открыть. :nav — избранное и недавние".into();
            }
            Ok(Payload::Info { title, value }) => {
                if value["operation"]["state"] == "confirmed" {
                    self.update_resource(&value["operation"]["response"]);
                }
                let id = value["id"]
                    .as_str()
                    .or_else(|| value["draft_id"].as_str())
                    .unwrap_or("")
                    .to_owned();
                self.overlay =
                    Some(if title == "Изменения" || title == "Конфликт" {
                        Overlay::Versions {
                            id,
                            value,
                            scroll: 0,
                        }
                    } else {
                        Overlay::Text {
                            title,
                            text: pretty(&value),
                            scroll: 0,
                        }
                    });
            }
            Ok(Payload::Saved {
                results,
                quit,
                offline,
                error,
            }) => {
                self.saving = false;
                let quit = quit || self.quit_after_save;
                self.quit_after_save = false;
                for (id, value) in results {
                    if !offline {
                        self.drafts.remove(&id);
                        self.local_text.remove(&id);
                        self.local_checks.remove(&id);
                    }
                    self.update_resource(&value);
                }
                self.status = if offline {
                    "Черновики сохранены локально; Notion не обновлён"
                } else {
                    "Изменения подтверждены Notion"
                }
                .into();
                if let Some(error) = error {
                    self.status =
                        format!("{}: {}", error.code, content::terminal_text(&error.message));
                    if error.code == 5
                        && let Some(value) = error.partial
                    {
                        let id = value["draft_id"].as_str().unwrap_or("").to_owned();
                        self.overlay = Some(Overlay::Versions {
                            id,
                            value,
                            scroll: 0,
                        });
                    }
                } else if quit {
                    self.quit = true;
                }
            }
            Ok(Payload::Mutation { value, generation }) => {
                self.saving = false;
                if self.quit_after_save {
                    self.quit_after_save = false;
                    self.quit = true;
                }
                if let Some(id) = value["draft_id"].as_str() {
                    self.drafts.insert(id.into());
                }
                self.status = if value["published"] == false {
                    format!(
                        "Локальный черновик {} · Notion не обновлён · :drafts/:apply",
                        value["draft_id"].as_str().unwrap_or("")
                    )
                } else {
                    "Операция подтверждена; :refresh обновит экран".into()
                };
                if generation == self.generation
                    && value["object"] == "page"
                    && let Some(grid) = &mut self.grid
                {
                    for row in &mut grid.rows {
                        if row["id"] == value["id"] {
                            *row = value.clone();
                        }
                    }
                }
            }
        }
        true
    }
    async fn load_nav(&mut self, app: &App) -> Result<()> {
        let mut nav = Vec::new();
        for (kind, label) in [
            ("roots", "Корни"),
            ("favorites", "Избранное"),
            ("recent", "Недавние"),
        ] {
            nav.push(NavItem {
                id: None,
                source: None,
                title: label.into(),
                depth: 0,
                expanded: false,
                loaded: true,
            });
            for item in app.navigation(kind).await?.as_array().into_iter().flatten() {
                if let Some(id) = item["id"].as_str().and_then(|id| id.parse().ok()) {
                    nav.push(NavItem {
                        id: Some(id),
                        source: None,
                        title: item["title"].as_str().unwrap_or("Без названия").into(),
                        depth: 1,
                        expanded: false,
                        loaded: false,
                    });
                }
            }
        }
        self.nav = nav;
        self.nav_selected = self.nav.iter().position(nav_selectable).unwrap_or(0);
        Ok(())
    }
    fn palette_entries(&self) -> Vec<(String, String)> {
        let query = match &self.overlay {
            Some(Overlay::Palette { query, .. }) => query.trim(),
            _ => return Vec::new(),
        };
        if query.parse::<PageId>().is_ok() {
            return vec![(
                "Открыть страницу по ссылке или ID".into(),
                format!("open {query}"),
            )];
        }
        let mut seen = HashSet::new();
        let mut entries: Vec<_> = self
            .nav
            .iter()
            .filter_map(|item| {
                let command = item
                    .id
                    .map(|id| format!("open {id}"))
                    .or_else(|| item.source.map(|id| format!("table {id}")))?;
                seen.insert(command.clone())
                    .then(|| (item.title.clone(), command))
            })
            .collect();
        entries.extend(
            PALETTE
                .iter()
                .map(|(label, command)| (label.to_string(), command.to_string())),
        );
        let lower = query.to_lowercase();
        entries.retain(|(label, command)| {
            label.to_lowercase().contains(&lower)
                || command
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .contains(&lower)
        });
        if !query.is_empty() {
            entries.push((
                format!("Искать в Notion: {query}"),
                format!("search {query}"),
            ));
        }
        entries
    }
    fn open_nav(&mut self, app: &App, tx: &mpsc::UnboundedSender<Message>) {
        if let Some(item) = self.nav.get(self.nav_selected) {
            if let Some(id) = item.id {
                self.open_page(app, tx, id);
            } else if let Some(source) = item.source {
                self.open_grid(app, tx, source, false, None, (Value::Null, Value::Null));
            }
        }
    }
    fn scroll_document(&mut self, delta: isize) {
        self.document_scroll = self
            .document_scroll
            .saturating_add_signed(delta)
            .min(self.document_limit);
        self.follow_selection = false;
    }
    fn scroll_view(&mut self, delta: isize) {
        if self.nav_focus || self.grid.is_some() {
            self.vertical(delta);
        } else {
            self.scroll_document(delta);
        }
    }
    async fn toggle_todo(&mut self, app: &App) -> Result<()> {
        let Some(node) = self
            .current_node()
            .filter(|n| content::block_type(&n.raw) == "to_do")
        else {
            return Ok(());
        };
        let base = node.raw.clone();
        let path = format!("blocks/{}", base["id"].as_str().unwrap_or(""));
        let existing = app.store.draft(&path).await?;
        let checked = !self
            .local_checks
            .get(&path)
            .copied()
            .unwrap_or(base["to_do"]["checked"] == true);
        let patch = json!({"to_do":{"checked":checked}});
        if let Some(draft) = existing {
            if draft.base["kind"] != "patch" || draft.local != json!({"to_do":{"checked":!checked}})
            {
                return Err(Error::new(
                    5,
                    "У задачи есть другой черновик; сначала отправьте его через Ctrl+S",
                ));
            }
            app.store.put_draft(&path, draft.base, patch).await?;
        } else {
            app.stage("PATCH", &path, Some(&path), base, patch).await?;
        }
        self.local_checks.insert(path.clone(), checked);
        self.drafts.insert(path);
        self.status = "Флажок сохранён локально · Ctrl+S отправить".into();
        Ok(())
    }
    fn visible_nodes(&self) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut hidden_depth = None;
        for (index, node) in self.nodes.iter().enumerate() {
            if hidden_depth.is_some_and(|depth| node.depth > depth) {
                continue;
            }
            hidden_depth = None;
            visible.push(index);
            if !node.expanded {
                hidden_depth = Some(node.depth);
            }
        }
        visible
    }
    fn current_node(&self) -> Option<&Node> {
        self.visible_nodes()
            .get(self.selected)
            .and_then(|index| self.nodes.get(*index))
    }
    fn selected_page(&self) -> Option<Value> {
        self.grid
            .as_ref()
            .and_then(|grid| grid.rows.get(grid.row).cloned())
            .or_else(|| self.page.clone())
    }
    fn draft_id(&self) -> Option<String> {
        self.editor
            .as_ref()
            .map(|e| e.draft_id.clone())
            .or_else(|| {
                self.current_node()
                    .and_then(|n| n.raw["id"].as_str())
                    .map(|id| format!("blocks/{id}"))
            })
            .or_else(|| {
                self.selected_page()
                    .and_then(|p| p["id"].as_str().map(|id| format!("pages/{id}")))
            })
    }
    async fn persist_editor(&mut self, app: &App) -> Result<()> {
        let Some(edit) = &self.editor else {
            return Ok(());
        };
        let text = edit.text();
        if text == edit.last_saved {
            return Ok(());
        }
        let saved_text = text.clone();
        match &edit.target {
            Target::Block(base) => {
                app.stage_block(base.clone(), text.clone()).await?;
                self.local_text.insert(edit.draft_id.clone(), text);
            }
            Target::Property { base, name, kind } => {
                let existing = app.store.draft(&edit.draft_id).await?;
                let mut context = existing.as_ref().map(|d|d.base.clone()).unwrap_or_else(||json!({"kind":"patch","method":"PATCH","path":edit.draft_id,"read_path":edit.draft_id,"resource":base,"editor_inputs":{},"editor_invalid":{}}));
                let mut payload = existing
                    .map(|d| d.local)
                    .unwrap_or_else(|| json!({"properties":{}}));
                if !context["editor_inputs"].is_object() {
                    context["editor_inputs"] = json!({});
                }
                if !context["editor_invalid"].is_object() {
                    context["editor_invalid"] = json!({});
                }
                if context["editor_inputs"].get(name).is_none() {
                    context["resource"]["properties"][name] = base["properties"][name].clone();
                }
                context["editor_inputs"][name] = json!(text);
                let value = property_input(kind, &text).and_then(|value| {
                    if let Some(grid) = &self.grid {
                        validate_against_schema(&grid.schema, &json!({name:value.clone()}))?;
                    }
                    Ok(value)
                });
                match value {
                    Ok(value) => {
                        payload["properties"][name] = value;
                        context["editor_invalid"][name] = json!(false);
                    }
                    Err(_) => {
                        context["editor_invalid"][name] = json!(true);
                    }
                }
                app.store
                    .put_draft(&edit.draft_id, context, payload)
                    .await?;
            }
        }
        self.drafts.insert(edit.draft_id.clone());
        if let Some(edit) = &mut self.editor {
            edit.last_saved = saved_text;
        }
        self.status = "Черновик сохранён на устройстве · Ctrl+S отправить".into();
        Ok(())
    }
    async fn edit_block(&mut self, app: &App) -> Result<()> {
        let node = self
            .current_node()
            .ok_or_else(|| Error::input("Нет выбранного блока"))?;
        let base = node.raw.clone();
        let kind = content::block_type(&base);
        if !content::TEXT_BLOCKS.contains(&kind) {
            self.overlay = Some(Overlay::Text {
                title: format!("{kind} · защищённый блок"),
                text: pretty(&base),
                scroll: 0,
            });
            return Ok(());
        }
        let id = format!(
            "blocks/{}",
            base["id"]
                .as_str()
                .ok_or_else(|| Error::input("У блока нет ID"))?
        );
        let draft = app.store.draft(&id).await?;
        if draft
            .as_ref()
            .is_some_and(|d| d.base["kind"] != "block_text")
        {
            return Err(Error::input(
                "Сначала отправьте существующее оформление или флажок через :w",
            ));
        }
        let original = content::block_text(&base);
        let text = draft
            .and_then(|d| d.local["text"].as_str().map(str::to_owned))
            .unwrap_or_else(|| original.clone());
        if content::terminal_text(&original) != original || content::terminal_text(&text) != text {
            self.overlay = Some(Overlay::Text {
                title: "Управляющие символы · текстовое редактирование защищено".into(),
                text: pretty(&json!({"block":base,"editor_text":text})),
                scroll: 0,
            });
            return Ok(());
        }
        self.editor = Some(Edit {
            state: EditorState::new(Lines::from(text.as_str())),
            handler: EditorEventHandler::default(),
            target: Target::Block(base),
            draft_id: id,
            last_saved: text,
        });
        self.follow_selection = true;
        Ok(())
    }
    fn edit_property(&mut self, app: &App, tx: &mpsc::UnboundedSender<Message>) -> Result<()> {
        let grid = self
            .grid
            .as_ref()
            .ok_or_else(|| Error::input("Откройте таблицу"))?;
        let mut base = grid
            .rows
            .get(grid.row)
            .cloned()
            .ok_or_else(|| Error::input("Нет выбранной записи"))?;
        let name = grid
            .columns
            .get(grid.column)
            .cloned()
            .ok_or_else(|| Error::input("Нет выбранного свойства"))?;
        let kind = grid.schema["properties"][&name]["type"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        if !matches!(
            kind.as_str(),
            "title"
                | "rich_text"
                | "status"
                | "select"
                | "multi_select"
                | "checkbox"
                | "date"
                | "number"
                | "url"
                | "people"
                | "relation"
        ) {
            return Err(Error::unsupported(
                "Вычисляемое или неизвестное свойство доступно только для чтения",
            ));
        }
        let property_id = base["properties"][&name]["id"]
            .as_str()
            .or_else(|| grid.schema["properties"][&name]["id"].as_str())
            .unwrap_or(&name)
            .to_owned();
        let page: PageId = base["id"]
            .as_str()
            .ok_or_else(|| Error::input("Нет ID записи"))?
            .parse()?;
        let app = app.clone();
        let generation = self.generation;
        self.spawn(tx, Some(generation), async move {
            let full = if matches!(kind.as_str(), "title" | "rich_text" | "people" | "relation") {
                let value = app.property(page, &property_id, true, None).await?;
                if value["object"] == "list" {
                    json!(
                        array_results(&value)?
                            .iter()
                            .map(|item| item[&kind].clone())
                            .collect::<Vec<_>>()
                    )
                } else {
                    value[&kind].clone()
                }
            } else {
                base["properties"][&name][&kind].clone()
            };
            base["properties"][&name][&kind] = full.clone();
            let id = format!("pages/{page}");
            let draft = app.store.draft(&id).await?;
            let text = draft
                .as_ref()
                .and_then(|d| d.base["editor_inputs"][&name].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| property_editor_text(&kind, &full));
            Ok(Payload::Property {
                target: Target::Property { base, name, kind },
                text,
                draft_id: id,
            })
        });
        Ok(())
    }
    async fn publish(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        quit: bool,
    ) -> Result<()> {
        self.persist_editor(app).await?;
        let mut ids = Vec::new();
        if let Some(grid) = &self.grid {
            for row in &grid.rows {
                if let Some(id) = row["id"].as_str() {
                    let key = format!("pages/{id}");
                    if self.drafts.contains(&key) {
                        ids.push(key);
                    }
                }
            }
        } else {
            for node in &self.nodes {
                if let Some(id) = node.raw["id"].as_str() {
                    let key = format!("blocks/{id}");
                    if self.drafts.contains(&key) {
                        ids.push(key);
                    }
                }
            }
        }
        if let Some(edit) = &self.editor
            && !ids.contains(&edit.draft_id)
            && app.store.draft(&edit.draft_id).await?.is_some()
        {
            ids.push(edit.draft_id.clone());
        }
        if ids.is_empty() {
            self.status = "Нет изменений для отправки".into();
            if quit {
                self.quit = true;
            }
            return Ok(());
        }
        for id in &ids {
            if let Some(draft) = app.store.draft(id).await?
                && draft.base["editor_invalid"]
                    .as_object()
                    .is_some_and(|map| map.values().any(|v| v == true))
            {
                return Err(Error::input(
                    "В редакторе есть незавершённое значение свойства; черновик сохранён",
                ));
            }
        }
        self.saving = true;
        self.status = "Отправка; ввод приостановлен до подтверждения…".into();
        let app = app.clone();
        self.spawn(tx, None, async move {
            let mut results = Vec::new();
            for id in ids {
                match app.apply_draft(&id).await {
                    Ok(value) => results.push((id, value)),
                    Err(error) => {
                        return Ok(Payload::Saved {
                            results,
                            quit: false,
                            offline: app.offline,
                            error: Some(error),
                        });
                    }
                }
            }
            Ok(Payload::Saved {
                results,
                quit,
                offline: app.offline,
                error: None,
            })
        });
        Ok(())
    }
    fn mutation<F>(&mut self, tx: &mpsc::UnboundedSender<Message>, future: F)
    where
        F: Future<Output = Result<Value>> + Send + 'static,
    {
        self.saving = true;
        let generation = self.generation;
        self.spawn(tx, None, async move {
            Ok(Payload::Mutation {
                value: future.await?,
                generation,
            })
        });
    }
    fn expand(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        collapse: bool,
    ) -> Result<()> {
        if self.nav_focus {
            let index = self.nav_selected;
            let Some(item) = self.nav.get(index).cloned() else {
                return Ok(());
            };
            let Some(id) = item.id else {
                return Ok(());
            };
            if collapse {
                self.nav[index].expanded = false;
                self.nav[index].loaded = false;
                let end = (index + 1..self.nav.len())
                    .find(|i| self.nav[*i].depth <= item.depth)
                    .unwrap_or(self.nav.len());
                self.nav.drain(index + 1..end);
                return Ok(());
            }
            if item.expanded && !item.loaded {
                return Ok(());
            }
            if item.loaded {
                self.nav[index].expanded = true;
                return Ok(());
            }
            self.nav[index].expanded = true;
            let app = app.clone();
            let generation = self.generation;
            self.spawn(tx, Some(generation), async move {
                let blocks = array_results(&app.children(BlockId(id.0), true, None).await?)?;
                let items = blocks
                    .iter()
                    .filter(|b| content::block_type(b) == "child_page")
                    .filter_map(|b| {
                        Some(NavItem {
                            id: Some(b["id"].as_str()?.parse().ok()?),
                            source: None,
                            title: content::block_text(b),
                            depth: item.depth + 1,
                            expanded: false,
                            loaded: false,
                        })
                    })
                    .collect();
                Ok(Payload::NavChildren { id, index, items })
            });
        } else {
            let Some(index) = self.visible_nodes().get(self.selected).copied() else {
                return Ok(());
            };
            let node = &self.nodes[index];
            if collapse {
                self.nodes[index].expanded = false;
                return Ok(());
            }
            if node.expanded && !node.loaded {
                return Ok(());
            }
            if node.loaded {
                self.nodes[index].expanded = true;
                return Ok(());
            }
            if node.raw["has_children"] != true {
                return Ok(());
            }
            let id = node.raw["id"]
                .as_str()
                .ok_or_else(|| Error::input("Нет ID блока"))?
                .to_owned();
            let block = id.parse()?;
            self.nodes[index].expanded = true;
            let app = app.clone();
            let generation = self.generation;
            self.spawn(tx, Some(generation), async move {
                Ok(Payload::Children {
                    id,
                    blocks: array_results(&app.children(block, true, None).await?)?,
                })
            });
        }
        Ok(())
    }
    async fn command(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        line: &str,
    ) -> Result<()> {
        let (name, args) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let args = args.trim();
        match name {
            "" => {}
            "q" => {
                self.persist_editor(app).await?;
                self.quit = true;
            }
            "w" => self.publish(app, tx, false).await?,
            "wq" => self.publish(app, tx, true).await?,
            "help" => self.overlay = Some(Overlay::Help),
            "open" => self.open_page(app, tx, args.parse()?),
            "nav" => self.load_nav(app).await?,
            "refresh" => self.refresh(app, tx),
            "table" | "board" => {
                let (source, group) = args
                    .split_once(' ')
                    .map(|(a, b)| (a, Some(b.to_owned())))
                    .unwrap_or((args, None));
                self.open_grid(
                    app,
                    tx,
                    source.parse()?,
                    name == "board",
                    group,
                    (Value::Null, Value::Null),
                );
            }
            "view" => self.open_view(app, tx, args.parse()?),
            "drafts" | "operations" => {
                let app = app.clone();
                let drafts = name == "drafts";
                self.spawn(tx, None, async move {
                    let value = if drafts {
                        serde_json::to_value(app.store.drafts().await?)?
                    } else {
                        serde_json::to_value(app.store.operations().await?)?
                    };
                    Ok(Payload::Info {
                        title: if drafts {
                            "Черновики · :apply ID"
                        } else {
                            "Операции · :check ID"
                        }
                        .into(),
                        value,
                    })
                });
            }
            "apply" => {
                let id = args.to_owned();
                let app = app.clone();
                self.saving = true;
                self.spawn(tx, None, async move {
                    let value = app.apply_draft(&id).await?;
                    Ok(Payload::Saved {
                        results: vec![(id, value)],
                        quit: false,
                        offline: app.offline,
                        error: None,
                    })
                });
            }
            "check" => {
                let id = args.to_owned();
                let app = app.clone();
                self.spawn(tx, None, async move {
                    Ok(Payload::Info {
                        title: "Сверка операции".into(),
                        value: app.check_operation(&id).await?,
                    })
                });
            }
            "filter" | "sort" => {
                let grid = self
                    .grid
                    .clone()
                    .ok_or_else(|| Error::input("Откройте data source"))?;
                let value: Value = serde_json::from_str(args)?;
                if name == "sort" && !value.is_array() && !value.is_null() {
                    return Err(Error::input("sort должен быть массивом или null"));
                }
                let (filter, sorts) = if name == "filter" {
                    (value, grid.sorts)
                } else {
                    (grid.filter, value)
                };
                self.open_grid(
                    app,
                    tx,
                    grid.source,
                    grid.board,
                    Some(grid.group),
                    (filter, sorts),
                );
            }
            "search" | "local" => {
                self.generation += 1;
                let query = args.to_owned();
                let app = app.clone();
                let local = name == "local";
                let generation = self.generation;
                self.spawn(tx, Some(generation), async move {
                    let value = app.search(&query, local, true, None).await?;
                    let mut items = vec![NavItem {
                        id: None,
                        source: None,
                        title: if local {
                            format!("Локально · загружено {}", value["loaded_resources"])
                        } else {
                            "Названия в Notion".into()
                        },
                        depth: 0,
                        expanded: false,
                        loaded: true,
                    }];
                    for entry in value["results"].as_array().into_iter().flatten() {
                        let id = if local {
                            cached_search_page(&app, entry["id"].as_str().unwrap_or("")).await?
                        } else if entry["object"] == "page" {
                            entry["id"].as_str().and_then(|id| id.parse().ok())
                        } else {
                            None
                        };
                        let source = if local {
                            entry["id"]
                                .as_str()
                                .and_then(|path| path.strip_prefix("data_sources/"))
                                .and_then(|id| id.parse().ok())
                        } else if entry["object"] == "data_source" {
                            entry["id"].as_str().and_then(|id| id.parse().ok())
                        } else {
                            None
                        };
                        let title = if local {
                            format!(
                                "{}{}",
                                entry["snippet"].as_str().unwrap_or(""),
                                if id.is_none() && source.is_none() {
                                    " [цель не поддерживается или родитель не загружен]"
                                } else {
                                    ""
                                }
                            )
                        } else {
                            let title = content::page_title(entry);
                            if id.is_none() && source.is_none() {
                                format!("{title} [цель не поддерживается]")
                            } else {
                                title
                            }
                        };
                        items.push(NavItem {
                            id,
                            source,
                            title,
                            depth: 1,
                            expanded: false,
                            loaded: true,
                        });
                    }
                    Ok(Payload::Search(items))
                });
            }
            "favorite" | "pin" => {
                let page = self
                    .selected_page()
                    .ok_or_else(|| Error::input("Откройте страницу"))?;
                let id = page["id"]
                    .as_str()
                    .ok_or_else(|| Error::input("Нет ID страницы"))?;
                let kind = if name == "pin" { "roots" } else { "favorites" };
                app.remember(kind, id, &content::page_title(&page)).await?;
                self.load_nav(app).await?;
                self.status = "Сохранено в навигации".into();
            }
            "diff" | "conflict" => {
                let id = self
                    .draft_id()
                    .ok_or_else(|| Error::input("Выберите изменённый блок или свойство"))?;
                self.persist_editor(app).await?;
                let app = app.clone();
                let title = if name == "diff" {
                    "Изменения"
                } else {
                    "Конфликт"
                }
                .to_owned();
                self.spawn(tx, None, async move {
                    let value = app.draft_versions(&id).await?;
                    Ok(Payload::Info { title, value })
                });
            }
            "format" => {
                self.persist_editor(app).await?;
                let base = self
                    .current_node()
                    .ok_or_else(|| Error::input("Выберите текстовый блок"))?
                    .raw
                    .clone();
                let id = base["id"]
                    .as_str()
                    .ok_or_else(|| Error::input("Нет ID блока"))?;
                let path = format!("blocks/{id}");
                if self.drafts.contains(&path) {
                    return Err(Error::input(
                        "Сначала отправьте текст через :w, затем примените оформление",
                    ));
                }
                let mut parts = args.split_whitespace();
                let attribute = parts
                    .next()
                    .ok_or_else(|| Error::input("format ATTRIBUTE VALUE [START END]"))?;
                let raw = parts
                    .next()
                    .ok_or_else(|| Error::input("Нужно значение атрибута"))?;
                let value = if matches!(attribute, "link" | "color") {
                    if raw == "null" {
                        Value::Null
                    } else {
                        json!(raw)
                    }
                } else {
                    serde_json::from_str(raw)?
                };
                let start = parts
                    .next()
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| Error::input("START должен быть числом"))?
                    .unwrap_or(0);
                let end = parts
                    .next()
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| Error::input("END должен быть числом"))?;
                let patch = content::format_patch(&base, start, end, attribute, value)?;
                let draft = app.stage("PATCH", &path, Some(&path), base, patch).await?;
                self.drafts.insert(draft);
                self.editor = None;
                self.status = "Оформление сохранено локально · :w отправить".into();
            }
            "append" => {
                let (kind, text) = args.split_once(' ').unwrap_or((args, ""));
                let kind = kind.to_owned();
                let text = text.to_owned();
                let page = self
                    .page
                    .clone()
                    .ok_or_else(|| Error::input("Откройте страницу"))?;
                let id = page["id"]
                    .as_str()
                    .ok_or_else(|| Error::input("Нет ID страницы"))?
                    .parse()?;
                let block = content::new_block(&kind, &text)?;
                let app = app.clone();
                self.mutation(tx, async move { app.append(id, vec![block]).await });
            }
            "new" => {
                let parent = self
                    .page
                    .as_ref()
                    .and_then(|p| p["id"].as_str())
                    .ok_or_else(|| Error::input("Откройте родительскую страницу"))?
                    .parse()?;
                let title = args.to_owned();
                let app = app.clone();
                self.mutation(
                    tx,
                    async move { app.create_page(parent, &title, None).await },
                );
            }
            "task" => {
                let source = self
                    .grid
                    .as_ref()
                    .ok_or_else(|| Error::input("Откройте таблицу или доску"))?
                    .source;
                let title = args.to_owned();
                let app = app.clone();
                self.mutation(tx, async move {
                    app.create_task(source, &title, json!({})).await
                });
            }
            "title" => {
                let base = self
                    .selected_page()
                    .ok_or_else(|| Error::input("Откройте страницу"))?;
                let key = base["properties"]
                    .as_object()
                    .and_then(|p| p.iter().find(|(_, v)| v["type"] == "title"))
                    .map(|(k, _)| k.clone())
                    .unwrap_or_else(|| "title".into());
                let payload = json!({"properties":{key:{"title":crate::app::text_runs(args)}}});
                let app = app.clone();
                self.mutation(tx, async move { app.patch_page_from(base, payload).await });
            }
            "move" => {
                let id = self
                    .page
                    .as_ref()
                    .and_then(|p| p["id"].as_str())
                    .ok_or_else(|| Error::input("Откройте страницу"))?
                    .parse()?;
                let parent = args.parse()?;
                let app = app.clone();
                self.mutation(tx, async move { app.move_page(id, parent).await });
            }
            "trash" => {
                self.overlay = Some(Overlay::Confirm(if args == "page" {
                    Confirm::Page(
                        self.selected_page()
                            .ok_or_else(|| Error::input("Откройте страницу"))?,
                    )
                } else if args == "block" {
                    Confirm::Block(
                        self.current_node()
                            .ok_or_else(|| Error::input("Выберите блок"))?
                            .raw
                            .clone(),
                    )
                } else {
                    return Err(Error::input("trash page или trash block"));
                }));
            }
            "theme" => {
                if !matches!(args, "default" | "dark" | "light") {
                    return Err(Error::input("theme default|dark|light"));
                }
                self.theme = args.into();
            }
            "width" => {
                let width: u16 = args.parse().map_err(|_| Error::input("width 12..60"))?;
                if !(12..=60).contains(&width) {
                    return Err(Error::input("width 12..60"));
                }
                self.width = width;
            }
            _ => return Err(Error::input("Неизвестная команда; ? — справка")),
        }
        Ok(())
    }
    async fn event(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        event: Event,
    ) -> Result<()> {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Release {
                return Ok(());
            }
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.persist_editor(app).await?;
                self.cancelled = true;
                self.quit = true;
                return Ok(());
            }
        }
        if self.saving {
            if let Event::Key(key) = &event
                && key.code == KeyCode::Char('q')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                self.quit_after_save = true;
            }
            return Ok(());
        }
        if let Event::Key(key) = &event
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            match key.code {
                KeyCode::Char('s') => return self.publish(app, tx, false).await,
                KeyCode::Char('q') => {
                    self.persist_editor(app).await?;
                    self.quit = true;
                    return Ok(());
                }
                _ => {}
            }
        }
        if self.command.is_some() {
            match event {
                Event::Key(key) => match key.code {
                    KeyCode::Esc => self.command = None,
                    KeyCode::Enter => {
                        let line = self.command.take().unwrap();
                        self.command(app, tx, &line).await?;
                    }
                    KeyCode::Backspace => {
                        self.command.as_mut().unwrap().pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.command.as_mut().unwrap().push(c)
                    }
                    _ => {}
                },
                Event::Paste(text) => self
                    .command
                    .as_mut()
                    .unwrap()
                    .push_str(&content::terminal_text(&text).replace('\n', " ")),
                _ => {}
            }
            return Ok(());
        }
        if let Event::Key(key) = &event
            && key.code == KeyCode::Char(':')
            && self.overlay.is_none()
            && self
                .editor
                .as_ref()
                .is_none_or(|e| e.state.mode == EditorMode::Normal)
        {
            self.command = Some(String::new());
            return Ok(());
        }
        if let Event::Key(key) = &event
            && key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.persist_editor(app).await?;
            self.overlay = Some(Overlay::Palette {
                selected: 0,
                query: String::new(),
            });
            return Ok(());
        }
        if self.overlay.is_some() {
            self.overlay_event(app, tx, event).await?;
            return Ok(());
        }
        if let Some(edit) = &mut self.editor {
            if let Event::Key(key) = &event
                && key.code == KeyCode::Esc
                && edit.state.mode == EditorMode::Normal
            {
                self.persist_editor(app).await?;
                self.editor = None;
                return Ok(());
            }
            let before = edit.text();
            let event = match event {
                Event::Paste(text) => Event::Paste(content::terminal_text(&text)),
                other => other,
            };
            edit.handler.on_event(event, &mut edit.state);
            if edit.text() != before {
                self.persist_editor(app).await?;
            }
            return Ok(());
        }
        match event {
            Event::Key(key) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    if key.code == KeyCode::Char('w') {
                        self.nav_focus = !self.nav_focus;
                        self.nav_visible = true;
                    } else if key.code == KeyCode::Char('r') {
                        self.refresh(app, tx);
                    } else if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('u')) {
                        let delta = isize::try_from(self.content_area.height / 2).unwrap_or(1);
                        self.scroll_view(if key.code == KeyCode::Char('d') {
                            delta
                        } else {
                            -delta
                        });
                    }
                    return Ok(());
                }
                match key.code {
                    KeyCode::Char('q') => self.quit = true,
                    KeyCode::Char('?') => self.overlay = Some(Overlay::Help),
                    KeyCode::F(2) => self.properties = !self.properties,
                    KeyCode::Tab | KeyCode::BackTab => {
                        self.nav_focus = !self.nav_focus;
                        self.nav_visible = true;
                    }
                    KeyCode::Char('n') => self.nav_visible = !self.nav_visible,
                    KeyCode::Char('/') => {
                        self.overlay = Some(Overlay::Palette {
                            selected: 0,
                            query: String::new(),
                        })
                    }
                    KeyCode::Esc => {
                        self.nav_focus = false;
                        self.properties = false;
                    }
                    KeyCode::PageDown => {
                        self.scroll_view(isize::try_from(self.content_area.height).unwrap_or(1))
                    }
                    KeyCode::PageUp => {
                        self.scroll_view(-isize::try_from(self.content_area.height).unwrap_or(1))
                    }
                    KeyCode::Char('j') | KeyCode::Down => self.vertical(1),
                    KeyCode::Char('k') | KeyCode::Up => self.vertical(-1),
                    KeyCode::Char('h') | KeyCode::Left => {
                        if self.grid.is_some() && !self.nav_focus {
                            self.horizontal(-1);
                        } else {
                            self.expand(app, tx, true)?;
                        }
                    }
                    KeyCode::Char('l') | KeyCode::Right => {
                        if self.grid.is_some() && !self.nav_focus {
                            self.horizontal(1);
                        } else {
                            self.expand(app, tx, false)?;
                        }
                    }
                    KeyCode::Char('H') => self.move_card(app, tx, -1)?,
                    KeyCode::Char('L') => self.move_card(app, tx, 1)?,
                    KeyCode::Char('+') => self.width = (self.width + 2).min(60),
                    KeyCode::Char('-') => self.width = self.width.saturating_sub(2).max(12),
                    KeyCode::Char('b') if self.grid.is_some() && !self.nav_focus => {
                        let grid = self.grid.as_mut().unwrap();
                        if grid.group.is_empty() {
                            return Err(Error::unsupported(
                                "Для доски нужно свойство status/select",
                            ));
                        }
                        grid.board = !grid.board;
                    }
                    KeyCode::Char('e') if self.grid.is_some() && !self.nav_focus => {
                        self.edit_property(app, tx)?
                    }
                    KeyCode::Char('i') | KeyCode::Char('e')
                        if self.grid.is_none() && !self.nav_focus =>
                    {
                        self.edit_block(app).await?;
                        if key.code == KeyCode::Char('i')
                            && let Some(edit) = &mut self.editor
                        {
                            edit.state.mode = EditorMode::Insert;
                        }
                    }
                    KeyCode::Enter => {
                        if self.nav_focus {
                            self.open_nav(app, tx);
                        } else if let Some(grid) = &self.grid {
                            if let Some(id) = grid
                                .rows
                                .get(grid.row)
                                .and_then(|p| p["id"].as_str())
                                .and_then(|id| id.parse().ok())
                            {
                                self.open_page(app, tx, id);
                            }
                        } else if let Some(node) = self.current_node() {
                            if content::block_type(&node.raw) == "child_page" {
                                let id = node.raw["id"]
                                    .as_str()
                                    .ok_or_else(|| Error::input("Нет ID дочерней страницы"))?
                                    .parse()?;
                                self.open_page(app, tx, id);
                            } else if content::block_type(&node.raw) == "child_database" {
                                let id = node.raw["id"]
                                    .as_str()
                                    .ok_or_else(|| Error::input("Нет ID базы"))?
                                    .parse::<crate::content::DatabaseId>()?;
                                let app = app.clone();
                                let generation = self.generation;
                                self.spawn(tx, Some(generation), async move {
                                    let database = app.resource(&format!("databases/{id}")).await?;
                                    let sources =
                                        database["data_sources"].as_array().ok_or_else(|| {
                                            Error::unsupported("База не содержит data sources")
                                        })?;
                                    if sources.len() != 1 {
                                        return Ok(Payload::Info {
                                            title: "Источники базы · :table DATA_SOURCE_ID".into(),
                                            value: json!(sources),
                                        });
                                    }
                                    let source = sources[0]["id"]
                                        .as_str()
                                        .ok_or_else(|| Error::input("Нет ID источника"))?
                                        .parse()?;
                                    let schema = app.data_source(source).await?;
                                    let rows =
                                        array_results(&app.query(source, json!({}), true).await?)?;
                                    Ok(Payload::Grid(make_grid(
                                        source,
                                        schema,
                                        rows,
                                        false,
                                        None,
                                        (Value::Null, Value::Null),
                                        None,
                                    )?))
                                });
                            } else {
                                self.edit_block(app).await?;
                            }
                        }
                    }
                    KeyCode::Char(' ') => {
                        if !self.nav_focus {
                            self.toggle_todo(app).await?;
                        }
                    }
                    _ => {}
                }
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                    let delta = if mouse.kind == MouseEventKind::ScrollDown {
                        1
                    } else {
                        -1
                    };
                    if self.nav_area.contains((mouse.column, mouse.row).into()) {
                        let previous = self.nav_focus;
                        self.nav_focus = true;
                        self.vertical(delta);
                        self.nav_focus = previous;
                    } else if self.grid.is_some() {
                        let previous = self.nav_focus;
                        self.nav_focus = false;
                        self.vertical(delta);
                        self.nav_focus = previous;
                    } else {
                        self.scroll_document(delta * 3);
                    }
                }
                MouseEventKind::Down(_) => {
                    if self.nav_area.contains((mouse.column, mouse.row).into()) {
                        self.nav_focus = true;
                        let selected = (usize::from(mouse.row.saturating_sub(self.nav_area.y))
                            + self.nav_state.offset())
                        .min(self.nav.len().saturating_sub(1));
                        if self.nav.get(selected).is_some_and(nav_selectable) {
                            self.nav_selected = selected;
                            self.open_nav(app, tx);
                        }
                    } else if self.content_area.contains((mouse.column, mouse.row).into()) {
                        self.nav_focus = false;
                        if let Some(grid) = &mut self.grid {
                            if grid.board {
                                if let Some((index, _)) =
                                    self.document_rows.iter().find(|(_, rect)| {
                                        rect.contains((mouse.column, mouse.row).into())
                                    })
                                {
                                    grid.row = *index;
                                }
                            } else {
                                grid.row = (usize::from(
                                    mouse.row.saturating_sub(self.content_area.y + 2),
                                ) + self.table_state.offset())
                                .min(grid.rows.len().saturating_sub(1));
                                grid.column = (usize::from(
                                    mouse.column.saturating_sub(self.content_area.x + 1),
                                ) / usize::from(self.width + 1))
                                .min(grid.columns.len().saturating_sub(1));
                            }
                        } else if let Some((index, _)) = self
                            .document_rows
                            .iter()
                            .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                        {
                            self.selected = *index;
                            self.follow_selection = true;
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }
        Ok(())
    }
    fn vertical(&mut self, delta: isize) {
        if self.nav_focus {
            let selectable: Vec<_> = self
                .nav
                .iter()
                .enumerate()
                .filter(|(_, item)| nav_selectable(item))
                .map(|(index, _)| index)
                .collect();
            if let Some(position) = selectable
                .iter()
                .position(|index| *index == self.nav_selected)
            {
                self.nav_selected = selectable[step(position, delta, selectable.len())];
            } else if let Some(index) = selectable.first() {
                self.nav_selected = *index;
            }
        } else if let Some(grid) = &mut self.grid {
            if grid.board {
                let columns = board_columns(grid);
                let current = grid
                    .rows
                    .get(grid.row)
                    .map(|row| row_column(grid, row, &columns))
                    .unwrap_or(0);
                let rows: Vec<_> = grid
                    .rows
                    .iter()
                    .enumerate()
                    .filter(|(_, row)| row_column(grid, row, &columns) == current)
                    .map(|(i, _)| i)
                    .collect();
                if let Some(position) = rows.iter().position(|i| *i == grid.row) {
                    grid.row = rows[step(position, delta, rows.len())];
                }
            } else {
                grid.row = step(grid.row, delta, grid.rows.len());
            }
        } else {
            self.selected = step(self.selected, delta, self.visible_nodes().len());
            self.follow_selection = true;
        }
    }
    fn horizontal(&mut self, delta: isize) {
        if let Some(grid) = &mut self.grid {
            if grid.board {
                let columns = board_columns(grid);
                let current = grid
                    .rows
                    .get(grid.row)
                    .map(|row| row_column(grid, row, &columns))
                    .unwrap_or(0);
                let target = step(current, delta, columns.len());
                if let Some(row) = grid
                    .rows
                    .iter()
                    .position(|row| row_column(grid, row, &columns) == target)
                {
                    grid.row = row;
                }
            } else {
                grid.column = step(grid.column, delta, grid.columns.len());
            }
        }
    }
    fn move_card(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        delta: isize,
    ) -> Result<()> {
        let grid = self
            .grid
            .as_ref()
            .filter(|grid| grid.board)
            .ok_or_else(|| Error::input("H/L работает на доске"))?;
        let row = grid
            .rows
            .get(grid.row)
            .cloned()
            .ok_or_else(|| Error::input("Нет карточки"))?;
        let columns = board_columns(grid);
        let current = row_column(grid, &row, &columns);
        let next = step(current, delta, columns.len());
        if next == current {
            return Ok(());
        }
        let kind = grid.schema["properties"][&grid.group]["type"]
            .as_str()
            .unwrap_or("");
        let value = if let Some(id) = columns[next].ids.first() {
            json!({"id":id})
        } else {
            Value::Null
        };
        let patch = json!({"properties":{grid.group.clone():{kind:value}}});
        validate_against_schema(&grid.schema, &patch["properties"])?;
        let app = app.clone();
        self.mutation(tx, async move { app.patch_page_from(row, patch).await });
        Ok(())
    }
    async fn overlay_event(
        &mut self,
        app: &App,
        tx: &mpsc::UnboundedSender<Message>,
        event: Event,
    ) -> Result<()> {
        if let Event::Paste(text) = &event
            && let Some(Overlay::Palette { query, selected }) = &mut self.overlay
        {
            query.push_str(&content::terminal_text(text).replace('\n', " "));
            *selected = 0;
            return Ok(());
        }
        let entries = self.palette_entries();
        let Event::Key(key) = event else {
            return Ok(());
        };
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('q')
                && !matches!(self.overlay, Some(Overlay::Palette { .. })))
        {
            self.overlay = None;
            return Ok(());
        }
        match self.overlay.as_mut().unwrap() {
            Overlay::Palette { selected, query } => match key.code {
                KeyCode::Down => *selected = step(*selected, 1, entries.len()),
                KeyCode::Up => *selected = step(*selected, -1, entries.len()),
                KeyCode::Backspace => {
                    query.pop();
                    *selected = 0;
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(c);
                    *selected = 0;
                }
                KeyCode::Enter => {
                    if let Some((_, command)) = entries.get(*selected) {
                        let command = command.clone();
                        self.overlay = None;
                        if command.ends_with(' ') {
                            self.command = Some(command);
                        } else {
                            self.command(app, tx, &command).await?;
                        }
                    }
                }
                _ => {}
            },
            Overlay::Text { scroll, .. } | Overlay::Versions { scroll, .. } => match key.code {
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Char('L') | KeyCode::Char('R') => {
                    if let Some(Overlay::Versions { id, .. }) = &self.overlay {
                        self.overlay = Some(Overlay::Confirm(Confirm::Resolve {
                            id: id.clone(),
                            choice: if key.code == KeyCode::Char('L') {
                                "local"
                            } else {
                                "remote"
                            }
                            .into(),
                        }));
                    }
                }
                _ => {}
            },
            Overlay::Confirm(action) => {
                if matches!(
                    key.code,
                    KeyCode::Char('y')
                        | KeyCode::Char('Y')
                        | KeyCode::Char('д')
                        | KeyCode::Char('Д')
                ) {
                    let action = action.clone();
                    self.overlay = None;
                    let app = app.clone();
                    self.mutation(tx, async move {
                        match action {
                            Confirm::Page(base) => {
                                app.patch_page_from(base, json!({"in_trash":true})).await
                            }
                            Confirm::Block(base) => {
                                let path = format!(
                                    "blocks/{}",
                                    base["id"]
                                        .as_str()
                                        .ok_or_else(|| Error::input("Нет ID блока"))?
                                );
                                content::block_patch(&base, json!({"in_trash":true}))?;
                                app.mutate(
                                    "PATCH",
                                    &path,
                                    Some(&path),
                                    base,
                                    json!({"in_trash":true}),
                                )
                                .await
                            }
                            Confirm::Resolve { id, choice } => {
                                app.resolve_draft(&id, &choice).await
                            }
                        }
                    });
                }
            }
            Overlay::Help => {}
        }
        Ok(())
    }
}

struct TerminalGuard;
fn restore_terminal() {
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}
pub async fn run(app: App, initial: Option<PageId>) -> Result<()> {
    let stored = app.store.drafts().await?;
    let drafts = stored.iter().map(|d| d.id.clone()).collect();
    let mut ui = Ui::new(drafts);
    for draft in stored {
        if draft.base["kind"] == "block_text"
            && let Some(text) = draft.local["text"].as_str()
        {
            ui.local_text.insert(draft.id, text.into());
        } else if let Some(checked) = draft.local["to_do"]["checked"].as_bool() {
            ui.local_checks.insert(draft.id, checked);
        }
    }
    ui.load_nav(&app).await?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
    let _guard = TerminalGuard;
    let mut terminal = ratatui::try_init()?;
    execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let recent = app.navigation("recent").await?;
    let resume = recent
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|item| item["id"].as_str()?.parse().ok());
    if let Some(id) = initial
        .or(resume)
        .or_else(|| ui.nav.iter().find_map(|item| item.id))
    {
        ui.open_page(&app, &tx, id);
    } else {
        ui.nav_focus = true;
        ui.overlay = Some(Overlay::Palette {
            selected: 0,
            query: String::new(),
        });
    }
    let mut events = EventStream::new();
    while !ui.quit {
        terminal.draw(|frame| draw(frame, &mut ui, app.offline))?;
        tokio::select! {
            Some(message)=rx.recv()=>ui.receive(&app, message).await?,
            event=events.next()=>match event{Some(Ok(event))=>{if let Err(error)=ui.event(&app,&tx,event).await{ui.status=format!("{}: {}",error.code,content::terminal_text(&error.message));}},Some(Err(_))=>return Err(Error::new(1,"Ошибка событий терминала")),None=>break},
        }
    }
    if ui.cancelled {
        return Err(Error::new(130, "Отменено; локальные черновики сохранены"));
    }
    Ok(())
}

fn draw(frame: &mut Frame, ui: &mut Ui, offline: bool) {
    let area = frame.area();
    frame.render_widget(Block::default().style(theme_style(&ui.theme)), area);
    if area.width < 20 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("Увеличьте терминал · Ctrl+Q выйти").wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let vertical = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .split(area);
    let title = ui
        .grid
        .as_ref()
        .map(|g| {
            format!(
                "{} / {}",
                g.name,
                if g.board {
                    "Доска"
                } else {
                    "Таблица"
                }
            )
        })
        .or_else(|| ui.page.as_ref().map(content::page_title))
        .unwrap_or_else(|| "Заметки и задачи".into());
    let header =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(20)]).split(vertical[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" zottion", accent(&ui.theme).add_modifier(Modifier::BOLD)),
            Span::styled("  /  ", muted(&ui.theme)),
            Span::raw(content::terminal_text(&title)),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(muted(&ui.theme)),
        ),
        vertical[0],
    );
    let header_state = format!(
        "{}{} ",
        if ui.saving {
            "Отправка…"
        } else if offline {
            "Офлайн"
        } else {
            "Notion"
        },
        if ui.drafts.is_empty() {
            String::new()
        } else {
            format!(" · *{}", ui.drafts.len())
        }
    );
    frame.render_widget(
        Paragraph::new(header_state)
            .right_aligned()
            .style(muted(&ui.theme)),
        header[1],
    );
    let body = vertical[1];
    let nav_width = if ui.nav_visible && body.width >= 70 {
        (body.width / 4).clamp(22, 30)
    } else {
        0
    };
    let prop_width = if ui.properties && body.width >= 110 {
        30
    } else {
        0
    };
    let columns = Layout::horizontal([
        Constraint::Length(nav_width),
        Constraint::Min(1),
        Constraint::Length(prop_width),
    ])
    .split(body);
    ui.nav_area = columns[0];
    ui.content_area = columns[1];
    if nav_width > 0 {
        let items: Vec<_> = ui
            .nav
            .iter()
            .map(|item| {
                if !nav_selectable(item) {
                    ListItem::new(Line::styled(format!(" {}", item.title), muted(&ui.theme)))
                } else {
                    let current = item.id.is_some_and(|id| {
                        ui.page
                            .as_ref()
                            .is_some_and(|page| page["id"] == id.to_string())
                    });
                    let marker = if item.expanded { "▾" } else { "▸" };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{}{} ", " ".repeat(item.depth.min(8)), marker),
                            muted(&ui.theme),
                        ),
                        Span::styled(
                            content::terminal_text(&item.title),
                            if current {
                                accent(&ui.theme).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                    ]))
                }
            })
            .collect();
        ui.nav_state.select(
            ui.nav
                .get(ui.nav_selected)
                .filter(|_| ui.nav_focus)
                .map(|_| ui.nav_selected),
        );
        frame.render_stateful_widget(
            List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::RIGHT)
                        .border_style(muted(&ui.theme)),
                )
                .highlight_symbol("› ")
                .highlight_style(selection(&ui.theme, true)),
            columns[0],
            &mut ui.nav_state,
        );
    }
    if ui
        .editor
        .as_ref()
        .is_some_and(|edit| matches!(edit.target, Target::Property { .. }))
    {
        let edit = ui.editor.as_mut().unwrap();
        let name = match &edit.target {
            Target::Property { name, .. } => content::terminal_text(name),
            _ => String::new(),
        };
        frame.render_widget(
            EditorView::new(&mut edit.state).wrap(true).theme(
                EditorTheme::default()
                    .hide_status_line()
                    .selection_style(selection(&ui.theme, true))
                    .cursor_style(Style::default().add_modifier(Modifier::REVERSED))
                    .base(theme_style(&ui.theme))
                    .block(
                        Block::bordered()
                            .title(format!(" {name} · Esc обратно "))
                            .border_style(accent(&ui.theme)),
                    ),
            ),
            columns[1].inner(Margin::new(1, 1)),
        );
    } else if let Some(grid) = &ui.grid {
        if grid.board {
            draw_board(frame, ui, columns[1]);
        } else {
            draw_table(frame, ui, columns[1]);
        }
    } else {
        draw_document(frame, ui, columns[1]);
    }
    if prop_width > 0 {
        draw_properties(frame, ui, columns[2]);
    } else if ui.properties {
        let panel = Rect::new(
            columns[1].x + columns[1].width / 3,
            columns[1].y,
            columns[1].width - columns[1].width / 3,
            columns[1].height,
        );
        frame.render_widget(Clear, panel);
        draw_properties(frame, ui, panel);
    }
    let mode = ui
        .editor
        .as_ref()
        .map(|e| match e.state.mode.name().as_str() {
            "Insert" => "Ввод",
            "Visual" => "Выделение",
            _ => "Normal",
        })
        .unwrap_or(if ui.nav_focus {
            "Страницы"
        } else {
            "Просмотр"
        });
    let message = if ui.quit_after_save {
        "Выход после завершения отправки…"
    } else if ui.saving {
        "Отправка; дождитесь подтверждения…"
    } else if ui.pending > 0 {
        "Загрузка…"
    } else {
        &ui.status
    };
    let is_error = message.as_bytes().get(1) == Some(&b':')
        && message.as_bytes().first().is_some_and(u8::is_ascii_digit);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!(" {mode}  "), accent(&ui.theme)),
            Span::styled(
                content::terminal_text(message),
                if is_error {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    muted(&ui.theme)
                },
            ),
        ])),
        Rect::new(vertical[2].x, vertical[2].y, vertical[2].width, 1),
    );
    let hints = if ui.editor.is_some() {
        " Esc Normal/блоки  Ctrl+P поиск  Ctrl+S отправить  Ctrl+Q выйти"
    } else if ui.nav_focus {
        " Enter открыть  Tab документ  Ctrl+P поиск  Ctrl+Q выйти"
    } else if ui.grid.is_some() {
        " h/l колонки  e править  b вид  Ctrl+P поиск  Ctrl+S отправить  Ctrl+Q выйти"
    } else if area.width < 100 {
        " i писать  Tab дерево  Ctrl+P поиск  Ctrl+S отправить  Ctrl+Q выйти  ? помощь"
    } else {
        " i писать  Enter открыть  Tab страницы  Ctrl+P поиск  Ctrl+S отправить  Ctrl+Q выйти  F2 свойства  ? помощь"
    };
    frame.render_widget(
        Paragraph::new(hints).style(muted(&ui.theme)),
        Rect::new(vertical[2].x, vertical[2].y + 1, vertical[2].width, 1),
    );
    if let Some(command) = &ui.command {
        let command_area = Rect::new(vertical[2].x, vertical[2].y + 1, vertical[2].width, 1);
        frame.render_widget(Clear, command_area);
        frame.render_widget(Paragraph::new(format!(":{command}")), command_area);
        let cursor = Line::from(command.as_str())
            .width()
            .saturating_add(1)
            .min(usize::from(command_area.width.saturating_sub(1))) as u16;
        frame.set_cursor_position((command_area.x + cursor, command_area.y));
    }
    if let Some(overlay) = &ui.overlay {
        draw_overlay(frame, overlay, &ui.theme, &ui.palette_entries());
    }
}

fn document_text(ui: &Ui, node: &Node, selected: bool) -> Text<'static> {
    let kind = content::block_type(&node.raw);
    let id = format!("blocks/{}", node.raw["id"].as_str().unwrap_or(""));
    let checked = ui
        .local_checks
        .get(&id)
        .copied()
        .unwrap_or(node.raw["to_do"]["checked"] == true);
    let prefix = match kind {
        "bulleted_list_item" => "• ",
        "numbered_list_item" => "1. ",
        "to_do" => {
            if checked {
                "[x] "
            } else {
                "[ ] "
            }
        }
        "quote" => "│ ",
        "callout" => "› ",
        "child_page" | "child_database" | "toggle" => {
            if node.expanded {
                "▾ "
            } else {
                "▸ "
            }
        }
        "divider" => "────────────────────",
        _ => "",
    };
    let base = if kind.starts_with("heading_") {
        Style::default().add_modifier(Modifier::BOLD)
    } else if kind == "to_do" && checked {
        muted(&ui.theme)
    } else {
        Style::default()
    };
    let plain = ui.local_text.get(&id).cloned().unwrap_or_else(|| {
        let text = content::block_text(&node.raw);
        if text.is_empty() {
            node.raw[kind]["title"].as_str().unwrap_or("").to_owned()
        } else {
            text
        }
    });
    let mut lines = vec![Line::from(vec![Span::styled(
        format!(
            "{}{}{}",
            "  ".repeat(node.depth.min(12)),
            if ui.drafts.contains(&id) { "* " } else { "" },
            prefix
        ),
        muted(&ui.theme),
    )])];
    if ui.local_text.contains_key(&id)
        || node.raw[kind]["rich_text"]
            .as_array()
            .is_none_or(|rich| rich.is_empty())
    {
        for (index, line) in content::terminal_text(&plain).split('\n').enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            lines
                .last_mut()
                .unwrap()
                .spans
                .push(Span::styled(line.to_owned(), base));
        }
    } else {
        for part in node.raw[kind]["rich_text"].as_array().unwrap() {
            let mut style = base;
            for (name, modifier) in [
                ("bold", Modifier::BOLD),
                ("italic", Modifier::ITALIC),
                ("underline", Modifier::UNDERLINED),
                ("strikethrough", Modifier::CROSSED_OUT),
            ] {
                if part["annotations"][name] == true {
                    style = style.add_modifier(modifier);
                }
            }
            if part["href"].is_string() || part["text"]["link"]["url"].is_string() {
                style = style
                    .patch(accent(&ui.theme))
                    .add_modifier(Modifier::UNDERLINED);
            }
            let text = part["text"]["content"]
                .as_str()
                .or_else(|| part["plain_text"].as_str())
                .unwrap_or("");
            for (index, line) in content::terminal_text(text).split('\n').enumerate() {
                if index > 0 {
                    lines.push(Line::default());
                }
                lines
                    .last_mut()
                    .unwrap()
                    .spans
                    .push(Span::styled(line.to_owned(), style));
            }
        }
    }
    if plain.is_empty() && kind != "divider" {
        lines.last_mut().unwrap().spans.push(Span::styled(
            if content::TEXT_BLOCKS.contains(&kind) {
                if selected {
                    "Пустой блок · i писать".into()
                } else {
                    " ".into()
                }
            } else {
                format!("{kind} · защищено")
            },
            muted(&ui.theme),
        ));
    }
    if node.raw["has_children"] == true && !node.loaded {
        lines
            .last_mut()
            .unwrap()
            .spans
            .push(Span::styled("  ▸ не загружено", muted(&ui.theme)));
    }
    if !content::TEXT_BLOCKS.contains(&kind)
        && !matches!(kind, "child_page" | "child_database" | "divider")
        && !plain.is_empty()
    {
        lines
            .last_mut()
            .unwrap()
            .spans
            .push(Span::styled(" · защищено", muted(&ui.theme)));
    }
    if matches!(kind, "heading_2" | "heading_3") {
        lines.insert(0, Line::default());
    }
    Text::from(lines)
}

fn draw_document(frame: &mut Frame, ui: &mut Ui, area: Rect) {
    ui.document_rows.clear();
    let margin = if area.width > 104 {
        (area.width - 100) / 2
    } else {
        2.min(area.width / 8)
    };
    let inner = area.inner(Margin::new(margin, 1));
    if inner.is_empty() {
        return;
    }
    let title = ui
        .page
        .as_ref()
        .map(content::page_title)
        .unwrap_or_else(|| "Откройте свои заметки".into());
    frame.render_widget(
        Paragraph::new(content::terminal_text(&title))
            .style(Style::default().add_modifier(Modifier::BOLD)),
        Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(1), 1),
    );
    let body = Rect::new(
        inner.x,
        inner.y + 2,
        inner.width,
        inner.height.saturating_sub(2),
    );
    ui.content_area = body;
    let visible = ui.visible_nodes();
    if visible.is_empty() {
        let text = if ui.pending > 0 {
            "Загрузка страницы…"
        } else if ui.page.is_some() {
            "На странице пока нет блоков.\nCtrl+P → Добавить блок — начать заметку."
        } else {
            "Ctrl+P — найти страницу по названию или вставить ссылку.\nTab — перейти к закреплённым и недавним страницам."
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(muted(&ui.theme)),
            body,
        );
        return;
    }
    let width = body.width.saturating_sub(2).max(1);
    let editing_block = ui
        .editor
        .as_ref()
        .filter(|e| matches!(e.target, Target::Block(_)))
        .map(|e| e.draft_id.clone());
    let blocks: Vec<_> = visible
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let node = &ui.nodes[*index];
            let id = format!("blocks/{}", node.raw["id"].as_str().unwrap_or(""));
            let editing = editing_block.as_ref() == Some(&id);
            let paragraph = Paragraph::new(document_text(ui, node, position == ui.selected))
                .wrap(Wrap { trim: false });
            let height = if editing {
                let text = ui.editor.as_ref().unwrap().text();
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    .saturating_add(2)
                    .max(4)
                    .min(usize::from(body.height).max(1))
            } else {
                paragraph.line_count(width).max(1)
            };
            (paragraph, height, editing)
        })
        .collect();
    let total: usize = blocks.iter().map(|(_, height, _)| height).sum();
    ui.document_limit = total.saturating_sub(usize::from(body.height));
    if ui.follow_selection {
        let start: usize = blocks.iter().take(ui.selected).map(|(_, h, _)| h).sum();
        let height = blocks.get(ui.selected).map(|(_, h, _)| *h).unwrap_or(1);
        if start < ui.document_scroll || start >= ui.document_scroll + usize::from(body.height) {
            ui.document_scroll = start;
        } else if height <= usize::from(body.height)
            && start + height > ui.document_scroll + usize::from(body.height)
        {
            ui.document_scroll = start + height - usize::from(body.height);
        }
        ui.follow_selection = false;
    }
    ui.document_scroll = ui.document_scroll.min(ui.document_limit);
    let mut offset = 0;
    for (position, (paragraph, height, editing)) in blocks.into_iter().enumerate() {
        let end = offset + height;
        if end > ui.document_scroll && offset < ui.document_scroll + usize::from(body.height) {
            let clipped = ui.document_scroll.saturating_sub(offset);
            let y = body.y + offset.saturating_sub(ui.document_scroll) as u16;
            let shown = (height - clipped).min(usize::from(body.bottom().saturating_sub(y))) as u16;
            let rect = Rect::new(body.x + 2, y, width, shown);
            ui.document_rows.push((position, rect));
            let selected = position == ui.selected;
            if selected {
                frame.render_widget(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(if ui.nav_focus {
                            muted(&ui.theme)
                        } else {
                            accent(&ui.theme)
                        }),
                    Rect::new(body.x, y, 1, shown),
                );
            }
            if editing {
                let edit = ui.editor.as_mut().unwrap();
                frame.render_widget(
                    EditorView::new(&mut edit.state).wrap(true).theme(
                        EditorTheme::default()
                            .hide_status_line()
                            .selection_style(selection(&ui.theme, true))
                            .cursor_style(Style::default().add_modifier(Modifier::REVERSED))
                            .base(theme_style(&ui.theme))
                            .block(
                                Block::default()
                                    .borders(Borders::TOP | Borders::BOTTOM)
                                    .border_style(accent(&ui.theme))
                                    .title(" Редактирование · Esc обратно "),
                            ),
                    ),
                    rect,
                );
            } else {
                frame.render_widget(
                    paragraph
                        .scroll((clipped.min(u16::MAX as usize) as u16, 0))
                        .style(if selected {
                            selection(&ui.theme, !ui.nav_focus)
                        } else {
                            Style::default()
                        }),
                    rect,
                );
            }
        }
        offset = end;
    }
    if total > usize::from(body.height) {
        let track = body.height.saturating_sub(1);
        let thumb = (ui.document_scroll * usize::from(track) / ui.document_limit.max(1)) as u16;
        frame.render_widget(
            Paragraph::new("┃").style(muted(&ui.theme)),
            Rect::new(body.right().saturating_sub(1), body.y + thumb, 1, 1),
        );
    }
}

fn draw_table(frame: &mut Frame, ui: &mut Ui, area: Rect) {
    let grid = ui.grid.as_ref().unwrap();
    let count = (usize::from(area.width.saturating_sub(2)) / usize::from(ui.width + 1)).max(1);
    let start = (grid.column / count) * count;
    let columns: Vec<_> = grid.columns.iter().skip(start).take(count).collect();
    let header = Row::new(columns.iter().enumerate().map(|(index, name)| {
        Cell::from(content::terminal_text(name)).style(if start + index == grid.column {
            accent(&ui.theme).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        })
    }));
    let rows = grid.rows.iter().enumerate().map(|(index, row)| {
        Row::new(columns.iter().map(|name| {
            Cell::from(content::terminal_text(&property_display(
                &row["properties"][*name],
            )))
        }))
        .style(if index == grid.row {
            accent(&ui.theme)
        } else {
            Style::default()
        })
    });
    ui.table_state.select(Some(grid.row));
    frame.render_stateful_widget(
        Table::new(rows, vec![Constraint::Length(ui.width); columns.len()])
            .header(header)
            .row_highlight_style(selection(&ui.theme, !ui.nav_focus))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(muted(&ui.theme))
                    .title(format!(
                        " {} · e редактор · b доска · {}/{} ",
                        content::terminal_text(&grid.name),
                        grid.column + 1,
                        grid.columns.len()
                    )),
            ),
        area,
        &mut ui.table_state,
    );
}
fn draw_board(frame: &mut Frame, ui: &mut Ui, area: Rect) {
    ui.document_rows.clear();
    let grid = ui.grid.as_ref().unwrap();
    let columns = board_columns(grid);
    let current = grid
        .rows
        .get(grid.row)
        .map(|row| row_column(grid, row, &columns))
        .unwrap_or(0);
    let visible = (usize::from(area.width) / 24).max(1);
    let start = (current / visible) * visible;
    let shown: Vec<_> = columns
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .collect();
    let areas = Layout::horizontal(vec![
        Constraint::Ratio(1, shown.len().max(1) as u32);
        shown.len()
    ])
    .split(area.inner(Margin::new(1, 1)));
    for ((column_index, column), area) in shown.into_iter().zip(areas.iter()) {
        let rows: Vec<_> = grid
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row_column(grid, row, &columns) == column_index)
            .collect();
        let border = Block::default()
            .borders(Borders::TOP)
            .border_style(if current == column_index {
                accent(&ui.theme)
            } else {
                muted(&ui.theme)
            })
            .title(content::terminal_text(&format!(
                " {}  {} ",
                column.name,
                rows.len()
            )));
        let inside = border.inner(*area).inner(Margin::new(1, 1));
        frame.render_widget(border, *area);
        let cards: Vec<_> = rows
            .iter()
            .map(|(index, row)| {
                let dirty = row["id"]
                    .as_str()
                    .is_some_and(|id| ui.drafts.contains(&format!("pages/{id}")));
                let text = format!(
                    "{}{}",
                    if dirty { "* " } else { "" },
                    content::terminal_text(&content::page_title(row))
                );
                let card = Paragraph::new(text).wrap(Wrap { trim: false });
                let height = card
                    .line_count(inside.width.saturating_sub(2).max(1))
                    .max(1)
                    + 1;
                (*index, card, height)
            })
            .collect();
        let selected_end: usize = cards
            .iter()
            .take_while(|(index, _, _)| *index != grid.row)
            .map(|(_, _, h)| *h)
            .sum::<usize>()
            + cards
                .iter()
                .find(|(index, _, _)| *index == grid.row)
                .map(|(_, _, h)| *h)
                .unwrap_or(0);
        let scroll = if current == column_index {
            selected_end.saturating_sub(usize::from(inside.height))
        } else {
            0
        };
        let mut offset = 0;
        for (index, card, height) in cards {
            if offset + height > scroll && offset < scroll + usize::from(inside.height) {
                let clipped = scroll.saturating_sub(offset);
                let y = inside.y + offset.saturating_sub(scroll) as u16;
                let shown =
                    (height - clipped).min(usize::from(inside.bottom().saturating_sub(y))) as u16;
                let rect = Rect::new(inside.x, y, inside.width, shown);
                ui.document_rows.push((index, rect));
                let selected = index == grid.row;
                frame.render_widget(
                    card.scroll((clipped.min(u16::MAX as usize) as u16, 0))
                        .block(
                            Block::default()
                                .borders(Borders::LEFT)
                                .border_style(if selected {
                                    accent(&ui.theme)
                                } else {
                                    muted(&ui.theme)
                                }),
                        )
                        .style(if selected {
                            selection(&ui.theme, !ui.nav_focus)
                        } else {
                            Style::default()
                        }),
                    rect,
                );
            }
            offset += height;
        }
        if rows.is_empty() {
            frame.render_widget(Paragraph::new("Нет задач").style(muted(&ui.theme)), inside);
        }
    }
}
fn draw_properties(frame: &mut Frame, ui: &Ui, area: Rect) {
    let value = ui.selected_page().unwrap_or(Value::Null);
    let mut lines = vec![Line::from("F2 — скрыть свойства")];
    if let Some(properties) = value["properties"].as_object() {
        for (name, value) in properties {
            lines.push(Line::from(content::terminal_text(&format!(
                "{name}: {}",
                property_display(value)
            ))));
        }
    } else if let Some(node) = ui.current_node() {
        lines.push(Line::from(format!(
            "Тип: {}",
            content::block_type(&node.raw)
        )));
        lines.push(Line::from(content::terminal_text(
            node.raw["id"].as_str().unwrap_or(""),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Свойства ")),
        area,
    );
}
fn draw_overlay(frame: &mut Frame, overlay: &Overlay, theme: &str, entries: &[(String, String)]) {
    let full = frame.area();
    let area = Rect::new(
        full.x + 2,
        full.y + 1,
        full.width.saturating_sub(4),
        full.height.saturating_sub(2),
    );
    if !matches!(overlay, Overlay::Palette { .. }) {
        frame.render_widget(Clear, area);
    }
    match overlay {
        Overlay::Help => {
            frame.render_widget(
                Paragraph::new(HELP)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Клавиши · Esc закрыть "))
                    .style(theme_style(theme)),
                area,
            );
        }
        Overlay::Palette { selected, query } => {
            let width = full.width.saturating_sub(4).min(76);
            let height = full.height.saturating_sub(4).min(18);
            let popup = Rect::new(
                full.x + full.width.saturating_sub(width) / 2,
                full.y + 2,
                width,
                height,
            );
            frame.render_widget(Clear, popup);
            let border = Block::bordered()
                .title(" Страницы и действия ")
                .border_style(accent(theme))
                .style(theme_style(theme));
            let inside = border.inner(popup);
            frame.render_widget(border, popup);
            let parts = Layout::vertical([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inside);
            frame.render_widget(
                Paragraph::new(format!(" > {query}")).style(accent(theme)),
                parts[0],
            );
            let items: Vec<_> = entries
                .iter()
                .map(|(label, _)| ListItem::new(format!(" {label}")))
                .collect();
            let mut state = ListState::default();
            state.select(Some((*selected).min(entries.len().saturating_sub(1))));
            frame.render_stateful_widget(
                List::new(items)
                    .highlight_symbol("› ")
                    .highlight_style(selection(theme, true)),
                parts[1],
                &mut state,
            );
            frame.render_widget(
                Paragraph::new(" Печатайте для поиска · ↑↓ выбрать · Enter · Esc")
                    .style(muted(theme)),
                parts[2],
            );
            let cursor = Line::from(query.as_str())
                .width()
                .saturating_add(3)
                .min(usize::from(parts[0].width.saturating_sub(1))) as u16;
            frame.set_cursor_position((parts[0].x + cursor, parts[0].y));
        }
        Overlay::Text {
            title,
            text,
            scroll,
        } => frame.render_widget(
            Paragraph::new(content::terminal_text(text))
                .wrap(Wrap { trim: false })
                .scroll((*scroll, 0))
                .block(
                    Block::bordered()
                        .title(format!(" {} · j/k · Esc ", content::terminal_text(title))),
                ),
            area,
        ),
        Overlay::Versions { id, value, scroll } => {
            let border =
                Block::bordered().title(" База / локальная / удалённая · L/R выбор · Esc ");
            let inside = border.inner(area);
            frame.render_widget(border, area);
            if inside.width >= 96 {
                let columns = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(inside);
                let base = if value["base"]["resource"].is_object() {
                    &value["base"]["resource"]
                } else {
                    &value["base"]
                };
                for ((label, version), area) in [
                    ("Базовая", base),
                    ("Локальная", &value["local"]),
                    ("Удалённая", &value["remote"]),
                ]
                .into_iter()
                .zip(columns.iter())
                {
                    frame.render_widget(
                        Paragraph::new(content::terminal_text(&pretty(version)))
                            .wrap(Wrap { trim: false })
                            .scroll((*scroll, 0))
                            .block(Block::default().borders(Borders::TOP).title(label)),
                        *area,
                    );
                }
            } else {
                let text = format!(
                    "Черновик: {id}\n\nБАЗОВАЯ\n{}\n\nЛОКАЛЬНАЯ\n{}\n\nУДАЛЁННАЯ\n{}",
                    pretty(&value["base"]),
                    pretty(&value["local"]),
                    pretty(&value["remote"])
                );
                frame.render_widget(
                    Paragraph::new(content::terminal_text(&text))
                        .wrap(Wrap { trim: false })
                        .scroll((*scroll, 0)),
                    inside,
                );
            }
        }
        Overlay::Confirm(action) => {
            let text = match action {
                Confirm::Page(_) => "Переместить страницу в корзину?",
                Confirm::Block(_) => "Переместить выбранный блок в корзину?",
                Confirm::Resolve { choice, .. } => {
                    if choice == "local" {
                        "Отправить локальную версию поверх актуальной удалённой?"
                    } else {
                        "Удалить локальный черновик и оставить удалённую версию?"
                    }
                }
            };
            frame.render_widget(
                Paragraph::new(format!("{text}\n\ny/д — подтвердить · Esc — отменить"))
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" Подтверждение ")),
                area,
            );
        }
    }
}
fn theme_style(theme: &str) -> Style {
    if std::env::var_os("NO_COLOR").is_some() {
        Style::default()
    } else {
        match theme {
            "dark" => Style::default()
                .fg(Color::Rgb(216, 222, 233))
                .bg(Color::Rgb(23, 27, 36)),
            "light" => Style::default()
                .fg(Color::Rgb(37, 43, 54))
                .bg(Color::Rgb(250, 249, 245)),
            _ => Style::default(),
        }
    }
}
fn accent(theme: &str) -> Style {
    if std::env::var_os("NO_COLOR").is_some() {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(match theme {
            "light" => Color::Rgb(49, 91, 151),
            "dark" => Color::Rgb(130, 170, 255),
            _ => Color::Blue,
        })
    }
}
fn muted(theme: &str) -> Style {
    if std::env::var_os("NO_COLOR").is_some() {
        Style::default()
    } else {
        Style::default().fg(match theme {
            "light" => Color::Rgb(109, 116, 130),
            "dark" => Color::Rgb(130, 142, 163),
            _ => Color::DarkGray,
        })
    }
}
fn selection(theme: &str, focused: bool) -> Style {
    if !focused {
        return Style::default();
    }
    if std::env::var_os("NO_COLOR").is_some() {
        return Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    }
    match theme {
        "dark" => Style::default().bg(Color::Rgb(41, 52, 72)),
        "light" => Style::default().bg(Color::Rgb(224, 232, 244)),
        _ => Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    }
}
fn nav_selectable(item: &NavItem) -> bool {
    item.id.is_some() || item.source.is_some()
}
fn array_results(value: &Value) -> Result<Vec<Value>> {
    value["results"]
        .as_array()
        .cloned()
        .ok_or_else(|| Error::new(8, "Ответ не содержит полный массив results"))
}
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".into())
}
fn step(index: usize, delta: isize, len: usize) -> usize {
    index
        .saturating_add_signed(delta)
        .min(len.saturating_sub(1))
}
fn schema_name(schema: &Value, key: &str) -> Option<String> {
    schema["properties"]
        .as_object()?
        .iter()
        .find(|(name, value)| name.as_str() == key || value["id"] == key)
        .map(|(name, _)| name.clone())
}
fn make_grid(
    source: DataSourceId,
    schema: Value,
    rows: Vec<Value>,
    board: bool,
    group: Option<String>,
    query: (Value, Value),
    view: Option<(ViewId, String)>,
) -> Result<Grid> {
    let (filter, sorts) = query;
    let (view, name) = view
        .map(|(id, name)| (Some(id), Some(name)))
        .unwrap_or((None, None));
    let properties = schema["properties"]
        .as_object()
        .ok_or_else(|| Error::input("У data source нет схемы"))?;
    let mut columns: Vec<_> = properties.keys().cloned().collect();
    columns.sort_by_key(|name| {
        if properties[name]["type"] == "title" {
            0
        } else {
            1
        }
    });
    let group = if let Some(key) = group {
        schema_name(&schema, &key)
            .ok_or_else(|| Error::input("Свойство группировки отсутствует в схеме"))?
    } else {
        properties
            .iter()
            .find(|(_, p)| p["type"] == "status")
            .or_else(|| properties.iter().find(|(_, p)| p["type"] == "select"))
            .map(|(name, _)| name.clone())
            .unwrap_or_default()
    };
    if board
        && !matches!(
            properties.get(&group).and_then(|p| p["type"].as_str()),
            Some("status" | "select")
        )
    {
        return Err(Error::unsupported(
            "Для доски нужно существующее status/select свойство",
        ));
    }
    let name = name.unwrap_or_else(|| content::rich_text_plain(&schema["title"]));
    Ok(Grid {
        source,
        schema,
        rows,
        columns,
        row: 0,
        column: 0,
        board,
        group,
        by_group: false,
        filter,
        sorts,
        view,
        name,
    })
}
fn board_columns(grid: &Grid) -> Vec<BoardColumn> {
    let prop = &grid.schema["properties"][&grid.group];
    let kind = prop["type"].as_str().unwrap_or("");
    let mut columns = Vec::new();
    if grid.by_group && kind == "status" {
        for group in prop[kind]["groups"].as_array().into_iter().flatten() {
            columns.push(BoardColumn {
                name: group["name"].as_str().unwrap_or("Группа").into(),
                ids: group["option_ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|id| id.as_str().map(str::to_owned))
                    .collect(),
            });
        }
    } else {
        for option in prop[kind]["options"].as_array().into_iter().flatten() {
            columns.push(BoardColumn {
                name: option["name"].as_str().unwrap_or("Вариант").into(),
                ids: option["id"]
                    .as_str()
                    .map(|id| vec![id.into()])
                    .unwrap_or_default(),
            });
        }
    }
    columns.push(BoardColumn {
        name: "Без статуса / другое".into(),
        ids: Vec::new(),
    });
    columns
}
fn row_column(grid: &Grid, row: &Value, columns: &[BoardColumn]) -> usize {
    let kind = grid.schema["properties"][&grid.group]["type"]
        .as_str()
        .unwrap_or("");
    let value = &row["properties"][&grid.group][kind];
    columns
        .iter()
        .position(|column| {
            value["id"]
                .as_str()
                .is_some_and(|id| column.ids.iter().any(|candidate| candidate == id))
        })
        .unwrap_or(columns.len().saturating_sub(1))
}
fn property_display(property: &Value) -> String {
    let kind = property["type"].as_str().unwrap_or("");
    let value = &property[kind];
    let mut text = match kind {
        "title" | "rich_text" => content::rich_text_plain(value),
        "status" | "select" => value["name"].as_str().unwrap_or("—").into(),
        "checkbox" => if value == true { "[x]" } else { "[ ]" }.into(),
        "url" => value.as_str().unwrap_or("—").into(),
        "formula" => format!("= {}", pretty(value)),
        "rollup" => format!("Σ {}", pretty(value)),
        _ => value.to_string(),
    };
    if property["has_more"] == true {
        text.push_str(" [+ не всё загружено]");
    }
    text.replace('\n', " ")
}
fn property_editor_text(kind: &str, full: &Value) -> String {
    let input = property_write_value(kind, full.clone());
    let full = &input;
    match kind {
        "status" | "select" => full["name"].as_str().unwrap_or("").into(),
        "url" => full.as_str().unwrap_or("").into(),
        _ => pretty(full),
    }
}
fn property_input(kind: &str, text: &str) -> Result<Value> {
    let value = match kind {
        "status" | "select" => {
            if text.trim().is_empty() {
                Value::Null
            } else {
                json!({"name":text.trim()})
            }
        }
        "url" => {
            if text.trim().is_empty() {
                Value::Null
            } else {
                json!(text.trim())
            }
        }
        _ => serde_json::from_str(text)?,
    };
    let input = json!({kind:property_write_value(kind,value)});
    validate_property(kind, &input)?;
    Ok(input)
}

fn property_write_value(kind: &str, value: Value) -> Value {
    if matches!(kind, "title" | "rich_text") {
        return value
            .as_array()
            .map(|items| json!(items.iter().map(content::rich_input).collect::<Vec<_>>()))
            .unwrap_or(value);
    }
    if matches!(kind, "people" | "relation") {
        return value
            .as_array()
            .map(|items| {
                json!(
                    items
                        .iter()
                        .map(|item| json!({"id":item["id"]}))
                        .collect::<Vec<_>>()
                )
            })
            .unwrap_or(value);
    }
    if kind == "multi_select" {
        return value
            .as_array()
            .map(|items| {
                json!(
                    items
                        .iter()
                        .map(|item| if item["id"].is_string() {
                            json!({"id":item["id"]})
                        } else {
                            json!({"name":item["name"]})
                        })
                        .collect::<Vec<_>>()
                )
            })
            .unwrap_or(value);
    }
    value
}

async fn cached_search_page(app: &App, path: &str) -> Result<Option<PageId>> {
    if let Some(id) = path.strip_prefix("pages/") {
        return Ok(id.parse().ok());
    }
    let mut path = path.to_owned();
    let mut seen = HashSet::new();
    for _ in 0..100 {
        if !seen.insert(path.clone()) {
            break;
        }
        let Some(block) = app.store.snapshot("resource", &path).await? else {
            return Ok(None);
        };
        if content::block_type(&block) == "child_page" {
            return Ok(block["id"].as_str().and_then(|id| id.parse().ok()));
        }
        if let Some(id) = block["parent"]["page_id"].as_str() {
            return Ok(id.parse().ok());
        }
        if let Some(id) = block["parent"]["block_id"].as_str() {
            path = format!("blocks/{id}");
        } else {
            break;
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Scope, Store};
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};
    fn page() -> Value {
        json!({"object":"page","id":"b55c9c91-384d-452b-81db-d1ef79372b75","properties":{"title":{"type":"title","title":[{"text":{"content":"Кириллица 😀"}}]}}})
    }
    fn block() -> Value {
        json!({"object":"block","id":"c02fc1d3-db8b-45c5-a222-27595b15aea7","type":"paragraph","paragraph":{"rich_text":[{"type":"text","text":{"content":"Привет"}}]}})
    }
    #[tokio::test]
    async fn wrapped_document_mouse_and_search_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap(),
            None,
            true,
        );
        let mut long = block();
        long["paragraph"]["rich_text"][0]["text"]["content"] = json!(format!(
            "{} КОНЕЦ",
            "Длинный текст с кириллицей 😀 ".repeat(100)
        ));
        let mut second = block();
        second["id"] = json!("58b15747-ff92-4e6b-94dd-ab080940ef04");
        second["paragraph"]["rich_text"][0]["text"]["content"] = json!("Следующий блок");
        let mut ui = Ui::new(HashSet::new());
        ui.accept(Message {
            generation: None,
            write: false,
            result: Ok(Payload::Document {
                page: page(),
                blocks: vec![long, second],
            }),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut ui, true)).unwrap();
        assert!(ui.document_limit > 0);
        ui.scroll_document(isize::MAX);
        terminal.draw(|frame| draw(frame, &mut ui, true)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("КОНЕЦ"));
        let (_, rect) = ui
            .document_rows
            .iter()
            .find(|(index, _)| *index == 1)
            .copied()
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        ui.event(
            &app,
            &tx,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .await
        .unwrap();
        assert_eq!(ui.selected, 1);
        let id: PageId = page()["id"].as_str().unwrap().parse().unwrap();
        ui.nav = vec![
            NavItem {
                id: None,
                source: None,
                title: "Корни".into(),
                depth: 0,
                expanded: false,
                loaded: true,
            },
            NavItem {
                id: Some(id),
                source: None,
                title: "Кириллица".into(),
                depth: 1,
                expanded: false,
                loaded: false,
            },
            NavItem {
                id: None,
                source: None,
                title: "Недавние".into(),
                depth: 0,
                expanded: false,
                loaded: true,
            },
            NavItem {
                id: Some(id),
                source: None,
                title: "Другая".into(),
                depth: 1,
                expanded: false,
                loaded: false,
            },
        ];
        ui.nav_focus = true;
        ui.nav_selected = 1;
        ui.vertical(1);
        assert_eq!(ui.nav_selected, 3);
        terminal.draw(|frame| draw(frame, &mut ui, true)).unwrap();
        ui.event(
            &app,
            &tx,
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: ui.nav_area.x,
                row: ui.nav_area.y,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .await
        .unwrap();
        assert_eq!(ui.nav_selected, 3);
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
        )
        .await
        .unwrap();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert!(matches!(&ui.overlay, Some(Overlay::Palette { query, .. }) if query == "q"));
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        ui.event(
            &app,
            &tx,
            Event::Paste(
                "https://app.notion.com/p/workspace/b55c9c91384d452b81dbd1ef79372b75".into(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(ui.palette_entries().len(), 1);
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert!(ui.overlay.is_none() && ui.command.is_none());
        assert!(ui.pending > 0);
        assert!(ui.generation > 0);
    }
    #[tokio::test]
    async fn direct_edit_save_quit_and_pending_checkboxes() {
        let dir = tempfile::tempdir().unwrap();
        let app = App::new(
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap(),
            None,
            true,
        );
        let mut ui = Ui::new(HashSet::new());
        ui.accept(Message {
            generation: None,
            write: false,
            result: Ok(Payload::Document {
                page: page(),
                blocks: vec![block()],
            }),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert_eq!(ui.editor.as_ref().unwrap().state.mode, EditorMode::Insert);
        ui.event(&app, &tx, Event::Paste("Новая мысль 😀".into()))
            .await
            .unwrap();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        )
        .await
        .unwrap();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        )
        .await
        .unwrap();
        assert!(!ui.quit && ui.quit_after_save);
        let message = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        ui.receive(&app, message).await.unwrap();
        assert!(!ui.saving);
        assert!(ui.quit);
        let path = "blocks/c02fc1d3-db8b-45c5-a222-27595b15aea7";
        assert!(
            app.store.draft(path).await.unwrap().unwrap().local["text"]
                .as_str()
                .unwrap()
                .contains("Новая мысль 😀")
        );
        let todo = json!({"object":"block","id":"58b15747-ff92-4e6b-94dd-ab080940ef04","type":"to_do","to_do":{"checked":false,"rich_text":[{"text":{"content":"Задача"}}]}});
        let mut ui = Ui::new(HashSet::new());
        ui.accept(Message {
            generation: None,
            write: false,
            result: Ok(Payload::Document {
                page: page(),
                blocks: vec![todo],
            }),
        });
        let path = "blocks/58b15747-ff92-4e6b-94dd-ab080940ef04";
        for checked in [true, false] {
            ui.event(
                &app,
                &tx,
                Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)),
            )
            .await
            .unwrap();
            assert_eq!(ui.local_checks.get(path), Some(&checked));
            assert_eq!(
                app.store.draft(path).await.unwrap().unwrap().local["to_do"]["checked"],
                checked
            );
        }
        app.store
            .put_draft(
                path,
                json!({"kind":"block_text"}),
                json!({"text":"Не терять"}),
            )
            .await
            .unwrap();
        assert_eq!(ui.toggle_todo(&app).await.unwrap_err().code, 5);
        assert_eq!(
            app.store.draft(path).await.unwrap().unwrap().local["text"],
            "Не терять"
        );
        assert!(app.store.operations().await.unwrap().is_empty());
        ui.saving = true;
        ui.quit_after_save = true;
        ui.accept(Message {
            generation: None,
            write: true,
            result: Err(Error::new(6, "Сбой отправки")),
        });
        assert!(!ui.quit && !ui.saving && !ui.quit_after_save);
    }
    #[test]
    fn narrow_unicode_protected_content_and_stale_results() {
        let mut ui = Ui::new(HashSet::new());
        ui.generation = 2;
        ui.pending = 2;
        assert!(!ui.accept(Message {
            generation: Some(1),
            write: false,
            result: Ok(Payload::Document {
                page: page(),
                blocks: vec![block()]
            })
        }));
        assert!(ui.page.is_none());
        let mut protected = block();
        protected["type"] = json!("unsupported");
        protected["has_children"] = json!(true);
        assert!(ui.accept(Message {
            generation: Some(2),
            write: false,
            result: Ok(Payload::Document {
                page: page(),
                blocks: vec![block(), protected]
            })
        }));
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut ui, true)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("zottion"));
        assert!(text.contains("защищено") || text.contains("не загружено"));
        assert!(text.contains("Кириллица"));
    }
    #[tokio::test]
    async fn edtui_paste_double_escape_and_durable_draft() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap();
        let app = App::new(store, None, true);
        let mut ui = Ui::new(HashSet::new());
        ui.nodes.push(Node {
            raw: block(),
            depth: 0,
            expanded: false,
            loaded: false,
        });
        let (tx, _) = mpsc::unbounded_channel();
        ui.edit_block(&app).await.unwrap();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        ui.event(&app, &tx, Event::Paste("Язык 😀".into()))
            .await
            .unwrap();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert!(ui.editor.is_some());
        assert_eq!(ui.editor.as_ref().unwrap().state.mode, EditorMode::Normal);
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert!(ui.editor.is_none());
        let draft = app
            .store
            .draft("blocks/c02fc1d3-db8b-45c5-a222-27595b15aea7")
            .await
            .unwrap()
            .unwrap();
        assert!(draft.local["text"].as_str().unwrap().contains("Язык 😀"));
    }
    #[tokio::test]
    async fn inspection_is_clean_and_invalid_property_input_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap();
        let app = App::new(store, None, true);
        let mut ui = Ui::new(HashSet::new());
        ui.nodes.push(Node {
            raw: block(),
            depth: 0,
            expanded: false,
            loaded: false,
        });
        let (tx, _) = mpsc::unbounded_channel();
        ui.edit_block(&app).await.unwrap();
        ui.event(
            &app,
            &tx,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        )
        .await
        .unwrap();
        assert!(app.store.drafts().await.unwrap().is_empty());
        let mut base = page();
        base["properties"]["People"] = json!({"id":"p","type":"people","people":[]});
        let id = format!("pages/{}", base["id"].as_str().unwrap());
        ui.editor = Some(Edit {
            state: EditorState::new(Lines::from("[")),
            handler: EditorEventHandler::default(),
            target: Target::Property {
                base,
                name: "People".into(),
                kind: "people".into(),
            },
            draft_id: id.clone(),
            last_saved: "[]".into(),
        });
        ui.persist_editor(&app).await.unwrap();
        let draft = app.store.draft(&id).await.unwrap().unwrap();
        assert_eq!(draft.base["editor_inputs"]["People"], "[");
        assert_eq!(draft.base["editor_invalid"]["People"], true);
    }
    #[tokio::test]
    async fn clean_editor_write_and_write_quit_are_noops() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap();
        let app = App::new(store, None, true);
        let mut ui = Ui::new(HashSet::new());
        ui.nodes.push(Node {
            raw: block(),
            depth: 0,
            expanded: false,
            loaded: false,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        ui.edit_block(&app).await.unwrap();
        ui.command(&app, &tx, "w").await.unwrap();
        assert!(!ui.saving, "clean :w must not apply a nonexistent draft");
        assert!(app.store.drafts().await.unwrap().is_empty());
        assert!(rx.try_recv().is_err());
        ui.command(&app, &tx, "wq").await.unwrap();
        assert!(ui.quit);

        ui.quit = false;
        ui.editor.as_mut().unwrap().state = EditorState::new(Lines::from("Изменено"));
        ui.persist_editor(&app).await.unwrap();
        let id = ui.editor.as_ref().unwrap().draft_id.clone();
        app.store.delete_draft(&id).await.unwrap();
        let mut saved = block();
        saved["paragraph"]["rich_text"][0]["text"]["content"] = json!("Изменено");
        ui.accept(Message {
            generation: None,
            write: true,
            result: Ok(Payload::Saved {
                results: vec![(id.clone(), saved)],
                quit: false,
                offline: false,
                error: None,
            }),
        });
        ui.command(&app, &tx, "w").await.unwrap();
        assert!(!ui.saving, "second :w must not reapply the saved draft");
        ui.command(&app, &tx, "wq").await.unwrap();
        assert!(ui.quit);
        assert!(app.store.drafts().await.unwrap().is_empty());

        ui.quit = false;
        ui.editor.as_mut().unwrap().state = EditorState::new(Lines::from("Подтверждено"));
        ui.persist_editor(&app).await.unwrap();
        app.store.delete_draft(&id).await.unwrap();
        let mut confirmed = block();
        confirmed["paragraph"]["rich_text"][0]["text"]["content"] = json!("Подтверждено");
        ui.receive(&app, Message { generation: None, write: false, result: Ok(Payload::Info {
            title: "Сверка операции".into(),
            value: json!({"operation":{"state":"confirmed","base":{"draft_id":id},"response":confirmed},"remote":confirmed,"matches_local":true}),
        }) }).await.unwrap();
        assert!(!ui.drafts.contains(&id));
        assert!(!ui.local_text.contains_key(&id));
        assert_eq!(ui.nodes[0].raw, confirmed);
        let Target::Block(base) = &ui.editor.as_ref().unwrap().target else {
            panic!()
        };
        assert_eq!(base, &confirmed);
        ui.command(&app, &tx, "wq").await.unwrap();
        assert!(ui.quit);
        assert!(!ui.saving);
        assert!(app.store.drafts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn control_characters_are_protected_from_plain_editing() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap();
        let app = App::new(store, None, true);
        let mut ui = Ui::new(HashSet::new());
        let mut raw = block();
        raw["paragraph"]["rich_text"][0]["text"]["content"] = json!("до\u{001b}[31mпосле");
        ui.nodes.push(Node {
            raw: raw.clone(),
            depth: 0,
            expanded: false,
            loaded: false,
        });
        ui.edit_block(&app).await.unwrap();
        ui.persist_editor(&app).await.unwrap();
        assert!(
            ui.editor.is_none(),
            "unsafe original text must not enter the plain editor"
        );
        assert!(
            matches!(&ui.overlay, Some(Overlay::Text { text, .. }) if text.contains("\\u001b"))
        );
        assert_eq!(ui.nodes[0].raw, raw);
        assert!(app.store.drafts().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn confirmed_check_preserves_newer_draft_and_updates_editor_base() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap();
        let app = App::new(store, None, true);
        let mut ui = Ui::new(HashSet::new());
        ui.nodes.push(Node {
            raw: block(),
            depth: 0,
            expanded: false,
            loaded: false,
        });
        ui.edit_block(&app).await.unwrap();
        ui.editor.as_mut().unwrap().state = EditorState::new(Lines::from("B"));
        ui.persist_editor(&app).await.unwrap();
        let id = ui.editor.as_ref().unwrap().draft_id.clone();
        let mut confirmed = block();
        confirmed["paragraph"]["rich_text"][0]["text"]["content"] = json!("A");
        ui.receive(&app, Message { generation: None, write: false, result: Ok(Payload::Info {
            title: "Сверка операции".into(),
            value: json!({"operation":{"state":"confirmed","base":{"draft_id":id},"response":confirmed},"remote":confirmed,"matches_local":true}),
        }) }).await.unwrap();
        assert!(
            ui.drafts.contains(&id),
            "confirmation of A must preserve newer B"
        );
        assert_eq!(ui.local_text.get(&id).map(String::as_str), Some("B"));
        assert_eq!(ui.nodes[0].raw, confirmed);
        let Target::Block(base) = &ui.editor.as_ref().unwrap().target else {
            panic!()
        };
        assert_eq!(base, &confirmed);
        assert_eq!(ui.editor.as_ref().unwrap().text(), "B");
        assert_eq!(
            app.store.draft(&id).await.unwrap().unwrap().local["text"],
            "B"
        );
        ui.receive(
            &app,
            Message {
                generation: None,
                write: true,
                result: Ok(Payload::Saved {
                    results: vec![(id.clone(), confirmed)],
                    quit: false,
                    offline: false,
                    error: None,
                }),
            },
        )
        .await
        .unwrap();
        assert!(ui.drafts.contains(&id));
        assert_eq!(ui.local_text.get(&id).map(String::as_str), Some("B"));
    }

    #[tokio::test]
    async fn searches_open_data_sources_and_latest_request_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            Store::open(&dir.path().join("state.sqlite3"), Scope::new("p", "w", "i")).unwrap();
        let app = App::new(store, None, true);
        let source: DataSourceId = "c02fc1d3-db8b-45c5-a222-27595b15aea7".parse().unwrap();
        let schema = json!({"object":"data_source","id":source,"title":[{"text":{"content":"Задачи"}}],
            "properties":{"Name":{"id":"title","type":"title","title":{}}}});
        app.store
            .put_snapshot(
                "resource",
                &format!("data_sources/{source}"),
                schema.clone(),
                true,
            )
            .await
            .unwrap();
        app.store.put_snapshot("list", &json!(["search",{"query":"Задачи"},[]]).to_string(),
            json!({"results":[schema,page(),{"object":"database","id":source,"title":[]}],"has_more":false}), true).await.unwrap();
        app.store
            .put_snapshot(
                "list",
                &json!([format!("data_sources/{source}/query"), {}, []]).to_string(),
                json!({"results":[],"has_more":false}),
                true,
            )
            .await
            .unwrap();
        let mut ui = Ui::new(HashSet::new());
        let (tx, mut rx) = mpsc::unbounded_channel();
        for command in ["search Задачи", "local Задачи"] {
            ui.command(&app, &tx, command).await.unwrap();
            assert!(ui.accept(rx.recv().await.unwrap()));
            let selected = ui
                .nav
                .iter()
                .position(|item| item.source == Some(source))
                .unwrap();
            assert!(ui.nav[selected].id.is_none());
            assert!(ui.nav[selected].title.contains("Задачи"));
            if command.starts_with("search") {
                assert_eq!(
                    ui.nav[2].id,
                    Some(page()["id"].as_str().unwrap().parse().unwrap())
                );
                assert!(ui.nav[3].id.is_none() && ui.nav[3].source.is_none());
                assert!(ui.nav[3].title.contains("не поддерживается"));
            }
            ui.nav_selected = selected;
            ui.event(
                &app,
                &tx,
                Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            )
            .await
            .unwrap();
            assert!(ui.accept(rx.recv().await.unwrap()));
            assert_eq!(ui.grid.as_ref().unwrap().source, source);
        }
        let generation = ui.generation;
        ui.command(&app, &tx, "search Задачи").await.unwrap();
        ui.command(&app, &tx, "local Ничего").await.unwrap();
        assert_eq!(ui.generation, generation + 2);
        let mut results = vec![rx.recv().await.unwrap(), rx.recv().await.unwrap()];
        results.sort_by_key(|message| std::cmp::Reverse(message.generation));
        let mut results = results.into_iter();
        assert!(ui.accept(results.next().unwrap()));
        assert_eq!(ui.nav.len(), 1);
        assert!(!ui.accept(results.next().unwrap()));
        assert_eq!(ui.nav.len(), 1);
        assert!(ui.nav[0].title.contains("Локально"));
    }

    #[test]
    fn data_source_title_comes_from_title_array() {
        let source = json!({"object":"data_source","title":[{"text":{"content":"Задачи"}}],
            "properties":{"Name":{"type":"title","title":{}}}});
        assert_eq!(content::page_title(&source), "Задачи");
        let mut database = source.clone();
        database["object"] = json!("database");
        assert_eq!(content::page_title(&database), "Задачи");
        assert_eq!(content::page_title(&page()), "Кириллица 😀");
    }

    #[test]
    fn board_grouping_and_property_inputs_preserve_values() {
        let source = "b55c9c91-384d-452b-81db-d1ef79372b75".parse().unwrap();
        let schema = json!({"title":[],"properties":{"Title":{"type":"title","id":"title"},"Status":{"type":"status","id":"s","status":{"options":[{"id":"a","name":"Todo"},{"id":"b","name":"Done"}],"groups":[{"name":"Open","option_ids":["a"]},{"name":"Closed","option_ids":["b"]}]}}}});
        let row =
            json!({"properties":{"Status":{"type":"status","status":{"id":"b","name":"Done"}}}});
        let mut grid = make_grid(
            source,
            schema,
            vec![row.clone()],
            true,
            None,
            (Value::Null, Value::Null),
            None,
        )
        .unwrap();
        grid.by_group = true;
        let columns = board_columns(&grid);
        assert_eq!(row_column(&grid, &row, &columns), 1);
        assert_eq!(columns[0].ids[0], "a");
        grid.rows[0]["properties"]["Title"] = json!({"type":"title","title":[{"text":{"content":"Длинное название задачи должно переноситься целиком КОНЕЦ"}}]});
        let mut ui = Ui::new(HashSet::new());
        ui.grid = Some(grid);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &mut ui, true)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("КОНЕЦ"));
        assert!(property_input("relation", "[{\"id\":\"wrong\"}]").is_err());
        let rich = json!([{"type":"mention","mention":{"type":"user","user":{"id":"b55c9c91-384d-452b-81db-d1ef79372b75"}},"annotations":{"bold":true}}]);
        assert_eq!(
            property_input("rich_text", &pretty(&rich)).unwrap()["rich_text"],
            rich
        );
    }
}
