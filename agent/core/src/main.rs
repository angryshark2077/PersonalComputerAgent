mod app;
mod collector_registry;
mod config;
mod event_factory;
mod event_sink;
mod lifecycle;
mod system_runtime;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let exit_code = match config::CommandConfig::parse(std::env::args_os()) {
        Ok(command) => app::execute(command).await,
        Err(message) => {
            eprintln!("pca-agentd: {message}");
            app::EXIT_USAGE
        }
    };
    std::process::ExitCode::from(exit_code)
}
