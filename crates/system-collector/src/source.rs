use crate::{SystemMetricsSource, SystemSampleError, SystemSampleErrorKind};
use pca_domain::{AgentCpuMemory, CpuMemorySample, DiskSample, DiskScope, HostCpuMemory};
use std::{
    path::{Path, PathBuf},
    thread,
    time::Instant,
};
use sysinfo::{
    get_current_pid, CpuRefreshKind, Disks, MemoryRefreshKind, Pid, ProcessRefreshKind,
    ProcessesToUpdate, RefreshKind, System, MINIMUM_CPU_UPDATE_INTERVAL,
};

const LOW_SPACE_THRESHOLD_BYTES: u64 = 2_147_483_648;

#[derive(Debug, Clone, PartialEq, Eq)]
struct MountSnapshot {
    mount_point: PathBuf,
    total_bytes: u64,
    available_bytes: u64,
}

pub struct SysinfoMetricsSource {
    data_dir: PathBuf,
    disks: Disks,
    pid: Pid,
    system: System,
    last_cpu_refresh: Instant,
}

impl SysinfoMetricsSource {
    /// Creates the minimal sysinfo-backed system metric source.
    ///
    /// # Errors
    ///
    /// Returns a typed fatal error when the current process identifier cannot be read.
    pub fn new(data_dir: PathBuf) -> Result<Self, SystemSampleError> {
        let pid = get_current_pid().map_err(|message| {
            SystemSampleError::new(
                SystemSampleErrorKind::Fatal,
                "SYSTEM_CURRENT_PROCESS_UNAVAILABLE",
                message,
            )
        })?;
        let mut system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        refresh_current_process(&mut system, pid);

        Ok(Self {
            data_dir,
            disks: Disks::new_with_refreshed_list(),
            pid,
            system,
            last_cpu_refresh: Instant::now(),
        })
    }
}

impl SystemMetricsSource for SysinfoMetricsSource {
    fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, SystemSampleError> {
        let elapsed = self.last_cpu_refresh.elapsed();
        if let Some(remaining) = MINIMUM_CPU_UPDATE_INTERVAL.checked_sub(elapsed) {
            thread::sleep(remaining);
        }
        let sample_window = self.last_cpu_refresh.elapsed();

        self.system.refresh_cpu_usage();
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        let refreshed = refresh_current_process(&mut self.system, self.pid);
        self.last_cpu_refresh = Instant::now();

        if refreshed == 0 {
            return Err(SystemSampleError::new(
                SystemSampleErrorKind::Retryable,
                "SYSTEM_CURRENT_PROCESS_MISSING",
                "the current Agent process was not returned by sysinfo",
            ));
        }

        let logical_cpu_count = u32::try_from(self.system.cpus().len()).map_err(|_| {
            SystemSampleError::new(
                SystemSampleErrorKind::Fatal,
                "SYSTEM_LOGICAL_CPU_COUNT_INVALID",
                "logical CPU count does not fit the event contract",
            )
        })?;
        if logical_cpu_count == 0 {
            return Err(SystemSampleError::new(
                SystemSampleErrorKind::Unsupported,
                "SYSTEM_LOGICAL_CPU_UNAVAILABLE",
                "sysinfo returned zero logical CPUs",
            ));
        }

        let process = self.system.process(self.pid).ok_or_else(|| {
            SystemSampleError::new(
                SystemSampleErrorKind::Retryable,
                "SYSTEM_CURRENT_PROCESS_MISSING",
                "the current Agent process disappeared during sampling",
            )
        })?;
        let host_cpu = checked_percentage(f64::from(self.system.global_cpu_usage()), "host CPU")?;
        let agent_cpu = normalize_agent_cpu(f64::from(process.cpu_usage()), logical_cpu_count)?;
        let host = HostCpuMemory::try_new(
            host_cpu,
            self.system.total_memory(),
            self.system.used_memory(),
        )
        .map_err(|error| domain_sample_error(&error))?;
        let agent = AgentCpuMemory::try_new(agent_cpu, process.memory())
            .map_err(|error| domain_sample_error(&error))?;
        let sample_window_ms = u64::try_from(sample_window.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);

        CpuMemorySample::try_new(sample_window_ms, logical_cpu_count, host, agent)
            .map_err(|error| domain_sample_error(&error))
    }

    fn sample_disk(&mut self) -> Result<DiskSample, SystemSampleError> {
        self.disks.refresh(true);
        let mounts = self
            .disks
            .list()
            .iter()
            .map(|disk| MountSnapshot {
                mount_point: disk.mount_point().to_path_buf(),
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
            })
            .collect::<Vec<_>>();
        let selected = select_data_volume(&self.data_dir, &mounts)?;
        disk_sample_from_mount(selected)
    }
}

fn refresh_current_process(system: &mut System, pid: Pid) -> usize {
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    )
}

fn select_data_volume<'a>(
    data_dir: &Path,
    mounts: &'a [MountSnapshot],
) -> Result<&'a MountSnapshot, SystemSampleError> {
    mounts
        .iter()
        .filter(|disk| data_dir.starts_with(&disk.mount_point))
        .max_by_key(|disk| disk.mount_point.components().count())
        .ok_or_else(|| {
            SystemSampleError::new(
                SystemSampleErrorKind::Unsupported,
                "SYSTEM_DATA_VOLUME_NOT_FOUND",
                "no mounted volume contains the PCA data directory",
            )
        })
}

