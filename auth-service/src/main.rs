use zottion_auth_service::{Config, router};
#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
async fn run() -> Result<(), String> {
    let config = Config {
        client_id: std::env::var("NOTION_CLIENT_ID").map_err(|_| "Требуется NOTION_CLIENT_ID")?,
        client_secret: std::env::var("NOTION_CLIENT_SECRET")
            .map_err(|_| "Требуется NOTION_CLIENT_SECRET")?,
        callback_url: std::env::var("ZOTTION_OAUTH_CALLBACK_URL")
            .map_err(|_| "Требуется ZOTTION_OAUTH_CALLBACK_URL")?,
    };
    let app = router(config)?;
    let bind = std::env::var("ZOTTION_OAUTH_BIND").unwrap_or_else(|_| "127.0.0.1:9393".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .map_err(|_| "Не удалось открыть порт OAuth-сервиса")?;
    eprintln!(
        "OAuth-сервис запущен. Размещайте его за HTTPS reverse proxy без журналирования query и тел запросов."
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|_| "Ошибка OAuth-сервиса".into())
}
