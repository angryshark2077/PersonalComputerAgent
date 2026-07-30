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

#[cfg(feature = "process-test-hooks")]
#[derive(Debug)]
pub(crate) struct ProcessTestFatalCleanupConfig {
    pub(crate) bridge_pid_ready: PathBuf,
    pub(crate) armed: PathBuf,
    pub(crate) release: PathBuf,
    pub(crate) cleanup_complete: PathBuf,
}

#[cfg(feature = "process-test-hooks")]
#[derive(Default)]
struct ProcessTestOptions {
    barrier_ready: Option<PathBuf>,
    barrier_release: Option<PathBuf>,
    fail_after_bridge_pid: Option<PathBuf>,
    fatal_armed: Option<PathBuf>,
    fatal_release: Option<PathBuf>,
    cleanup_complete: Option<PathBuf>,
}

#[cfg(feature = "process-test-hooks")]
impl ProcessTestOptions {
    fn parse_argument(
        &mut self,
        argument: &str,
        remaining: &mut impl Iterator<Item = OsString>,
    ) -> Result<bool, String> {
        let target = match argument {
            "--process-test-event-barrier-ready" if self.barrier_ready.is_none() => {
                &mut self.barrier_ready
            }
            "--process-test-event-barrier-release" if self.barrier_release.is_none() => {
                &mut self.barrier_release
            }
            "--process-test-fail-heartbeat-after-bridge-pid"
                if self.fail_after_bridge_pid.is_none() =>
            {
                &mut self.fail_after_bridge_pid
            }
            "--process-test-fatal-armed" if self.fatal_armed.is_none() => &mut self.fatal_armed,
            "--process-test-fatal-release" if self.fatal_release.is_none() => {
                &mut self.fatal_release
            }
            "--process-test-cleanup-complete" if self.cleanup_complete.is_none() => {
                &mut self.cleanup_complete
            }
            _ => return Ok(false),
        };
        *target = Some(PathBuf::from(remaining.next().ok_or_else(usage)?));
        Ok(true)
    }

    fn rebase(&mut self, original_root: &Path, canonical_root: &Path) -> Result<(), String> {
        for path in [
            &mut self.barrier_ready,
            &mut self.barrier_release,
            &mut self.fail_after_bridge_pid,
            &mut self.fatal_armed,
            &mut self.fatal_release,
            &mut self.cleanup_complete,
        ] {
            rebase_process_test_path(path, original_root, canonical_root)?;
        }
        Ok(())
    }

    fn reject_if_present(&self) -> Result<(), String> {
        if [
            &self.barrier_ready,
            &self.barrier_release,
            &self.fail_after_bridge_pid,
            &self.fatal_armed,
            &self.fatal_release,
            &self.cleanup_complete,
        ]
        .iter()
        .all(|path| path.is_none())
        {
            Ok(())
        } else {
            Err("process test options are valid only for run".to_owned())
        }
    }
}

#[derive(Debug)]
pub(crate) struct RunConfig {
    pub(crate) paths: RuntimePaths,
    pub(crate) bridge_executable: PathBuf,
    #[cfg(feature = "process-test-hooks")]
    pub(crate) process_test_barrier: Option<ProcessTestBarrierConfig>,
    #[cfg(feature = "process-test-hooks")]
    pub(crate) process_test_fatal_cleanup: Option<ProcessTestFatalCleanupConfig>,
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
        let mut process_test = ProcessTestOptions::default();

        while let Some(argument) = arguments.next() {
            #[cfg(feature = "process-test-hooks")]
            if let Some(argument) = argument.to_str() {
                if process_test.parse_argument(argument, &mut arguments)? {
                    continue;
                }
            }
            match argument.to_str() {
                Some("--runtime-root") if runtime_root.is_none() => {
                    runtime_root = Some(PathBuf::from(arguments.next().ok_or_else(usage)?));
                }
                _ => return Err(usage()),
            }
        }

        let explicit_root_path = runtime_root.clone();
        let explicit_root = explicit_root_path.is_some();
        let paths = match runtime_root {
            Some(root) => test_paths(&root)?,
            None => RuntimePaths::for_current_user().map_err(|_| "production root unavailable")?,
        };
        #[cfg(feature = "process-test-hooks")]
        if let Some(original_root) = explicit_root_path.as_deref() {
            process_test.rebase(original_root, &paths.root)?;
        }

        match command.as_str() {
            "run" => {
                #[cfg(feature = "process-test-hooks")]
                let process_test_barrier = barrier_config(
                    explicit_root,
                    &paths,
                    process_test.barrier_ready,
                    process_test.barrier_release,
                )?;
                #[cfg(feature = "process-test-hooks")]
                let process_test_fatal_cleanup = fatal_cleanup_config(
                    explicit_root,
                    &paths,
                    process_test.fail_after_bridge_pid,
                    process_test.fatal_armed,
                    process_test.fatal_release,
                    process_test.cleanup_complete,
                )?;
                let bridge_executable = bridge_executable(&paths, explicit_root)?;
                Ok(Self::Run(RunConfig {
                    paths,
                    bridge_executable,
                    #[cfg(feature = "process-test-hooks")]
                    process_test_barrier,
                    #[cfg(feature = "process-test-hooks")]
                    process_test_fatal_cleanup,
                }))
            }
            "health" => {
                #[cfg(feature = "process-test-hooks")]
                process_test.reject_if_present()?;
                Ok(Self::Health(paths))
            }
            "prepare-sleep" => {
                #[cfg(feature = "process-test-hooks")]
                process_test.reject_if_present()?;
                let _ = paths;
                Ok(Self::PrepareSleep)
            }
            _ => Err(usage()),
        }
    }
}

