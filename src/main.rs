use clap::Parser;
use std::io::{self, Write};
use zottion::{
    Error,
    cli::{Cli, envelope},
};
#[tokio::main]
async fn main() {
    let json = std::env::args_os().any(|a| a == "--json");
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            if json && code != 0 {
                fail(Error::input(error.to_string()), true, "default", "", false);
            } else {
                let _ = error.print();
                std::process::exit(code);
            }
        }
    };
    let result = tokio::select! {result=cli.run()=>result,_=tokio::signal::ctrl_c()=>Err(Error::new(130,"Отменено; неотправленные черновики сохранены"))};
    match result {
        Ok((Some(data), workspace)) => {
            let value = envelope(data, &cli.profile, &workspace, cli.offline, false);
            if let Err(error) = print(&value, cli.json) {
                fail(error, cli.json, &cli.profile, &workspace, cli.offline);
            }
        }
        Ok((None, _)) => {}
        Err(error) => fail(
            error,
            cli.json,
            &cli.profile,
            cli.workspace.as_deref().unwrap_or("default"),
            cli.offline,
        ),
    }
}
fn print(value: &serde_json::Value, json: bool) -> Result<(), Error> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if json {
        serde_json::to_writer(&mut out, value)?;
    } else {
        serde_json::to_writer_pretty(&mut out, value)?;
    }
    writeln!(out)?;
    Ok(())
}
fn fail(error: Error, json: bool, profile: &str, workspace: &str, offline: bool) -> ! {
    if let Some(data) = error.partial {
        let _ = print(&envelope(data, profile, workspace, offline, true), json);
    }
    if json {
        eprintln!(
            "{}",
            serde_json::json!({"schema_version":1,"error":{"code":error.code,"message":error.message}})
        );
    } else {
        eprintln!(
            "Ошибка: {}",
            zottion::content::terminal_text(&error.message)
        );
    }
    std::process::exit(i32::from(error.code));
}
