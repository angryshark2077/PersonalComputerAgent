mod app;
mod config;
mod lifecycle;

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
