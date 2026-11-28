use std::sync::Arc;

use fleetlayer_tmux::{plugin, terminal};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("FLEETLAYER_LOG"))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("terminal") {
        let Some(a) = terminal::parse_args(&args[1..]) else {
            eprintln!("usage: fleetlayer-tmux terminal <observe|input|control> <%pane> <tmux> [-L <name> | -S <path>]");
            std::process::exit(2);
        };
        if let Err(e) = terminal::run(a).await {
            eprintln!("fleetlayer-tmux terminal: {e}");
            std::process::exit(1);
        }
        return;
    }
    fleetlayer_plugin::serve_stdio(Arc::new(plugin::Shared(Arc::new(plugin::Plugin::default())))).await;
}