fn disk_sample_from_mount(selected: &MountSnapshot) -> Result<DiskSample, SystemSampleError> {
    if selected.total_bytes == 0 || selected.available_bytes > selected.total_bytes {
        return Err(SystemSampleError::new(
            SystemSampleErrorKind::Retryable,
            "SYSTEM_DISK_TOTALS_INVALID",
            "data-volume disk totals are unavailable or inconsistent",
        ));
    }

    let used_bytes = u128::from(selected.total_bytes - selected.available_bytes);
    let total_bytes = u128::from(selected.total_bytes);
    let hundredths_percent = (used_bytes * 10_000 + total_bytes / 2) / total_bytes;
    let used_percent = f64::from(u32::try_from(hundredths_percent).unwrap_or(10_000)) / 100.0;
    let low_space = selected.available_bytes < LOW_SPACE_THRESHOLD_BYTES;
    DiskSample::try_new(
        DiskScope::PcaDataVolume,
        selected.total_bytes,
        selected.available_bytes,
        used_percent,
        low_space,
        LOW_SPACE_THRESHOLD_BYTES,
        low_space.then(|| "DISK_SPACE_LOW".to_owned()),
    )
    .map_err(|error| domain_sample_error(&error))
}

fn normalize_agent_cpu(
    raw_cpu_usage: f64,
    logical_cpu_count: u32,
) -> Result<f64, SystemSampleError> {
    if logical_cpu_count == 0 {
        return Err(SystemSampleError::new(
            SystemSampleErrorKind::Unsupported,
            "SYSTEM_LOGICAL_CPU_UNAVAILABLE",
            "cannot normalize Agent CPU without logical CPUs",
        ));
    }
    checked_percentage(
        raw_cpu_usage / f64::from(logical_cpu_count),
        "normalized Agent CPU",
    )
}

fn checked_percentage(value: f64, field: &'static str) -> Result<f64, SystemSampleError> {
    if !value.is_finite() {
        return Err(SystemSampleError::new(
            SystemSampleErrorKind::Retryable,
            "SYSTEM_CPU_VALUE_INVALID",
            format!("{field} usage was not finite"),
        ));
    }
    Ok(value.clamp(0.0, 100.0))
}

fn domain_sample_error(error: &pca_domain::DomainError) -> SystemSampleError {
    SystemSampleError::new(
        SystemSampleErrorKind::Retryable,
        "SYSTEM_SAMPLE_INVALID",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::{disk_sample_from_mount, normalize_agent_cpu, select_data_volume, MountSnapshot};
    use crate::SystemSampleErrorKind;
    use std::path::{Path, PathBuf};

    fn disk(mount_point: &str, total_bytes: u64, available_bytes: u64) -> MountSnapshot {
        MountSnapshot {
            mount_point: PathBuf::from(mount_point),
            total_bytes,
            available_bytes,
        }
    }

    #[test]
    fn deepest_mount_prefix_selects_only_the_pca_data_volume() {
        let mounts = vec![
            disk("/", 1_000, 500),
            disk("/System/Volumes/Data", 900, 450),
            disk("/Volumes/External", 2_000, 1_500),
        ];

        let selected =
            select_data_volume(Path::new("/System/Volumes/Data/Users/a/PCA/Data"), &mounts)
                .expect("select data volume");

        assert_eq!(selected.total_bytes, 900);
        assert_eq!(selected.available_bytes, 450);
    }

    #[test]
    fn mount_prefix_matching_respects_path_component_boundaries() {
        let mounts = vec![disk("/", 1_000, 500), disk("/data", 900, 450)];

        let selected =
            select_data_volume(Path::new("/database/PCA/Data"), &mounts).expect("root volume");

        assert_eq!(selected.mount_point, Path::new("/"));
    }

    #[test]
    fn agent_cpu_is_divided_by_logical_cpu_count_and_clamped() {
        assert!((normalize_agent_cpu(240.0, 8).expect("normal CPU") - 30.0).abs() < f64::EPSILON);
        assert!(
            (normalize_agent_cpu(1_200.0, 8).expect("clamped CPU") - 100.0).abs() < f64::EPSILON
        );
    }

    #[test]
    fn agent_cpu_rejects_zero_logical_cpus_and_non_finite_input() {
        assert!(normalize_agent_cpu(10.0, 0).is_err());
        assert!(normalize_agent_cpu(f64::NAN, 8).is_err());
    }

    #[test]
    fn disk_sample_rejects_zero_or_inconsistent_totals() {
        for snapshot in [disk("/", 0, 0), disk("/", 100, 101)] {
            let error = disk_sample_from_mount(&snapshot).expect_err("invalid disk totals");

            assert_eq!(error.kind, SystemSampleErrorKind::Retryable);
            assert_eq!(error.code, "SYSTEM_DISK_TOTALS_INVALID");
        }
    }
}