fn test_paths(root: &Path) -> Result<RuntimePaths, String> {
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
    let production = RuntimePaths::for_current_user()
        .map_err(|_| "production root unavailable")?
        .root;
    canonical_test_root(root, &production).map(RuntimePaths::under)
}

fn bridge_executable(paths: &RuntimePaths, explicit_root: bool) -> Result<PathBuf, String> {
    if explicit_root {
        Ok(paths
            .app_dir
            .join("PersonalComputerAgent.app/Contents/Resources/bin/PCAPlatformBridge"))
    } else {
        production_bridge_executable()
    }
}

fn canonical_test_root(root: &Path, production_root: &Path) -> Result<PathBuf, String> {
    let canonical_root = std::fs::canonicalize(root)
        .map_err(|_| "--runtime-root must name an existing non-production directory")?;
    if !canonical_root.is_dir() {
        return Err("--runtime-root must name an existing non-production directory".to_owned());
    }
    if let Ok(canonical_production) = std::fs::canonicalize(production_root) {
        if canonical_root == canonical_production
            || canonical_root.starts_with(canonical_production)
        {
            return Err(
                "--runtime-root cannot equal or descend from the production root".to_owned(),
            );
        }
    }
    Ok(canonical_root)
}

#[cfg(feature = "process-test-hooks")]
fn rebase_process_test_path(
    path: &mut Option<PathBuf>,
    original_root: &Path,
    canonical_root: &Path,
) -> Result<(), String> {
    if let Some(current) = path {
        let relative = current
            .strip_prefix(original_root)
            .map_err(|_| "process test paths must be below --runtime-root")?;
        *current = canonical_root.join(relative);
    }
    Ok(())
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
fn fatal_cleanup_config(
    explicit_root: bool,
    paths: &RuntimePaths,
    bridge_pid_ready: Option<PathBuf>,
    armed: Option<PathBuf>,
    release: Option<PathBuf>,
    cleanup_complete: Option<PathBuf>,
) -> Result<Option<ProcessTestFatalCleanupConfig>, String> {
    match (bridge_pid_ready, armed, release, cleanup_complete) {
        (None, None, None, None) => Ok(None),
        (Some(bridge_pid_ready), Some(armed), Some(release), Some(cleanup_complete))
            if explicit_root
                && bridge_pid_ready.parent() == Some(paths.run_dir.as_path())
                && armed.parent() == Some(paths.run_dir.as_path())
                && release.parent() == Some(paths.run_dir.as_path())
                && cleanup_complete.parent() == Some(paths.run_dir.as_path())
                && all_paths_are_distinct(&[
                    &bridge_pid_ready,
                    &armed,
                    &release,
                    &cleanup_complete,
                ]) =>
        {
            Ok(Some(ProcessTestFatalCleanupConfig {
                bridge_pid_ready,
                armed,
                release,
                cleanup_complete,
            }))
        }
        _ => Err("fatal cleanup test requires distinct runtime-root Run siblings".to_owned()),
    }
}

#[cfg(feature = "process-test-hooks")]
fn all_paths_are_distinct(paths: &[&Path]) -> bool {
    paths
        .iter()
        .enumerate()
        .all(|(index, path)| paths[index + 1..].iter().all(|other| path != other))
}

fn usage() -> String {
    "usage: pca-agentd <run|health|prepare-sleep> [--runtime-root <absolute-path>]".to_owned()
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::fs::symlink};

    use super::{canonical_test_root, CommandConfig};

    #[test]
    fn production_command_surface_is_exact() {
        for command in ["start", "status", "sleep", "run-extra"] {
            assert!(
                CommandConfig::parse([OsString::from("pca-agentd"), OsString::from(command)])
                    .is_err()
            );
        }
    }

    #[test]
    fn explicit_root_uses_its_canonical_spelling() {
        let directory = tempfile::tempdir().expect("temporary root parent");
        let production = directory.path().join("production");
        let actual = directory.path().join("actual-test-root");
        let alias = directory.path().join("test-root-alias");
        std::fs::create_dir(&production).expect("create synthetic production root");
        std::fs::create_dir(&actual).expect("create actual test root");
        symlink(&actual, &alias).expect("create test-root alias");

        assert_eq!(
            canonical_test_root(&alias, &production).expect("canonical non-production root"),
            actual.canonicalize().expect("canonical expected root")
        );
    }

    #[test]
    fn explicit_root_rejects_production_descendants_and_their_aliases() {
        let directory = tempfile::tempdir().expect("temporary root parent");
        let production = directory.path().join("production");
        let descendant = production.join("nested-test-root");
        let alias = directory.path().join("descendant-alias");
        std::fs::create_dir_all(&descendant).expect("create production descendant");
        symlink(&descendant, &alias).expect("create descendant alias");

        assert!(canonical_test_root(&descendant, &production).is_err());
        assert!(canonical_test_root(&alias, &production).is_err());
    }
}
