use crate::{
    Error, Result,
    app::{App, text_runs},
    auth,
    content::{self, BlockId, DataSourceId, DatabaseId, PageId, ViewId},
    notion::Client,
    store::{Scope, Store},
};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{
    io::{self, IsTerminal, Read, Write},
    time::Duration,
};

#[derive(Parser)]
#[command(
    name = "zottion",
    version,
    about = "Заметки и задачи Notion в терминале"
)]
pub struct Cli {
    #[arg(long, global = true)]
    pub json: bool,
    #[arg(long, global = true, default_value = "default")]
    pub profile: String,
    #[arg(long, global = true)]
    pub workspace: Option<String>,
    #[arg(long,global=true,default_value="30",value_parser=clap::value_parser!(u64).range(1..=3600))]
    pub timeout: u64,
    #[arg(long, global = true)]
    pub offline: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}
#[derive(Subcommand)]
pub enum Command {
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Page {
        #[command(subcommand)]
        command: PageCommand,
    },
    Block {
        #[command(subcommand)]
        command: BlockCommand,
    },
    DataSource {
        #[command(subcommand)]
        command: SourceCommand,
    },
    View {
        #[command(subcommand)]
        command: ViewCommand,
    },
    Search {
        query: String,
        #[arg(long, value_enum, default_value = "titles")]
        scope: SearchScope,
        #[command(flatten)]
        list: ListArgs,
    },
    Draft {
        #[command(subcommand)]
        command: DraftCommand,
    },
    Operation {
        #[command(subcommand)]
        command: OperationCommand,
    },
    Open {
        id: PageId,
    },
    Completions {
        shell: clap_complete::Shell,
    },
}
#[derive(Subcommand)]
pub enum AuthCommand {
    Login {
        #[arg(long)]
        server: String,
        #[arg(long)]
        no_browser: bool,
    },
    Status,
    Refresh,
    Logout {
        #[arg(long)]
        revoke: bool,
    },
}
#[derive(Args, Default)]
pub struct ListArgs {
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub cursor: Option<String>,
}
#[derive(Args)]
pub struct TextArgs {
    #[arg(long, conflicts_with = "file")]
    pub text: Option<String>,
    #[arg(long)]
    pub file: Option<String>,
}
#[derive(Subcommand)]
pub enum PageCommand {
    Get {
        id: PageId,
        #[arg(long)]
        content: bool,
        #[command(flatten)]
        list: ListArgs,
    },
    Create {
        #[arg(
            long,
            required_unless_present = "data_source",
            conflicts_with = "data_source"
        )]
        parent: Option<PageId>,
        #[arg(long)]
        data_source: Option<DataSourceId>,
        #[arg(long)]
        title: String,
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        properties_file: Option<String>,
    },
    Title {
        id: PageId,
        title: String,
    },
    Move {
        id: PageId,
        #[arg(long)]
        parent: PageId,
    },
    Trash {
        id: PageId,
        #[arg(long)]
        yes: bool,
    },
    Restore {
        id: PageId,
    },
    Property {
        #[command(subcommand)]
        command: PropertyCommand,
    },
    Favorite {
        id: PageId,
        #[arg(long)]
        remove: bool,
    },
    Pin {
        id: PageId,
        #[arg(long)]
        remove: bool,
    },
    Recent,
}
#[derive(Subcommand)]
pub enum PropertyCommand {
    Get {
        id: PageId,
        #[arg(long)]
        property: String,
        #[command(flatten)]
        list: ListArgs,
    },
    Set {
        id: PageId,
        #[arg(long)]
        property: String,
        #[arg(long, conflicts_with = "file", required_unless_present = "file")]
        value_json: Option<String>,
        #[arg(long)]
        file: Option<String>,
    },
}
#[derive(Subcommand)]
pub enum BlockCommand {
    Get {
        id: BlockId,
    },
    Children {
        id: BlockId,
        #[command(flatten)]
        list: ListArgs,
    },
    Update {
        id: BlockId,
        #[command(flatten)]
        text: TextArgs,
        #[arg(long,conflicts_with_all=["text","file"])]
        patch_file: Option<String>,
    },
    Append {
        id: BlockId,
        #[arg(long, default_value = "paragraph")]
        kind: String,
        #[command(flatten)]
        text: TextArgs,
        #[arg(long,conflicts_with_all=["text","file"])]
        blocks_file: Option<String>,
    },
    Format {
        id: BlockId,
        #[arg(long, default_value = "0")]
        start: usize,
        #[arg(long)]
        end: Option<usize>,
        #[arg(long)]
        attribute: String,
        #[arg(long)]
        value: String,
    },
    Check {
        id: BlockId,
        #[arg(long,action=clap::ArgAction::Set)]
        checked: bool,
    },
    Trash {
        id: BlockId,
        #[arg(long)]
        yes: bool,
    },
    Restore {
        id: BlockId,
    },
    Move {
        id: BlockId,
        #[arg(long)]
        parent: BlockId,
    },
}
#[derive(Subcommand)]
pub enum SourceCommand {
    Get {
        id: DataSourceId,
    },
    Query {
        id: DataSourceId,
        #[arg(long,conflicts_with_all=["filter_file","sort_file"])]
        file: Option<String>,
        #[arg(long)]
        filter_file: Option<String>,
        #[arg(long)]
        sort_file: Option<String>,
        #[command(flatten)]
        list: ListArgs,
    },
}
#[derive(Subcommand)]
pub enum ViewCommand {
    Get {
        id: ViewId,
    },
    List {
        #[arg(long)]
        database: DatabaseId,
        #[command(flatten)]
        list: ListArgs,
    },
    Query {
        id: ViewId,
        #[arg(long)]
        all: bool,
    },
}
#[derive(ValueEnum, Clone, Copy)]
pub enum SearchScope {
    Titles,
    Local,
}
#[derive(Subcommand)]
pub enum DraftCommand {
    List,
    Show {
        id: String,
    },
    Apply {
        id: String,
    },
    Resolve {
        id: String,
        #[arg(long,value_parser=["local","remote"])]
        choice: String,
    },
    Drop {
        id: String,
        #[arg(long)]
        yes: bool,
    },
    ClearCache,
}
#[derive(Subcommand)]
pub enum OperationCommand {
    List,
    Show {
        id: String,
    },
    Check {
        id: String,
    },
    Resolve {
        id: String,
        #[arg(long,action=clap::ArgAction::Set)]
        confirmed: bool,
        #[arg(long)]
        yes: bool,
    },
}

