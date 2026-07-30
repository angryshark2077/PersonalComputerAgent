use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use pca_agent_runtime::RuntimePaths;

#[cfg(feature = "process-test-hooks")]
#[derive(Debug)]
pub(crate) struct ProcessTestBarrierConfig {
    pub(crate) ready: PathBuf,
    pub(crate) release: PathBuf,
}

#[derive(Debug)]
pub(crate) struct RunConfig {
    pub(crate) paths: RuntimePaths,
    pub(crate) bridge_executable: PathBuf,
    #[cfg(feature = "process-test-hooks")]
    pub(crate) process_test_barrier: Option<ProcessTestBarrierConfig>,
}

#[derive(Debug)]
pub(crate) enum CommandConfig {
    Run(RunConfig),
    Health(RuntimePaths),
    PrepareSleep,
}

impl CommandConfig {
    pub(crate) fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut arguments = arguments.into_iter();
        let _program = arguments.next();
        let command = arguments
            .next()
            .and_then(|argument| argument.into_string().ok())
            .ok_or_else(usage)?;

        let mut runtime_root = None;
        #[cfg(feature = "process-test-hooks")]
        let mut barrier_ready = None;
        #[cfg(feature = "process-test-hooks")]
        let mut barrier_release = None;

        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--runtime-root") if runtime_root.is_none() => {
                    runtime_root = Some(PathBuf::from(arguments.next().ok_or_else(usage)?));
                }
                #[cfg(feature = "process-test-hooks")]
                Some("--process-test-event-barrier-ready") if barrier_ready.is_none() => {
                    barrier_ready = Some(PathBuf::from(arguments.next().ok_or_else(usage)?));
                }
                #[cfg(feature = "process-test-hooks")]
                Some("--process-test-event-barrier-release") if barrier_release.is_none() => {
                    barrier_release = Some(PathBuf::from(arguments.next().ok_or_else(usage)?));
                }
                _ => return Err(usage()),
            }
        }

        let explicit_root = runtime_root.is_some();
        let paths = match runtime_root {
            Some(root) => test_paths(root)?,
            None => RuntimePaths::for_current_user().map_err(|_| "production root unavailable")?,
        };

        match command.as_str() {
            "run" => {
                #[cfg(feature = "process-test-hooks")]
                let process_test_barrier =
                    barrier_config(explicit_root, &paths, barrier_ready, barrier_release)?;
                let bridge_executable = if explicit_root {
                    paths
                        .app_dir
                        .join("PersonalComputerAgent.app/Contents/Resources/bin/PCAPlatformBridge")
                } else {
                    production_bridge_executable()?
                };
                Ok(Self::Run(RunConfig {
                    paths,
                    bridge_executable,
                    #[cfg(feature = "process-test-hooks")]
                    process_test_barrier,
                }))
            }
            "health" => {
                #[cfg(feature = "process-test-hooks")]
                reject_barrier_options(barrier_ready.as_ref(), barrier_release.as_ref())?;
                Ok(Self::Health(paths))
            }
            "prepare-sleep" => {
                #[cfg(feature = "process-test-hooks")]
                reject_barrier_options(barrier_ready.as_ref(), barrier_release.as_ref())?;
                let _ = paths;
                Ok(Self::PrepareSleep)
            }
            _ => Err(usage()),
        }
    }
}

fn test_paths(root: PathBuf) -> Result<RuntimePaths, String> {
    if !root.is_absolute()
        || root.as_os_str().is_empty()
        || root == Path::new("/")
        || root.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
    {
        return Err("--runtime-root must be a safe absolute non-production path".to_owned());
    }
    let canonical_root = std::fs::canonicalize(&root)
        .map_err(|_| "--runtime-root must name an existing non-production directory")?;
    let resolves_to_production = RuntimePaths::for_current_user()
        .ok()
        .and_then(|production| std::fs::canonicalize(production.root).ok())
        .is_some_and(|production| production == canonical_root);
    if resolves_to_production {
        return Err("--runtime-root cannot override the production root".to_owned());
    }
    Ok(RuntimePaths::under(root))
}

fn production_bridge_executable() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|_| "cannot resolve agent executable")?;
    let parent = executable
        .parent()
        .ok_or_else(|| "cannot resolve agent bundle directory".to_owned())?;
    Ok(parent.join("PCAPlatformBridge"))
}

#[cfg(feature = "process-test-hooks")]
fn barrier_config(
    explicit_root: bool,
    paths: &RuntimePaths,
    ready: Option<PathBuf>,
    release: Option<PathBuf>,
) -> Result<Option<ProcessTestBarrierConfig>, String> {
    match (ready, release) {
        (None, None) => Ok(None),
        (Some(ready), Some(release))
            if explicit_root
                && ready.parent() == Some(paths.run_dir.as_path())
                && release.parent() == Some(paths.run_dir.as_path())
                && ready != release =>
        {
            Ok(Some(ProcessTestBarrierConfig { ready, release }))
        }
        _ => Err("process test barrier requires distinct runtime-root Run siblings".to_owned()),
    }
}

#[cfg(feature = "process-test-hooks")]
fn reject_barrier_options(
    ready: Option<&PathBuf>,
    release: Option<&PathBuf>,
) -> Result<(), String> {
    if ready.is_none() && release.is_none() {
        Ok(())
    } else {
        Err("process test barrier is valid only for run".to_owned())
    }
}

fn usage() -> String {
    "usage: pca-agentd <run|health|prepare-sleep> [--runtime-root <absolute-path>]".to_owned()
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::CommandConfig;

    #[test]
    fn production_command_surface_is_exact() {
        for command in ["start", "status", "sleep", "run-extra"] {
            assert!(
                CommandConfig::parse([OsString::from("pca-agentd"), OsString::from(command)])
                    .is_err()
            );
        }
    }
}
