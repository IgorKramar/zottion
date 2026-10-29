use crate::{Error, Result};
use fs2::FileExt;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::mpsc,
};
use tokio::sync::oneshot;
use uuid::Uuid;

pub fn fingerprint(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
#[derive(Clone, Debug)]
pub struct Scope {
    pub profile: String,
    pub workspace: String,
    pub credential: String,
}
impl Scope {
    pub fn new(profile: &str, workspace: &str, credential_identity: &str) -> Self {
        Self {
            profile: profile.into(),
            workspace: workspace.into(),
            credential: credential_identity.into(),
        }
    }
    fn key(&self) -> String {
        fingerprint(&json!([self.profile, self.workspace, self.credential]).to_string())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Prepared,
    Sending,
    Waiting,
    Confirmed,
    Conflict,
    Failed,
    Unknown,
}
impl OperationState {
    fn text(&self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Sending => "sending",
            Self::Waiting => "waiting",
            Self::Confirmed => "confirmed",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub base: Value,
    pub local: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    pub method: String,
    pub path: String,
    pub base: Value,
    pub local: Value,
    pub state: OperationState,
    pub response: Option<Value>,
}
type Job = Box<dyn FnOnce(&mut Connection) + Send>;
#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
    scope: String,
    lock_path: PathBuf,
}
pub struct WriteGuard {
    file: File,
}
impl Drop for WriteGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl Store {
    /// Initialization happens before the render loop. All subsequent SQLite work
    /// is queued on this connection's dedicated thread.
    pub fn open(path: &Path, scope: Scope) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let mut conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: u32 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > 1 {
            return Err(Error::new(
                1,
                "Локальная БД создана более новой версией приложения",
            ));
        }
        if version == 0 {
            tx.execute_batch(include_str!("../migrations/001_initial.sql"))?;
        }
        tx.commit()?;
        let (sender, receiver) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("zottion-store".into())
            .spawn(move || {
                for job in receiver {
                    job(&mut conn);
                }
            })?;
        Ok(Self {
            sender,
            scope: scope.key(),
            lock_path: parent.join(format!("write-{}.lock", fingerprint(&scope.profile))),
        })
    }
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, String) -> Result<T> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let scope = self.scope.clone();
        self.sender
            .send(Box::new(move |conn| {
                let _ = sender.send(f(conn, scope));
            }))
            .map_err(|_| Error::new(1, "Очередь хранилища закрыта"))?;
        receiver
            .await
            .map_err(|_| Error::new(1, "Рабочий поток хранилища остановлен"))?
    }
    pub async fn snapshot(&self, kind: &str, id: &str) -> Result<Option<Value>> {
        let (kind, id) = (kind.to_owned(), id.to_owned());
        self.run(move |conn, scope| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT raw FROM snapshots WHERE scope=?1 AND kind=?2 AND id=?3",
                    params![scope, kind, id],
                    |row| row.get(0),
                )
                .optional()?;
            raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
                .transpose()
        })
        .await
    }
    pub async fn snapshot_complete(&self, kind: &str, id: &str) -> Result<Option<bool>> {
        let (kind, id) = (kind.to_owned(), id.to_owned());
        self.run(move |conn, scope| {
            Ok(conn
                .query_row(
                    "SELECT complete FROM snapshots WHERE scope=?1 AND kind=?2 AND id=?3",
                    params![scope, kind, id],
                    |row| row.get(0),
                )
                .optional()?)
        })
        .await
    }
    pub async fn put_snapshot(
        &self,
        kind: &str,
        id: &str,
        raw: Value,
        complete: bool,
    ) -> Result<()> {
        self.put_snapshots(vec![(kind.to_owned(), id.to_owned(), raw, complete)])
            .await
    }
    pub async fn put_snapshots(&self, snapshots: Vec<(String, String, Value, bool)>) -> Result<()> {
        self.run(move |conn, scope| {
            let tx = conn.transaction()?;
            for (kind, id, raw, complete) in snapshots {
                put_snapshot(&tx, &scope, &kind, &id, &raw, complete)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn remove_snapshot(&self, kind: &str, id: &str) -> Result<()> {
        let (kind, id) = (kind.to_owned(), id.to_owned());
        self.run(move |conn, scope| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM snapshots WHERE scope=?1 AND kind=?2 AND id=?3",
                params![scope, kind, id],
            )?;
            tx.execute(
                "DELETE FROM search_index WHERE scope=?1 AND kind=?2 AND id=?3",
                params![scope, kind, id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn draft(&self, id: &str) -> Result<Option<Draft>> {
        let id = id.to_owned();
        self.run(move |conn, scope| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT base,local FROM drafts WHERE scope=?1 AND id=?2",
                    params![scope, id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            row.map(|(base, local)| {
                Ok(Draft {
                    id,
                    base: serde_json::from_str(&base)?,
                    local: serde_json::from_str(&local)?,
                })
            })
            .transpose()
        })
        .await
    }
    pub async fn put_draft(&self, id: &str, base: Value, local: Value) -> Result<()> {
        let id = id.to_owned();
        self.run(move|conn,scope|{conn.execute("INSERT INTO drafts(scope,id,base,local) VALUES(?1,?2,?3,?4) ON CONFLICT(scope,id) DO UPDATE SET base=excluded.base,local=excluded.local,updated=unixepoch()",params![scope,id,base.to_string(),local.to_string()])?;Ok(())}).await
    }
    pub async fn drafts(&self) -> Result<Vec<Draft>> {
        self.run(move |conn, scope| {
            let mut stmt = conn.prepare(
                "SELECT id,base,local FROM drafts WHERE scope=?1 ORDER BY updated DESC,id",
            )?;
            let rows = stmt.query_map([scope], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            let mut result = Vec::new();
            for row in rows {
                let (id, base, local) = row?;
                result.push(Draft {
                    id,
                    base: serde_json::from_str(&base)?,
                    local: serde_json::from_str(&local)?,
                });
            }
            Ok(result)
        })
        .await
    }
    pub async fn delete_draft(&self, id: &str) -> Result<()> {
        let id = id.to_owned();
        self.run(move |conn, scope| {
            conn.execute(
                "DELETE FROM drafts WHERE scope=?1 AND id=?2",
                params![scope, id],
            )?;
            Ok(())
        })
        .await
    }
    pub async fn prepare(
        &self,
        method: &str,
        path: &str,
        base: Value,
        local: Value,
    ) -> Result<Operation> {
        let operation = Operation {
            id: Uuid::new_v4().to_string(),
            method: method.into(),
            path: path.into(),
            base,
            local,
            state: OperationState::Prepared,
            response: None,
        };
        let saved = operation.clone();
        self.run(move|conn,scope|{conn.execute("INSERT INTO operations(scope,id,method,path,base,local,state) VALUES(?1,?2,?3,?4,?5,?6,'prepared')",params![scope,saved.id,saved.method,saved.path,saved.base.to_string(),saved.local.to_string()])?;Ok(())}).await?;
        Ok(operation)
    }
    pub async fn set_operation(
        &self,
        id: &str,
        state: OperationState,
        response: Option<Value>,
    ) -> Result<()> {
        let id = id.to_owned();
        self.run(move|conn,scope|{let count=conn.execute("UPDATE operations SET state=?1,response=?2,updated=unixepoch() WHERE scope=?3 AND id=?4",params![state.text(),response.map(|value|value.to_string()),scope,id])?;if count==0{return Err(Error::new(4,"Операция не найдена"));}Ok(())}).await
    }
    /// Подтверждение и удаление только отправленной версии — одна транзакция.
    pub async fn confirm_operation(
        &self,
        id: &str,
        response: Option<Value>,
        draft: Option<Draft>,
    ) -> Result<()> {
        let id = id.to_owned();
        self.run(move |conn, scope| {
            let tx = conn.transaction()?;
            let count = tx.execute(
                "UPDATE operations SET state='confirmed',response=?1,updated=unixepoch() WHERE scope=?2 AND id=?3",
                params![response.map(|value| value.to_string()), scope, id],
            )?;
            if count == 0 {
                return Err(Error::new(4, "Операция не найдена"));
            }
            if let Some(draft) = draft {
                tx.execute(
                    "DELETE FROM drafts WHERE scope=?1 AND id=?2 AND base=?3 AND local=?4",
                    params![scope, draft.id, draft.base.to_string(), draft.local.to_string()],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn operations(&self) -> Result<Vec<Operation>> {
        self.run(move|conn,scope|{let mut stmt=conn.prepare("SELECT id,method,path,base,local,state,response FROM operations WHERE scope=?1 ORDER BY updated DESC,id")?;
        let rows=stmt.query_map([scope],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?,row.get::<_,Option<String>>(6)?)))?;
        let mut result=Vec::new();for row in rows{let(id,method,path,base,local,state,response)=row?;result.push(Operation{id,method,path,base:serde_json::from_str(&base)?,local:serde_json::from_str(&local)?,state:serde_json::from_value(json!(state))?,response:response.map(|raw|serde_json::from_str(&raw)).transpose()?});}Ok(result)}).await
    }
    pub async fn operation(&self, id: &str) -> Result<Operation> {
        self.operations()
            .await?
            .into_iter()
            .find(|operation| operation.id == id)
            .ok_or_else(|| Error::new(4, "Операция не найдена"))
    }
    /// The OS releases this lock on process death. Recovery is safe only after
    /// acquisition; merely opening a second reader must not recover live writes.
    pub async fn acquire_write(&self) -> Result<WriteGuard> {
        let path = self.lock_path.clone();
        let guard = tokio::task::spawn_blocking(move || -> Result<WriteGuard> {
            let mut options = OpenOptions::new();
            options.create(true).read(true).write(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(path)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            loop {
                match file.try_lock_exclusive() {
                    Ok(()) => break,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(25))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        return Err(Error::new(6, "Другой процесс удерживает блокировку записи"));
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(WriteGuard { file })
        })
        .await
        .map_err(|_| Error::new(1, "Не удалось получить блокировку записи"))??;
        self.run(move|conn,scope|{conn.execute("UPDATE operations SET state='unknown',updated=unixepoch() WHERE scope=?1 AND state IN ('sending','waiting')",[scope])?;Ok(())}).await?;
        Ok(guard)
    }
    pub async fn clear_cache(&self) -> Result<()> {
        self.run(move |conn, scope| {
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM snapshots WHERE scope=?1", [&scope])?;
            tx.execute("DELETE FROM search_index WHERE scope=?1", [&scope])?;
            tx.commit()?;
            Ok(())
        })
        .await
    }
    pub async fn search(&self, query: &str) -> Result<Value> {
        let query = query.to_owned();
        self.run(move|conn,scope|{let mut stmt=conn.prepare("SELECT kind,id,snippet(search_index,3,'[',']','…',24) FROM search_index WHERE search_index MATCH ?1 AND scope=?2 AND kind NOT IN ('list','navigation','preferences') ORDER BY rank LIMIT 100")?;
        let rows=stmt.query_map(params![query,scope],|row|Ok(json!({"kind":row.get::<_,String>(0)?,"id":row.get::<_,String>(1)?,"snippet":row.get::<_,String>(2)?}))).map_err(|_|Error::input("Некорректный запрос FTS5"))?;
        let results=rows.collect::<std::result::Result<Vec<_>,_>>()?;
        let loaded:i64=conn.query_row("SELECT count(*) FROM snapshots WHERE scope=?1 AND kind NOT IN ('list','navigation','preferences')",[scope],|row|row.get(0))?;
        Ok(json!({"scope":"loaded_content","loaded_resources":loaded,"results":results}))}).await
    }
}
fn put_snapshot(
    tx: &rusqlite::Transaction<'_>,
    scope: &str,
    kind: &str,
    id: &str,
    raw: &Value,
    complete: bool,
) -> Result<()> {
    tx.execute("INSERT INTO snapshots(scope,kind,id,raw,complete) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(scope,kind,id) DO UPDATE SET raw=excluded.raw,complete=excluded.complete,updated=unixepoch()", params![scope, kind, id, raw.to_string(), complete])?;
    tx.execute(
        "DELETE FROM search_index WHERE scope=?1 AND kind=?2 AND id=?3",
        params![scope, kind, id],
    )?;
    tx.execute(
        "INSERT INTO search_index(scope,kind,id,text) VALUES(?1,?2,?3,?4)",
        params![scope, kind, id, searchable_text(raw)],
    )?;
    Ok(())
}
fn searchable_text(raw: &Value) -> String {
    match raw {
        Value::Object(map) => {
            if map.get("object").and_then(Value::as_str) == Some("block")
                || map.get("type").is_some_and(|value| {
                    crate::content::TEXT_BLOCKS.contains(&value.as_str().unwrap_or(""))
                })
            {
                return crate::content::block_text(raw);
            }
            let mut text = crate::content::page_title(raw);
            for (key, value) in map {
                if key == "children" || key == "results" || key == "blocks" {
                    text.push('\n');
                    text.push_str(&searchable_text(value));
                }
            }
            text
        }
        Value::Array(values) => values
            .iter()
            .map(searchable_text)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}