impl Cli {
    pub fn options(&self) -> auth::Options {
        auth::Options {
            profile: self.profile.clone(),
            workspace: self.workspace.clone(),
            offline: self.offline,
            timeout: Duration::from_secs(self.timeout),
        }
    }
    pub async fn run(&self) -> Result<(Option<Value>, String)> {
        if let Some(Command::Completions { shell }) = &self.command {
            clap_complete::generate(*shell, &mut Self::command(), "zottion", &mut io::stdout());
            return Ok((None, String::new()));
        }
        let options = self.options();
        if let Some(Command::Auth { command }) = &self.command {
            let value = match command {
                AuthCommand::Login { server, no_browser } => {
                    auth::login(&options, server, *no_browser).await?
                }
                AuthCommand::Status => auth::status(&options).await?,
                AuthCommand::Refresh => auth::refresh(&options).await?,
                AuthCommand::Logout { revoke } => auth::logout(&options, *revoke).await?,
            };
            let workspace = value["workspace"].as_str().unwrap_or("default").to_owned();
            return Ok((Some(value), workspace));
        }
        if self.json
            && (self.command.is_none() || matches!(self.command, Some(Command::Open { .. })))
        {
            return Err(Error::input("Для --json укажите CLI-подкоманду"));
        }
        if (self.command.is_none() || matches!(self.command, Some(Command::Open { .. })))
            && (!io::stdin().is_terminal() || !io::stdout().is_terminal())
        {
            return Err(Error::input(
                "TUI требует терминал; используйте page, block или search --json",
            ));
        }
        let resolved = auth::resolve(&options).await?;
        let workspace = resolved.workspace.clone();
        let store = Store::open(
            &resolved.state_dir.join("state.sqlite3"),
            Scope::new(&self.profile, &workspace, &resolved.identity),
        )?;
        let client = resolved
            .token
            .map(|token| Client::new(token, options.timeout))
            .transpose()?;
        let app = App::new(store, client, self.offline);
        let value = match &self.command {
            None => {
                crate::tui::run(app, None).await?;
                return Ok((None, workspace));
            }
            Some(Command::Open { id }) => {
                crate::tui::run(app, Some(*id)).await?;
                return Ok((None, workspace));
            }
            Some(Command::Page { command }) => self.page(&app, command).await?,
            Some(Command::Block { command }) => self.block(&app, command).await?,
            Some(Command::DataSource { command }) => match command {
                SourceCommand::Get { id } => app.data_source(*id).await?,
                SourceCommand::Query {
                    id,
                    file,
                    filter_file,
                    sort_file,
                    list,
                } => {
                    if filter_file.as_deref() == Some("-") && sort_file.as_deref() == Some("-") {
                        return Err(Error::input(
                            "stdin можно использовать только для одного файла",
                        ));
                    }
                    let mut body = if let Some(file) = file {
                        json_file(file)?
                    } else {
                        json!({})
                    };
                    if !body.is_object() {
                        return Err(Error::input("Тело query должно быть объектом"));
                    }
                    if let Some(file) = filter_file {
                        body["filter"] = json_file(file)?;
                    }
                    if let Some(file) = sort_file {
                        body["sorts"] = json_file(file)?;
                    }
                    if let Some(cursor) = &list.cursor {
                        body["start_cursor"] = json!(cursor);
                    }
                    app.query(*id, body, list.all).await?
                }
            },
            Some(Command::View { command }) => match command {
                ViewCommand::Get { id } => app.resource(&format!("views/{id}")).await?,
                ViewCommand::List { database, list } => {
                    app.views(*database, list.all, list.cursor.clone()).await?
                }
                ViewCommand::Query { id, all } => app.query_view(*id, *all).await?,
            },
            Some(Command::Search { query, scope, list }) => {
                app.search(
                    query,
                    matches!(scope, SearchScope::Local),
                    list.all,
                    list.cursor.clone(),
                )
                .await?
            }
            Some(Command::Draft { command }) => match command {
                DraftCommand::List => serde_json::to_value(app.store.drafts().await?)?,
                DraftCommand::Show { id } => app.draft_versions(id).await?,
                DraftCommand::Apply { id } => app.apply_draft(id).await?,
                DraftCommand::Resolve { id, choice } => app.resolve_draft(id, choice).await?,
                DraftCommand::Drop { id, yes } => {
                    confirm(*yes, self.json, "Удалить локальный черновик?")?;
                    if app.store.draft(id).await?.is_none() {
                        return Err(Error::new(4, "Черновик не найден"));
                    }
                    app.store.delete_draft(id).await?;
                    json!({"deleted":id})
                }
                DraftCommand::ClearCache => {
                    app.store.clear_cache().await?;
                    json!({"cache_cleared":true,"drafts_and_operations_preserved":true})
                }
            },
            Some(Command::Operation { command }) => match command {
                OperationCommand::List => serde_json::to_value(app.store.operations().await?)?,
                OperationCommand::Show { id } => {
                    serde_json::to_value(app.store.operation(id).await?)?
                }
                OperationCommand::Check { id } => app.check_operation(id).await?,
                OperationCommand::Resolve { id, confirmed, yes } => {
                    confirm(
                        *yes,
                        self.json,
                        "Подтвердить результат ручной сверки операции?",
                    )?;
                    app.acknowledge_operation(id, *confirmed).await?
                }
            },
            Some(Command::Auth { .. } | Command::Completions { .. }) => unreachable!(),
        };
        Ok((Some(value), workspace))
    }
    async fn page(&self, app: &App, command: &PageCommand) -> Result<Value> {
        match command {
            PageCommand::Get { id, content, list } => {
                let page = app.page(*id).await?;
                if *content {
                    let blocks = app
                        .children(BlockId(id.0), list.all, list.cursor.clone())
                        .await?;
                    Ok(json!({"page":page,"blocks":blocks}))
                } else {
                    Ok(page)
                }
            }
            PageCommand::Create {
                parent,
                data_source,
                title,
                file,
                properties_file,
            } => {
                if let Some(parent) = parent {
                    if properties_file.is_some() {
                        return Err(Error::input("properties-file применим к data source"));
                    }
                    let text = file.as_deref().map(read_file).transpose()?;
                    app.create_page(*parent, title, text.as_deref()).await
                } else {
                    if file.is_some() {
                        return Err(Error::input(
                            "Для задачи задайте свойства через --properties-file",
                        ));
                    }
                    let props = properties_file
                        .as_deref()
                        .map(json_file)
                        .transpose()?
                        .unwrap_or_else(|| json!({}));
                    app.create_task(
                        data_source.ok_or_else(|| Error::input("Нужен родитель страницы"))?,
                        title,
                        props,
                    )
                    .await
                }
            }
            PageCommand::Title { id, title } => {
                let base = app.page(*id).await?;
                let key = base["properties"]
                    .as_object()
                    .and_then(|props| props.iter().find(|(_, prop)| prop["type"] == "title"))
                    .map(|(key, _)| key.clone())
                    .unwrap_or_else(|| "title".into());
                app.patch_page_from(base, json!({"properties":{key:{"title":text_runs(title)}}}))
                    .await
            }
            PageCommand::Move { id, parent } => app.move_page(*id, *parent).await,
            PageCommand::Trash { id, yes } => {
                confirm(*yes, self.json, "Переместить страницу в корзину?")?;
                app.patch_page(*id, json!({"in_trash":true})).await
            }
            PageCommand::Restore { id } => app.patch_page(*id, json!({"in_trash":false})).await,
            PageCommand::Property { command } => match command {
                PropertyCommand::Get { id, property, list } => {
                    app.property(*id, property, list.all, list.cursor.clone())
                        .await
                }
                PropertyCommand::Set {
                    id,
                    property,
                    value_json,
                    file,
                } => {
                    let value = if let Some(value) = value_json {
                        serde_json::from_str(value)?
                    } else {
                        json_file(
                            file.as_deref()
                                .ok_or_else(|| Error::input("Нужно значение свойства"))?,
                        )?
                    };
                    app.patch_page(*id, json!({"properties":{property:value}}))
                        .await
                }
            },
            PageCommand::Favorite { id, remove } | PageCommand::Pin { id, remove } => {
                let kind = if matches!(command, PageCommand::Pin { .. }) {
                    "roots"
                } else {
                    "favorites"
                };
                if *remove {
                    app.forget(kind, &id.to_string()).await
                } else {
                    let page = app.page(*id).await?;
                    app.remember(kind, &id.to_string(), &content::page_title(&page))
                        .await
                }
            }
            PageCommand::Recent => app.navigation("recent").await,
        }
    }
    async fn block(&self, app: &App, command: &BlockCommand) -> Result<Value> {
        match command {
            BlockCommand::Get { id } => app.block(*id).await,
            BlockCommand::Children { id, list } => {
                app.children(*id, list.all, list.cursor.clone()).await
            }
            BlockCommand::Update {
                id,
                text,
                patch_file,
            } => {
                if let Some(file) = patch_file {
                    app.patch_block(*id, json_file(file)?).await
                } else {
                    let base = app.block(*id).await?;
                    app.save_block(base, text_input(text)?).await
                }
            }
            BlockCommand::Append {
                id,
                kind,
                text,
                blocks_file,
            } => {
                let blocks = if let Some(file) = blocks_file {
                    json_file(file)?
                        .as_array()
                        .cloned()
                        .ok_or_else(|| Error::input("blocks-file должен содержать массив"))?
                } else {
                    vec![content::new_block(kind, &text_input(text)?)?]
                };
                app.append(*id, blocks).await
            }
            BlockCommand::Format {
                id,
                start,
                end,
                attribute,
                value,
            } => {
                let base = app.block(*id).await?;
                let value = if matches!(attribute.as_str(), "link" | "color") {
                    if value == "null" {
                        Value::Null
                    } else {
                        json!(value)
                    }
                } else {
                    serde_json::from_str(value)?
                };
                let patch = content::format_patch(&base, *start, *end, attribute, value)?;
                let path = format!("blocks/{id}");
                app.mutate("PATCH", &path, Some(&path), base, patch).await
            }
            BlockCommand::Check { id, checked } => {
                let block = app.block(*id).await?;
                if content::block_type(&block) != "to_do" {
                    return Err(Error::unsupported("checked применим к to_do"));
                }
                let path = format!("blocks/{id}");
                app.mutate(
                    "PATCH",
                    &path,
                    Some(&path),
                    block,
                    json!({"to_do":{"checked":checked}}),
                )
                .await
            }
            BlockCommand::Trash { id, yes } => {
                confirm(*yes, self.json, "Переместить блок в корзину?")?;
                app.patch_block(*id, json!({"in_trash":true})).await
            }
            BlockCommand::Restore { id } => app.patch_block(*id, json!({"in_trash":false})).await,
            BlockCommand::Move { .. } => Err(Error::unsupported(
                "Перенос обычного блока не подтверждён официальным API; копирование с удалением не выполняется",
            )),
        }
    }
}
pub fn envelope(
    data: Value,
    profile: &str,
    workspace: &str,
    offline: bool,
    partial: bool,
) -> Value {
    let cursor = data
        .get("next_cursor")
        .or_else(|| data.get("blocks").and_then(|b| b.get("next_cursor")))
        .cloned()
        .unwrap_or(Value::Null);
    let more = data["has_more"] == true || data["blocks"]["has_more"] == true;
    json!({"schema_version":1,"data":data,"meta":{"profile":profile,"workspace":workspace,"offline":offline,"complete":!partial&&!more,"partial":partial,"next_cursor":cursor}})
}
fn text_input(args: &TextArgs) -> Result<String> {
    match (&args.text, &args.file) {
        (Some(text), None) => Ok(text.clone()),
        (None, Some(file)) => read_file(file),
        _ => Err(Error::input("Укажите --text или --file (stdin: --file -)")),
    }
}
fn read_file(path: &str) -> Result<String> {
    let mut result = String::new();
    let input: Box<dyn Read> = if path == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(
            std::fs::File::open(path)
                .map_err(|_| Error::input("Не удалось прочитать входной файл"))?,
        )
    };
    input
        .take(10 * 1024 * 1024 + 1)
        .read_to_string(&mut result)
        .map_err(|_| Error::input("Вход должен быть UTF-8"))?;
    if result.len() > 10 * 1024 * 1024 {
        return Err(Error::input("Вход превышает 10 MiB"));
    }
    Ok(result)
}
fn json_file(path: &str) -> Result<Value> {
    Ok(serde_json::from_str(&read_file(path)?)?)
}
fn confirm(yes: bool, json: bool, prompt: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if json || !io::stdin().is_terminal() {
        return Err(Error::input("Для этой операции в скрипте нужен --yes"));
    }
    eprint!("{prompt} [y/д/N] ");
    io::stderr().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if matches!(input.trim(), "y" | "Y" | "д" | "Д") {
        Ok(())
    } else {
        Err(Error::new(130, "Отменено"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_contract() {
        Cli::command().debug_assert();
        assert!(
            Cli::try_parse_from([
                "zottion",
                "block",
                "update",
                "b55c9c91-384d-452b-81db-d1ef79372b75",
                "--file",
                "-",
                "--json"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["zottion", "--timeout", "0", "search", "x"]).is_err());
        let value = envelope(
            json!({"results":[],"has_more":true,"next_cursor":"x"}),
            "p",
            "w",
            false,
            false,
        );
        assert_eq!(value["meta"]["complete"], false);
        assert_eq!(value["meta"]["next_cursor"], "x");
    }
}
