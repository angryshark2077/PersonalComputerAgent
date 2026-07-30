mod app;
mod config;
mod lifecycle;

#[tokio::main]
async fn main() {
    let exit_code = match config::CommandConfig::parse(std::env::args_os()) {
        Ok(command) => app::execute(command).await,
        Err(message) => {
            eprintln!("pca-agentd: {message}");
            app::EXIT_USAGE
        }
    };
    std::process::exit(exit_code);
}
