mod app;
mod backend;
mod builtin;
mod cli;
mod env;
mod issue;
mod model;
mod router;
mod slash;
mod team;
mod tui;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = cli::Cli::parse();
    match args.cmd {
        cli::Cmd::Init => {
            cli::init()?;
        }
        cli::Cmd::Work { dir, team } => {
            let (mut model, warns) = cli::build(dir, team)?;
            // 预检警告作为系统消息写进第一个议题的时间线
            for w in warns {
                model.issues[0].timeline.push(model::ChatMsg {
                    ts: chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
                    author: "系统".into(),
                    text: format!("预检:{w}"),
                    is_system: true,
                });
            }
            app::run(model).await?;
        }
    }
    Ok(())
}
