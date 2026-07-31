mod app;
// Task 7 stages private Core modules before Task 8 wires production identity and lifecycle.
// Keep the allowance at the module boundary so Task 8 can remove it in one place.
#[cfg_attr(not(test), allow(dead_code))]
mod collector_registry;
mod config;
#[cfg_attr(not(test), allow(dead_code))]
mod event_factory;
#[cfg_attr(not(test), allow(dead_code))]
mod event_sink;
mod lifecycle;
#[cfg_attr(not(test), allow(dead_code))]
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
