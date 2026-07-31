use pca_domain::SystemMetricSample;
use pca_system_collector::{start_sampler, MetricGroup, SysinfoMetricsSource};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_sampler_returns_bounded_private_system_metrics() {
    let temporary_directory = tempfile::tempdir().expect("temporary directory");
    let data_dir = temporary_directory.path().join("Data");
    std::fs::create_dir(&data_dir).expect("create data directory");
    let sampler = start_sampler(SysinfoMetricsSource::new(data_dir.clone()).expect("source"));

    let cpu = sampler
        .sample(MetricGroup::CpuMemory)
        .await
        .expect("CPU sample");
    let disk = sampler
        .sample(MetricGroup::Disk)
        .await
        .expect("disk sample");

    let SystemMetricSample::CpuMemory(cpu_sample) = &cpu else {
        panic!("expected CPU/memory sample");
    };
    let SystemMetricSample::Disk(disk_sample) = &disk else {
        panic!("expected disk sample");
    };
    assert!(cpu_sample.host().cpu_usage_percent().is_finite());
    assert!((0.0..=100.0).contains(&cpu_sample.host().cpu_usage_percent()));
    assert!((0.0..=100.0).contains(&cpu_sample.agent().cpu_usage_percent()));
    assert!(cpu_sample.host().memory_used_bytes() <= cpu_sample.host().memory_total_bytes());
    assert!(disk_sample.available_bytes() <= disk_sample.total_bytes());

    let json = serde_json::to_string(&(cpu, disk)).expect("serialize samples");
    let current_pid = std::process::id().to_string();
    for secret in [
        data_dir.to_string_lossy().as_ref(),
        current_pid.as_str(),
        "mount_point",
        "filesystem",
        "command",
        "environment",
    ] {
        assert!(!json.contains(secret), "serialized sample leaked {secret}");
    }

    sampler.shutdown().await.expect("shut down actor");
}
