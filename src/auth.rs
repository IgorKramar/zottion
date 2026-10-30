use crate::{Error, Result, store::fingerprint};
use fs2::FileExt;
use keyring::v1::{Entry, Error as KeyringError};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{File, OpenOptions},
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{Instant, sleep, timeout_at},
};
use uuid::Uuid;

pub struct Options {
    pub profile: String,
    pub workspace: Option<String>,
    pub offline: bool,
    pub timeout: Duration,
}
/// No Debug: token-bearing state must never enter diagnostic logs.
pub struct Resolved {
    pub workspace: String,
    pub identity: String,
    pub token: Option<String>,
    pub state_dir: PathBuf,
}
#[derive(Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}
#[derive(Default, Serialize, Deserialize)]
struct Profile {
    selected_workspace: String,
    #[serde(default)]
    workspaces: BTreeMap<String, Workspace>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Workspace {
    identity: String,
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    keyring: bool,
    #[serde(default)]
    last_identity: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Credential {
    access_token: String,
    // Empty OAuth fields identify an internal API token; never refresh/revoke it via a server.
    refresh_token: String,
    workspace_id: String,
    bot_id: String,
    identity: String,
    server: String,
}
struct Paths {
    config: PathBuf,
    state_dir: PathBuf,
}
fn paths() -> Result<Paths> {
    let config_dir = std::env::var_os("ZOTTION_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::config_dir().map(|p| p.join("zottion")))
        .ok_or_else(|| Error::new(1, "Не найден каталог настроек"))?;
    let state_dir = std::env::var_os("ZOTTION_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|p| p.join("zottion")))
        .ok_or_else(|| Error::new(1, "Не найден каталог состояния"))?;
    Ok(Paths {
        config: config_dir.join("config.toml"),
        state_dir,
    })
}
fn validate(options: &Options) -> Result<()> {
    let profile = &options.profile;
    if profile.is_empty()
        || profile.len() > 128
        || profile == "."
        || profile == ".."
        || !profile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(Error::input(
            "Имя профиля: 1–128 букв, цифр, точек, '-' или '_'",
        ));
    }
    if options
        .workspace
        .as_ref()
        .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
    {
        return Err(Error::input("Некорректный workspace"));
    }
    if options.timeout.is_zero() {
        return Err(Error::input("Срок запроса должен быть больше нуля"));
    }
    Ok(())
}
fn read_config(path: &Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            toml::from_str(&text).map_err(|_| Error::input("Некорректный config.toml zottion"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(error) => Err(error.into()),
    }
}
fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}
struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}
async fn lock(path: PathBuf) -> Result<Lock> {
    tokio::task::spawn_blocking(move || {
        private_dir(
            path.parent()
                .ok_or_else(|| Error::input("У блокировки нет каталога"))?,
        )?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Lock(file)),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(Error::new(6, "Другой процесс изменяет профиль входа"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await
    .map_err(|_| Error::new(1, "Рабочий поток входа остановлен"))?
}
fn write_config(path: &Path, config: &Config) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::input("У настроек нет каталога"))?;
    private_dir(parent)?;
    let temp = parent.join(format!(".config-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        let text =
            toml::to_string(config).map_err(|_| Error::new(1, "Не удалось сохранить настройки"))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
fn workspace(config: &Config, options: &Options) -> String {
    options
        .workspace
        .clone()
        .or_else(|| {
            config
                .profiles
                .get(&options.profile)
                .map(|p| p.selected_workspace.clone())
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| "default".into())
}
fn entry(profile: &str, workspace: &str) -> Result<Entry> {
    Entry::new(
        "zottion",
        &fingerprint(&json!([profile, workspace]).to_string()),
    )
    .map_err(|_| keyring_error())
}
fn keyring_error() -> Error {
    Error::new(
        3,
        "Системное хранилище токенов недоступно; проверьте Secret Service/Keychain. Открытый файл не используется",
    )
}
async fn load_credential(profile: String, workspace: String) -> Result<Option<Credential>> {
    tokio::task::spawn_blocking(move || match entry(&profile, &workspace)?.get_password() {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|_| Error::new(3, "Повреждена запись токенов; выполните новый вход")),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(_) => Err(keyring_error()),
    })
    .await
    .map_err(|_| keyring_error())?
}
async fn save_credential(profile: String, workspace: String, credential: Credential) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let raw = serde_json::to_string(&credential)?;
        entry(&profile, &workspace)?
            .set_password(&raw)
            .map_err(|_| keyring_error())
    })
    .await
    .map_err(|_| keyring_error())?
}
async fn forget_credential(profile: String, workspace: String) -> Result<()> {
    tokio::task::spawn_blocking(
        move || match entry(&profile, &workspace)?.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(_) => Err(keyring_error()),
        },
    )
    .await
    .map_err(|_| keyring_error())?
}
fn env_token() -> Option<String> {
    std::env::var("NOTION_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
}
fn http(timeout: Duration) -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .map_err(|_| Error::new(1, "Не удалось создать HTTP-клиент входа"))
}
fn server_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).map_err(|_| Error::input("Некорректный URL OAuth-сервиса"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https"
            || url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")))
    {
        return Err(Error::input(
            "OAuth-сервис требует HTTPS; HTTP разрешён только для loopback",
        ));
    }
    if url.path() != "/" && !url.path().is_empty() {
        return Err(Error::input("Укажите корневой URL OAuth-сервиса"));
    }
    url.set_path("/");
    Ok(url)
}
fn login_server(
    explicit: Option<&str>,
    environment: Option<&str>,
    remembered: Option<&str>,
) -> Result<Option<Url>> {
    explicit
        .or(environment)
        .or(remembered)
        .map(server_url)
        .transpose()
}
fn notion_cli_error() -> Error {
    Error::new(
        3,
        "Не удалось выполнить вход через официальный Notion CLI (ntn). Установка: https://developers.notion.com/cli/get-started/installation; повторите вход или используйте auth login --token-stdin",
    )
}
fn check_notion_cli_status(status: ExitStatus) -> Result<()> {
    let code = status.code();
    #[cfg(unix)]
    let code = {
        use std::os::unix::process::ExitStatusExt;
        code.or_else(|| status.signal().map(|signal| 128 + signal))
    };
    match code {
        Some(0) => Ok(()),
        Some(130) => Err(Error::new(130, "Вход отменён")),
        _ => Err(notion_cli_error()),
    }
}
fn notion_cli_command(executable: &OsStr, workspace: Option<&str>) -> Command {
    let mut command = Command::new(executable);
    command
        .kill_on_drop(true)
        .stdin(Stdio::inherit())
        .env("NOTION_KEYRING", "1")
        .env("NOTION_ENV", "prod");
    for name in [
        "NOTION_API_TOKEN",
        "NOTION_BASE_URL",
        "NOTION_API_BASE_URL",
        "NOTION_WORKERS_CONFIG_FILE",
        "NOTION_WORKSPACE_ID",
    ] {
        command.env_remove(name);
    }
    if let Some(workspace) = workspace {
        command.env("NOTION_WORKSPACE_ID", workspace);
    }
    command
}
fn helper_stdout() -> Result<Stdio> {
    // ntn diagnostics must not corrupt zottion's versioned JSON on stdout.
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        std::io::stderr()
            .as_fd()
            .try_clone_to_owned()
            .map(Stdio::from)
            .map_err(|_| notion_cli_error())
    }
    #[cfg(not(unix))]
    {
        Err(Error::unsupported(
            "Браузерный вход через Notion CLI поддерживается на Linux/macOS",
        ))
    }
}
struct LoginTerminal(String);
impl LoginTerminal {
    fn capture() -> Result<Self> {
        let output = std::process::Command::new("/bin/stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .stderr(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(Error::new(1, "Не удалось сохранить режим терминала"));
        }
        let mode = String::from_utf8(output.stdout)
            .map_err(|_| Error::new(1, "Некорректный режим терминала"))?;
        Ok(Self(mode.trim().to_owned()))
    }
}
impl Drop for LoginTerminal {
    fn drop(&mut self) {
        // A killed helper cannot restore the terminal itself.
        let _ = std::process::Command::new("/bin/stty")
            .arg(&self.0)
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = crossterm::execute!(std::io::stderr(), crossterm::cursor::Show);
    }
}
async fn notion_cli_token(
    executable: &OsStr,
    options: &Options,
    no_browser: bool,
) -> Result<String> {
    let deadline = Instant::now() + options.timeout;
    // Local workspace aliases are not native Notion workspace IDs.
    let native_workspace = options
        .workspace
        .as_deref()
        .filter(|value| Uuid::parse_str(value).is_ok());
    let _terminal = if !no_browser && std::io::stdin().is_terminal() {
        Some(LoginTerminal::capture()?)
    } else {
        None
    };
    let steps: &[&[&str]] = if no_browser {
        &[&["login", "--no-browser"], &["login", "poll"]]
    } else {
        &[&["login"]]
    };
    for args in steps {
        let status = timeout_at(
            deadline,
            notion_cli_command(executable, native_workspace)
                .args(*args)
                .stdout(helper_stdout()?)
                .stderr(Stdio::inherit())
                .status(),
        )
        .await
        .map_err(|_| Error::new(6, "Срок входа через Notion CLI истёк; повторите auth login"))?
        .map_err(|_| notion_cli_error())?;
        check_notion_cli_status(status)?;
    }
    let mut child = notion_cli_command(executable, native_workspace)
        .args(["auth", "token", "--plain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| notion_cli_error())?;
    let token = timeout_at(deadline, async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(notion_cli_error)?
            .take(8193)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| notion_cli_error())?;
        if bytes.len() > 8192 {
            return Err(notion_cli_error());
        }
        check_notion_cli_status(child.wait().await.map_err(|_| notion_cli_error())?)?;
        let token = String::from_utf8(bytes).map_err(|_| notion_cli_error())?;
        if token.trim().is_empty() || !token.trim().bytes().all(|b| b.is_ascii_graphic()) {
            return Err(notion_cli_error());
        }
        Ok(token)
    })
    .await
    .map_err(|_| Error::new(6, "Срок входа через Notion CLI истёк; повторите auth login"))??;
    Ok(token)
}
fn random() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
async fn response_json(response: reqwest::Response) -> Result<Value> {
    let status = response.status();
    if !status.is_success() {
        return Err(Error::new(
            if status.as_u16() == 401 || status.as_u16() == 403 {
                3
            } else {
                6
            },
            "OAuth-сервис отклонил запрос; проверьте вход и доступность сервиса",
        ));
    }
    response
        .json()
        .await
        .map_err(|_| Error::new(6, "Некорректный ответ OAuth-сервиса"))
}
async fn remember(
    config_path: &Path,
    profile: &str,
    workspace: &str,
    value: Workspace,
) -> Result<()> {
    let _lock = lock(config_path.with_extension("lock")).await?;
    let mut config = read_config(config_path)?;
    let selected = config.profiles.entry(profile.into()).or_default();
    selected.selected_workspace = workspace.into();
    selected.workspaces.insert(workspace.into(), value);
    write_config(config_path, &config)
}
fn select_environment(config: &mut Config, profile: &str, workspace: &str, identity: &str) {
    let selected = config.profiles.entry(profile.into()).or_default();
    selected.selected_workspace = workspace.into();
    let stored = selected
        .workspaces
        .entry(workspace.into())
        .or_insert_with(|| Workspace {
            identity: identity.into(),
            server: None,
            keyring: false,
            last_identity: None,
        });
    stored.last_identity = Some(identity.into());
}
pub async fn resolve(options: &Options) -> Result<Resolved> {
    validate(options)?;
    let paths = paths()?;
    let config = read_config(&paths.config)?;
    let workspace = workspace(&config, options);
    if let Some(token) = env_token() {
        let identity = fingerprint(&token);
        let _config_lock = lock(paths.config.with_extension("lock")).await?;
        let mut config = read_config(&paths.config)?;
        select_environment(&mut config, &options.profile, &workspace, &identity);
        write_config(&paths.config, &config)?;
        return Ok(Resolved {
            workspace,
            identity,
            token: Some(token),
            state_dir: paths.state_dir,
        });
    }
    let stored = config
        .profiles
        .get(&options.profile)
        .and_then(|p| p.workspaces.get(&workspace));
    if options.offline {
        return Ok(Resolved {
            workspace,
            identity: stored
                .map(|v| {
                    v.last_identity
                        .clone()
                        .unwrap_or_else(|| v.identity.clone())
                })
                .unwrap_or_else(|| "signed-out".into()),
            token: None,
            state_dir: paths.state_dir,
        });
    }
    if stored.is_some_and(|value| value.keyring) {
        let credential = load_credential(options.profile.clone(), workspace.clone())
            .await?
            .ok_or_else(|| Error::new(3, "Токены отсутствуют; выполните auth login"))?;
        if stored.and_then(|value| value.last_identity.as_ref()) != Some(&credential.identity) {
            remember(
                &paths.config,
                &options.profile,
                &workspace,
                Workspace {
                    identity: credential.identity.clone(),
                    last_identity: Some(credential.identity.clone()),
                    server: (!credential.server.is_empty()).then(|| credential.server.clone()),
                    keyring: true,
                },
            )
            .await?;
        }
        return Ok(Resolved {
            workspace,
            identity: credential.identity,
            token: Some(credential.access_token),
            state_dir: paths.state_dir,
        });
    }
    Ok(Resolved {
        workspace,
        identity: "signed-out".into(),
        token: None,
        state_dir: paths.state_dir,
    })
}
async fn ensure_keyring() -> Result<()> {
    tokio::task::spawn_blocking(|| {
        Entry::store_status()
            .as_ref()
            .map(|_| ())
            .map_err(|_| keyring_error())
    })
    .await
    .map_err(|_| keyring_error())?
}
pub async fn login(options: &Options, server: Option<&str>, no_browser: bool) -> Result<Value> {
    validate(options)?;
    if options.offline {
        return Err(Error::input("Вход недоступен с --offline"));
    }
    let paths = paths()?;
    let config = read_config(&paths.config)?;
    let selected_workspace = workspace(&config, options);
    let remembered = config
        .profiles
        .get(&options.profile)
        .and_then(|profile| profile.workspaces.get(&selected_workspace))
        .and_then(|workspace| workspace.server.as_deref());
    let environment = match std::env::var("ZOTTION_AUTH_SERVER") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => {
            return Err(Error::input(
                "ZOTTION_AUTH_SERVER должен быть строкой UTF-8",
            ));
        }
    };
    let Some(server) = login_server(server, environment.as_deref(), remembered)? else {
        if env_token().is_some() {
            return Err(Error::input(
                "Уберите NOTION_TOKEN из окружения перед браузерным входом",
            ));
        }
        if options
            .workspace
            .as_ref()
            .is_some_and(|value| Uuid::parse_str(value).is_err())
        {
            return Err(Error::input(
                "Для браузерного входа --workspace должен быть реальным UUID workspace Notion",
            ));
        }
        // Keep helper selection and credential destination fixed throughout the login.
        let selected_options = Options {
            profile: options.profile.clone(),
            workspace: Some(selected_workspace),
            offline: options.offline,
            timeout: options.timeout,
        };
        let _lock = lock(paths.state_dir.join("auth-notion-cli.lock")).await?;
        let token = notion_cli_token(
            OsStr::new("ntn"),
            &selected_options,
            no_browser || !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal(),
        )
        .await?;
        return login_token(&selected_options, token).await;
    };
    let _lock = lock(
        paths
            .state_dir
            .join(format!("auth-{}.lock", fingerprint(&options.profile))),
    )
    .await?;
    // Initialize keyring before receiving tokens; never substitute a plaintext file.
    ensure_keyring().await?;
    let client = http(options.timeout)?;
    let verifier = random();
    let challenge = fingerprint(&verifier);
    let session = response_json(
        client
            .post(server.join("sessions").unwrap())
            .json(&json!({"challenge":challenge}))
            .send()
            .await
            .map_err(|_| Error::new(6, "Не удалось начать сеанс входа"))?,
    )
    .await?;
    let id = session["id"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| Error::new(6, "Сервис не вернул ID сеанса"))?;
    let authorize = Url::parse(
        session["authorize_url"]
            .as_str()
            .ok_or_else(|| Error::new(6, "Нет URL входа"))?,
    )
    .map_err(|_| Error::new(6, "Некорректный URL входа"))?;
    if authorize.scheme() != "https"
        || authorize.host_str() != Some("api.notion.com")
        || authorize.path() != "/v1/oauth/authorize"
        || !authorize.username().is_empty()
        || authorize.password().is_some()
    {
        return Err(Error::new(6, "OAuth-сервис вернул недопустимый URL входа"));
    }
    eprintln!("Откройте для входа: {authorize}");
    if !no_browser && webbrowser::open(authorize.as_str()).is_err() {
        eprintln!("Браузер не открыт; используйте URL выше.");
    }
    let deadline = Instant::now() + options.timeout.min(Duration::from_secs(300));
    let endpoint = server.join(&format!("sessions/{id}/result")).unwrap();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::new(
                6,
                "Срок входа истёк; начните auth login заново (при необходимости увеличьте --timeout до 300)",
            ));
        }
        let response = client
            .post(endpoint.clone())
            .timeout(remaining)
            .json(&json!({"verifier":verifier}))
            .send()
            .await
            .map_err(|_| Error::new(6, "Соединение прервано во время входа; начните заново"))?;
        if response.status().as_u16() == 202 {
            sleep(Duration::from_secs(1).min(remaining)).await;
            continue;
        }
        let raw = response_json(response).await?;
        let credential = credential(raw, server.as_str(), None)?;
        if options
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace != &credential.workspace_id)
        {
            return Err(Error::input(
                "Вход выполнен в другой workspace; повторите без --workspace или укажите его реальный ID",
            ));
        }
        let workspace = credential.workspace_id.clone();
        let identity = credential.identity.clone();
        save_credential(options.profile.clone(), workspace.clone(), credential).await?;
        remember(
            &paths.config,
            &options.profile,
            &workspace,
            Workspace {
                last_identity: Some(identity.clone()),
                identity,
                server: Some(server.to_string()),
                keyring: true,
            },
        )
        .await?;
        return Ok(
            json!({"logged_in":true,"profile":options.profile,"workspace":workspace,"storage":"system_keyring"}),
        );
    }
}
pub async fn login_token(options: &Options, token: String) -> Result<Value> {
    validate(options)?;
    if options.offline {
        return Err(Error::input("Вход недоступен с --offline"));
    }
    if env_token().is_some() {
        return Err(Error::input(
            "Уберите NOTION_TOKEN из окружения перед сохранением API-токена в keychain",
        ));
    }
    let too_long = token.len() > 8192;
    let token = token.trim();
    if too_long || token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::input(
            "Ожидается один непустой API-токен длиной до 8192 байт",
        ));
    }
    ensure_keyring().await?;
    let user = crate::notion::Client::new(token.to_owned(), options.timeout)?
        .get("users/me")
        .await?;
    let bot_id = user["id"]
        .as_str()
        .filter(|id| !id.is_empty() && user["type"] == "bot")
        .ok_or_else(|| Error::new(3, "Notion не подтвердил API-токен connection"))?;
    let paths = paths()?;
    let _lock = lock(
        paths
            .state_dir
            .join(format!("auth-{}.lock", fingerprint(&options.profile))),
    )
    .await?;
    let config = read_config(&paths.config)?;
    let workspace = workspace(&config, options);
    let identity = fingerprint(token);
    save_credential(
        options.profile.clone(),
        workspace.clone(),
        Credential {
            access_token: token.to_owned(),
            refresh_token: String::new(),
            workspace_id: workspace.clone(),
            bot_id: bot_id.to_owned(),
            identity: identity.clone(),
            server: String::new(),
        },
    )
    .await?;
    remember(
        &paths.config,
        &options.profile,
        &workspace,
        Workspace {
            identity: identity.clone(),
            last_identity: Some(identity),
            server: None,
            keyring: true,
        },
    )
    .await?;
    Ok(
        json!({"logged_in":true,"verified":true,"profile":options.profile,"workspace":workspace,"storage":"system_keyring","auth_type":"api_token"}),
    )
}
fn oauth_server(credential: &Credential) -> Result<Url> {
    if credential.server.is_empty() {
        return Err(Error::unsupported(
            "Токен этого способа отзывается в Notion; для нового входа используйте auth login или auth login --token-stdin",
        ));
    }
    server_url(&credential.server)
}
fn credential(raw: Value, server: &str, prior: Option<&Credential>) -> Result<Credential> {
    let access = raw["access_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::new(3, "Сервис не вернул access token"))?;
    let refresh = raw["refresh_token"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::new(3, "Сервис не вернул refresh token"))?;
    let workspace = raw["workspace_id"]
        .as_str()
        .or_else(|| prior.map(|v| v.workspace_id.as_str()))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::new(3, "Сервис не вернул workspace ID"))?;
    let bot = raw["bot_id"]
        .as_str()
        .or_else(|| prior.map(|v| v.bot_id.as_str()))
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Error::new(3, "Сервис не вернул bot ID"))?;
    if prior.is_some_and(|prior| prior.workspace_id != workspace || prior.bot_id != bot) {
        return Err(Error::new(
            3,
            "При обновлении изменилась идентичность; выполните новый вход",
        ));
    }
    Ok(Credential {
        access_token: access.into(),
        refresh_token: refresh.into(),
        workspace_id: workspace.into(),
        bot_id: bot.into(),
        identity: prior
            .map(|v| v.identity.clone())
            .unwrap_or_else(|| fingerprint(&json!([workspace, bot]).to_string())),
        server: server.into(),
    })
}
pub async fn status(options: &Options) -> Result<Value> {
    let resolved = resolve(options).await?;
    let source = if env_token().is_some() {
        "environment"
    } else if resolved.token.is_some() {
        "system_keyring"
    } else {
        "none"
    };
    let verified = if !options.offline {
        if let Some(token) = resolved.token {
            crate::notion::Client::new(token, options.timeout)?
                .get("users/me")
                .await?;
            true
        } else {
            false
        }
    } else {
        false
    };
    Ok(
        json!({"logged_in":source!="none","profile":options.profile,"workspace":resolved.workspace,"source":source,"verified":verified,"offline":options.offline,"cached_identity":resolved.identity!="signed-out"}),
    )
}
pub async fn refresh(options: &Options) -> Result<Value> {
    validate(options)?;
    if options.offline {
        return Err(Error::input("Обновление токенов недоступно с --offline"));
    }
    if env_token().is_some() {
        return Err(Error::unsupported(
            "Токен окружения обновляется его владельцем; уберите NOTION_TOKEN для OAuth-профиля",
        ));
    }
    let paths = paths()?;
    let _lock = lock(
        paths
            .state_dir
            .join(format!("auth-{}.lock", fingerprint(&options.profile))),
    )
    .await?;
    let config = read_config(&paths.config)?;
    let workspace = workspace(&config, options);
    let prior = load_credential(options.profile.clone(), workspace.clone())
        .await?
        .ok_or_else(|| Error::new(3, "Выполните auth login"))?;
    let server = oauth_server(&prior)?;
    let raw = response_json(
        http(options.timeout)?
            .post(server.join("refresh").unwrap())
            .json(&json!({"refresh_token":prior.refresh_token}))
            .send()
            .await
            .map_err(|_| {
                Error::new(
                    3,
                    "Исход ротации токенов неизвестен; выполните новый auth login",
                )
            })?,
    )
    .await?;
    let rotated = credential(raw, server.as_str(), Some(&prior))?;
    save_credential(options.profile.clone(), workspace.clone(), rotated).await?;
    Ok(json!({"refreshed":true,"profile":options.profile,"workspace":workspace}))
}
pub async fn logout(options: &Options, revoke: bool) -> Result<Value> {
    validate(options)?;
    if env_token().is_some() {
        if revoke {
            return Err(Error::unsupported(
                "Токен окружения отзывается его владельцем в Notion",
            ));
        }
        return Ok(
            json!({"logged_out":false,"environment_token_still_active":true,"hint":"Удалите NOTION_TOKEN из окружения; OAuth-профили сохранены","drafts_preserved":true}),
        );
    }
    if revoke && options.offline {
        return Err(Error::input("Отзыв токена недоступен с --offline"));
    }
    let paths = paths()?;
    let _lock = lock(
        paths
            .state_dir
            .join(format!("auth-{}.lock", fingerprint(&options.profile))),
    )
    .await?;
    let config = read_config(&paths.config)?;
    let workspace = workspace(&config, options);
    let stored = config
        .profiles
        .get(&options.profile)
        .and_then(|p| p.workspaces.get(&workspace));
    if stored.is_some_and(|value| value.keyring) {
        if revoke {
            let prior = load_credential(options.profile.clone(), workspace.clone())
                .await?
                .ok_or_else(|| Error::new(3, "Запись токенов отсутствует"))?;
            let server = oauth_server(&prior)?;
            response_json(
                http(options.timeout)?
                    .post(server.join("revoke").unwrap())
                    .json(&json!({"token":prior.access_token}))
                    .send()
                    .await
                    .map_err(|_| {
                        Error::new(6, "Отзыв токена не подтверждён; локальная запись сохранена")
                    })?,
            )
            .await?;
        }
        forget_credential(options.profile.clone(), workspace.clone()).await?;
    }
    let _config_lock = lock(paths.config.with_extension("lock")).await?;
    let mut config = read_config(&paths.config)?;
    if let Some(stored) = config
        .profiles
        .get_mut(&options.profile)
        .and_then(|profile| profile.workspaces.get_mut(&workspace))
    {
        stored.keyring = false;
    }
    write_config(&paths.config, &config)?;
    Ok(
        json!({"logged_out":true,"profile":options.profile,"workspace":workspace,"revoked":revoke,"environment_token_still_active":env_token().is_some(),"drafts_preserved":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_server_precedence_and_validation() {
        let explicit = Some("https://explicit.example.com");
        let environment = Some("https://env.example.com");
        let remembered = Some("https://remembered.example.com");
        for (a, b, c, expected) in [
            (
                explicit,
                environment,
                remembered,
                "https://explicit.example.com/",
            ),
            (None, environment, remembered, "https://env.example.com/"),
            (None, None, remembered, "https://remembered.example.com/"),
        ] {
            assert_eq!(login_server(a, b, c).unwrap().unwrap().as_str(), expected);
        }
        assert!(login_server(None, None, None).unwrap().is_none());
        for invalid in [
            "",
            "https://app.notion.com/p/profile",
            "http://example.com",
            "https://user:password@example.com",
            "https://example.com/?token=secret",
        ] {
            let error = login_server(Some(invalid), environment, remembered).unwrap_err();
            assert_eq!(error.code, 2);
            assert!(!error.message.contains(invalid) || invalid.is_empty());
        }
        assert_eq!(
            login_server(Some("http://127.0.0.1:9393"), None, None)
                .unwrap()
                .unwrap()
                .as_str(),
            "http://127.0.0.1:9393/"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_helper_sequence_limits_environment_and_failure() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fake-ntn");
        let mut options = Options {
            profile: "test".into(),
            workspace: None,
            offline: false,
            timeout: Duration::from_secs(2),
        };
        let command = notion_cli_command(executable.as_os_str(), None);
        let environment: BTreeMap<_, _> = command.as_std().get_envs().collect();
        for name in [
            "NOTION_API_TOKEN",
            "NOTION_BASE_URL",
            "NOTION_API_BASE_URL",
            "NOTION_WORKERS_CONFIG_FILE",
            "NOTION_WORKSPACE_ID",
        ] {
            assert_eq!(environment[OsStr::new(name)], None);
        }
        assert_eq!(
            environment[OsStr::new("NOTION_ENV")],
            Some(OsStr::new("prod"))
        );
        assert_eq!(
            environment[OsStr::new("NOTION_KEYRING")],
            Some(OsStr::new("1"))
        );
        let script = |body: &str| {
            std::fs::write(
                &executable,
                format!("#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> \"$0.log\"\n{body}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        script("[ \"$*\" != 'auth token --plain' ] || printf synthetic-token");
        assert_eq!(
            notion_cli_token(executable.as_os_str(), &options, true)
                .await
                .unwrap(),
            "synthetic-token"
        );
        assert_eq!(
            std::fs::read_to_string(executable.with_extension("log")).unwrap(),
            "login --no-browser\nlogin poll\nauth token --plain\n"
        );
        std::fs::remove_file(executable.with_extension("log")).unwrap();
        assert_eq!(
            notion_cli_token(executable.as_os_str(), &options, false)
                .await
                .unwrap(),
            "synthetic-token"
        );
        assert_eq!(
            std::fs::read_to_string(executable.with_extension("log")).unwrap(),
            "login\nauth token --plain\n"
        );
        options.workspace = Some("default".into());
        script(
            "[ \"${NOTION_WORKSPACE_ID+x}\" = '' ]\n[ \"$*\" != 'auth token --plain' ] || printf synthetic-token",
        );
        assert_eq!(
            notion_cli_token(executable.as_os_str(), &options, true)
                .await
                .unwrap(),
            "synthetic-token"
        );
        options.workspace = None;
        for body in [
            "exit 17",
            "[ \"$*\" != 'login poll' ]",
            "[ \"$*\" != 'auth token --plain' ] || { printf private-export-error >&2; exit 17; }",
            "[ \"$*\" != 'auth token --plain' ] || printf '%8193s' x",
            "[ \"$*\" != 'auth token --plain' ] || printf 'two tokens'",
        ] {
            script(body);
            let error = notion_cli_token(executable.as_os_str(), &options, true)
                .await
                .unwrap_err();
            assert_eq!(error.code, 3);
            assert!(!error.message.contains("private-export-error"));
        }
        script("exit 130");
        assert_eq!(
            notion_cli_token(executable.as_os_str(), &options, true)
                .await
                .unwrap_err()
                .code,
            130
        );
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            check_notion_cli_status(ExitStatus::from_raw(2))
                .unwrap_err()
                .code,
            130
        );
        options.timeout = Duration::from_millis(150);
        script("/bin/sleep 0.1\n[ \"$*\" != 'auth token --plain' ] || printf synthetic-token");
        assert_eq!(
            notion_cli_token(executable.as_os_str(), &options, true)
                .await
                .unwrap_err()
                .code,
            6
        );
        assert_eq!(
            notion_cli_token(dir.path().join("missing").as_os_str(), &options, true)
                .await
                .unwrap_err()
                .code,
            3
        );
    }
    #[test]
    fn configuration_and_token_rotation_preserve_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings/config.toml");
        let mut config = Config::default();
        config.profiles.insert(
            "work".into(),
            Profile {
                selected_workspace: "workspace".into(),
                workspaces: BTreeMap::from([(
                    "workspace".into(),
                    Workspace {
                        identity: "identity".into(),
                        server: Some("https://auth.example.com/".into()),
                        keyring: true,
                        last_identity: None,
                    },
                )]),
            },
        );
        select_environment(&mut config, "work", "workspace", "env-identity");
        assert!(config.profiles["work"].workspaces["workspace"].keyring);
        assert_eq!(
            config.profiles["work"].workspaces["workspace"].identity,
            "identity"
        );
        assert_eq!(
            config.profiles["work"].workspaces["workspace"]
                .last_identity
                .as_deref(),
            Some("env-identity")
        );
        write_config(&path, &config).unwrap();
        let loaded = read_config(&path).unwrap();
        assert_eq!(
            loaded.profiles["work"].workspaces["workspace"].identity,
            "identity"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let prior=credential(json!({"access_token":"old","refresh_token":"refresh-old","workspace_id":"workspace","bot_id":"bot"}),"https://auth.example.com/",None).unwrap();
        let rotated = credential(
            json!({"access_token":"new","refresh_token":"refresh-new"}),
            &prior.server,
            Some(&prior),
        )
        .unwrap();
        assert_eq!(prior.identity, rotated.identity);
        assert_eq!(rotated.refresh_token, "refresh-new");
        assert!(
            credential(
                json!({"access_token":"new","refresh_token":"refresh-new","workspace_id":"other"}),
                &prior.server,
                Some(&prior)
            )
            .is_err()
        );
        assert!(server_url("http://evil.test").is_err());
        assert!(server_url("https://user:secret@auth.example.com").is_err());
        assert!(server_url("http://127.0.0.1:9393").is_ok());
        let api_token = Credential {
            access_token: "synthetic-api-token".into(),
            refresh_token: String::new(),
            workspace_id: "workspace".into(),
            bot_id: "bot".into(),
            identity: fingerprint("synthetic-api-token"),
            server: String::new(),
        };
        let loaded: Credential =
            serde_json::from_str(&serde_json::to_string(&api_token).unwrap()).unwrap();
        assert_eq!(loaded.identity, fingerprint("synthetic-api-token"));
        assert_eq!(oauth_server(&loaded).unwrap_err().code, 7);
        assert!(oauth_server(&prior).is_ok());
    }
    #[tokio::test]
    async fn api_token_login_rejects_offline_and_malformed_input_before_keychain() {
        let mut options = Options {
            profile: "default".into(),
            workspace: None,
            offline: true,
            timeout: Duration::from_secs(1),
        };
        assert_eq!(
            login_token(&options, "synthetic".into())
                .await
                .unwrap_err()
                .code,
            2
        );
        options.offline = false;
        for token in [
            String::new(),
            "two tokens".into(),
            "token\nsecond".into(),
            "x".repeat(8193),
            format!("x{}", " ".repeat(8192)),
        ] {
            assert_eq!(login_token(&options, token).await.unwrap_err().code, 2);
        }
    }
}
