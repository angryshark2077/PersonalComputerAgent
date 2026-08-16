use pca_domain::{
    AgentCpuMemory, CpuMemorySample, DiskSample, DiskScope, HostCpuMemory, SystemMetricSample,
};
use pca_system_collector::{
    start_sampler, start_system_collector, start_system_collector_with_suppression, MetricGroup,
    SystemMetricsSource, SystemObservation, SystemSampleError, SystemSampleErrorKind,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc as std_mpsc, Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Clone)]
struct FakeControls {
    cpu_calls: Arc<AtomicUsize>,
    disk_calls: Arc<AtomicUsize>,
    cpu_results: Arc<Mutex<VecDeque<Result<(), SystemSampleError>>>>,
    disk_results: Arc<Mutex<VecDeque<Result<(), SystemSampleError>>>>,
}

impl FakeControls {
    fn always_succeeds() -> Self {
        Self {
            cpu_calls: Arc::new(AtomicUsize::new(0)),
            disk_calls: Arc::new(AtomicUsize::new(0)),
            cpu_results: Arc::new(Mutex::new(VecDeque::new())),
            disk_results: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn cpu_script(results: impl IntoIterator<Item = Result<(), SystemSampleError>>) -> Self {
        let controls = Self::always_succeeds();
        controls
            .cpu_results
            .lock()
            .expect("CPU result queue")
            .extend(results);
        controls
    }

    fn cpu_calls(&self) -> usize {
        self.cpu_calls.load(Ordering::SeqCst)
    }

    fn disk_calls(&self) -> usize {
        self.disk_calls.load(Ordering::SeqCst)
    }
}

struct FakeSource {
    controls: FakeControls,
    owner_stopped: Option<std_mpsc::Sender<()>>,
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        if let Some(owner_stopped) = self.owner_stopped.take() {
            let _ = owner_stopped.send(());
        }
    }
}

impl SystemMetricsSource for FakeSource {
    fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, SystemSampleError> {
        self.controls.cpu_calls.fetch_add(1, Ordering::SeqCst);
        take_result(&self.controls.cpu_results)?;
        Ok(cpu_sample())
    }

    fn sample_disk(&mut self) -> Result<DiskSample, SystemSampleError> {
        self.controls.disk_calls.fetch_add(1, Ordering::SeqCst);
        take_result(&self.controls.disk_results)?;
        Ok(disk_sample())
    }
}

fn take_result(
    results: &Mutex<VecDeque<Result<(), SystemSampleError>>>,
) -> Result<(), SystemSampleError> {
    results
        .lock()
        .expect("sample result queue")
        .pop_front()
        .unwrap_or(Ok(()))
}

fn retryable_error() -> SystemSampleError {
    SystemSampleError {
        kind: SystemSampleErrorKind::Retryable,
        code: "SYSTEM_TEST_RETRYABLE",
        message: "retry the deterministic sample".to_owned(),
    }
}

fn terminal_error(kind: SystemSampleErrorKind) -> SystemSampleError {
    SystemSampleError {
        kind,
        code: "SYSTEM_TEST_TERMINAL",
        message: "stop the deterministic metric group".to_owned(),
    }
}

fn cpu_sample() -> CpuMemorySample {
    CpuMemorySample::try_new(
        200,
        8,
        HostCpuMemory::try_new(25.0, 1_000, 400).expect("host fixture"),
        AgentCpuMemory::try_new(5.0, 20).expect("agent fixture"),
    )
    .expect("CPU fixture")
}

fn disk_sample() -> DiskSample {
    DiskSample::try_new(
        DiskScope::PcaDataVolume,
        10_000_000_000,
        5_000_000_000,
        50.0,
        false,
        2_147_483_648,
        None,
    )
    .expect("disk fixture")
}

fn test_runtime(
    controls: &FakeControls,
    capacity: usize,
) -> (
    pca_system_collector::SystemCollectorHandle,
    mpsc::Receiver<SystemObservation>,
) {
    test_runtime_with_owner_signal(controls, capacity, None)
}

fn test_runtime_with_owner_signal(
    controls: &FakeControls,
    capacity: usize,
    owner_stopped: Option<std_mpsc::Sender<()>>,
) -> (
    pca_system_collector::SystemCollectorHandle,
    mpsc::Receiver<SystemObservation>,
) {
    start_system_collector(
        start_sampler(FakeSource {
            controls: controls.clone(),
            owner_stopped,
        }),
        capacity,
    )
}

fn observation_group(observation: &SystemObservation) -> MetricGroup {
    match observation {
        SystemObservation::Sampled {
            sample: SystemMetricSample::CpuMemory(_),
            ..
        }
        | SystemObservation::Failed {
            group: MetricGroup::CpuMemory,
            ..
        } => MetricGroup::CpuMemory,
        SystemObservation::Sampled {
            sample: SystemMetricSample::Disk(_),
            ..
        }
        | SystemObservation::Failed {
            group: MetricGroup::Disk,
            ..
        } => MetricGroup::Disk,
    }
}

async fn next_group(observations: &mut mpsc::Receiver<SystemObservation>) -> MetricGroup {
    observation_group(&next_observation(observations).await)
}

async fn next_observation(
    observations: &mut mpsc::Receiver<SystemObservation>,
) -> SystemObservation {
    for _ in 0..10_000 {
        match observations.try_recv() {
            Ok(observation) => return observation,
            Err(mpsc::error::TryRecvError::Empty) => {
                tokio::task::yield_now().await;
                std::thread::sleep(Duration::from_micros(50));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                panic!("collector observation channel disconnected")
            }
        }
    }
    panic!("collector observation was not emitted")
}

async fn assert_initial_groups(observations: &mut mpsc::Receiver<SystemObservation>) {
    let mut groups = [
        next_group(observations).await,
        next_group(observations).await,
    ];
    groups.sort_by_key(|group| match group {
        MetricGroup::CpuMemory => 0,
        MetricGroup::Disk => 1,
    });
    assert_eq!(groups, [MetricGroup::CpuMemory, MetricGroup::Disk]);
    tokio::task::yield_now().await;
}

async fn initial_observation_for_group(
    observations: &mut mpsc::Receiver<SystemObservation>,
    wanted: MetricGroup,
) -> SystemObservation {
    let initial = [
        next_observation(observations).await,
        next_observation(observations).await,
    ];
    assert_ne!(
        observation_group(&initial[0]),
        observation_group(&initial[1])
    );
    initial
        .into_iter()
        .find(|observation| observation_group(observation) == wanted)
        .expect("initial metric group exists")
}

async fn next_for_group(
    observations: &mut mpsc::Receiver<SystemObservation>,
    wanted: MetricGroup,
) -> SystemObservation {
    for _ in 0..10_000 {
        match observations.try_recv() {
            Ok(observation) if observation_group(&observation) == wanted => {
                tokio::task::yield_now().await;
                return observation;
            }
            Ok(_) | Err(mpsc::error::TryRecvError::Empty) => {
                tokio::task::yield_now().await;
                std::thread::sleep(Duration::from_micros(50));
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                panic!("collector observation channel disconnected")
            }
        }
    }
    panic!("collector did not emit {wanted:?}")
}

async fn wait_for_calls(controls: &FakeControls, cpu: usize, disk: usize) {
    for _ in 0..1_000 {
        if controls.cpu_calls() == cpu && controls.disk_calls() == disk {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_micros(50));
    }
    panic!(
        "sample calls did not reach CPU={cpu}, disk={disk}; got CPU={}, disk={}",
        controls.cpu_calls(),
        controls.disk_calls()
    );
}

fn drain_groups(observations: &mut mpsc::Receiver<SystemObservation>) -> Vec<MetricGroup> {
    let mut groups = Vec::new();
    while let Ok(observation) = observations.try_recv() {
        groups.push(observation_group(&observation));
    }
    groups
}

async fn wait_for_owner_stop(owner_stopped: std_mpsc::Receiver<()>) {
    let stopped =
        tokio::task::spawn_blocking(move || owner_stopped.recv_timeout(Duration::from_secs(1)))
            .await
            .expect("owner-stop observer task");
    assert!(
        stopped.is_ok(),
        "sampler owner did not stop after observation receiver closed"
    );
}

#[tokio::test(start_paused = true)]
async fn emits_immediately_then_on_independent_periods() {
    let controls = FakeControls::always_succeeds();
    let (handle, mut observations) = test_runtime(&controls, 16);
    assert_initial_groups(&mut observations).await;

    tokio::time::advance(Duration::from_secs(30)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (2, 1));

    tokio::time::advance(Duration::from_secs(270)).await;
    let mut groups = [
        next_group(&mut observations).await,
        next_group(&mut observations).await,
    ];
    groups.sort_by_key(|group| match group {
        MetricGroup::CpuMemory => 0,
        MetricGroup::Disk => 1,
    });
    assert_eq!(groups, [MetricGroup::CpuMemory, MetricGroup::Disk]);
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (3, 2));
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn retry_delays_are_exact_and_cap_at_three_hundred_seconds() {
    let controls = FakeControls::cpu_script((0..7).map(|_| Err(retryable_error())));
    let (handle, mut observations) = test_runtime(&controls, 32);
    assert_initial_groups(&mut observations).await;
    assert_eq!(controls.cpu_calls(), 1);

    for (elapsed_before, elapsed_at, expected_calls) in [
        (29, 1, 2),
        (59, 1, 3),
        (119, 1, 4),
        (239, 1, 5),
        (299, 1, 6),
        (299, 1, 7),
    ] {
        tokio::time::advance(Duration::from_secs(elapsed_before)).await;
        tokio::task::yield_now().await;
        assert_eq!(controls.cpu_calls(), expected_calls - 1);

        tokio::time::advance(Duration::from_secs(elapsed_at)).await;
        let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
        assert_eq!(controls.cpu_calls(), expected_calls);
    }

    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn successful_retry_resets_backoff_without_changing_disk_schedule() {
    let controls = FakeControls::cpu_script([
        Err(retryable_error()),
        Err(retryable_error()),
        Ok(()),
        Err(retryable_error()),
        Ok(()),
    ]);
    let (handle, mut observations) = test_runtime(&controls, 32);
    assert_initial_groups(&mut observations).await;

    tokio::time::advance(Duration::from_secs(30)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (2, 1));
    tokio::time::advance(Duration::from_secs(60)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (3, 1));
    tokio::time::advance(Duration::from_secs(30)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (4, 1));
    tokio::time::advance(Duration::from_secs(30)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (5, 1));

    tokio::time::advance(Duration::from_secs(150)).await;
    let mut groups = [
        next_group(&mut observations).await,
        next_group(&mut observations).await,
    ];
    groups.sort_by_key(|group| match group {
        MetricGroup::CpuMemory => 0,
        MetricGroup::Disk => 1,
    });
    assert_eq!(groups, [MetricGroup::CpuMemory, MetricGroup::Disk]);
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (6, 2));
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn unsupported_error_stops_only_its_metric_group_until_shutdown() {
    let controls =
        FakeControls::cpu_script([Err(terminal_error(SystemSampleErrorKind::Unsupported))]);
    let (handle, mut observations) = test_runtime(&controls, 16);
    let observation =
        initial_observation_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert!(matches!(
        observation,
        SystemObservation::Failed {
            error: SystemSampleError {
                kind: SystemSampleErrorKind::Unsupported,
                ..
            },
            ..
        }
    ));

    tokio::time::advance(Duration::from_secs(900)).await;
    wait_for_calls(&controls, 1, 2).await;
    assert_eq!(controls.cpu_calls(), 1);
    let _ = next_for_group(&mut observations, MetricGroup::Disk).await;
    assert!(drain_groups(&mut observations).is_empty());

    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn fatal_error_stops_only_its_metric_group_until_shutdown() {
    let controls = FakeControls::cpu_script([Err(terminal_error(SystemSampleErrorKind::Fatal))]);
    let (handle, mut observations) = test_runtime(&controls, 16);
    let observation =
        initial_observation_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert!(matches!(
        observation,
        SystemObservation::Failed {
            error: SystemSampleError {
                kind: SystemSampleErrorKind::Fatal,
                ..
            },
            ..
        }
    ));

    tokio::time::advance(Duration::from_secs(900)).await;
    wait_for_calls(&controls, 1, 2).await;
    assert_eq!(controls.cpu_calls(), 1);
    let _ = next_for_group(&mut observations, MetricGroup::Disk).await;
    assert!(drain_groups(&mut observations).is_empty());

    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn terminal_result_stays_terminal_when_control_changes_during_observation_delivery() {
    let controls = FakeControls::cpu_script([
        Ok(()),
        Err(terminal_error(SystemSampleErrorKind::Unsupported)),
        Ok(()),
    ]);
    let (handle, mut observations) = test_runtime(&controls, 2);
    wait_for_calls(&controls, 1, 1).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }

    tokio::time::advance(Duration::from_secs(30)).await;
    wait_for_calls(&controls, 2, 1).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }

    handle.set_suppressed(true);
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    handle.set_suppressed(false);
    let _ = next_group(&mut observations).await;
    let _ = next_group(&mut observations).await;
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
        std::thread::yield_now();
    }
    assert_eq!(controls.disk_calls(), 2);
    assert_eq!(
        controls.cpu_calls(),
        2,
        "a terminal sample result must survive an interrupted observation send"
    );

    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn suppression_never_requests_the_sampler_and_resume_samples_fresh() {
    let controls = FakeControls::always_succeeds();
    let (handle, mut observations) = test_runtime(&controls, 16);
    assert_initial_groups(&mut observations).await;

    handle.set_suppressed(true);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(900)).await;
    tokio::task::yield_now().await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (1, 1));
    assert!(drain_groups(&mut observations).is_empty());

    handle.set_suppressed(false);
    wait_for_calls(&controls, 2, 2).await;
    assert_initial_groups(&mut observations).await;
    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (2, 2));
    tokio::time::advance(Duration::from_secs(1)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (3, 2));
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn initial_suppression_is_atomic_and_resume_samples_fresh() {
    let controls = FakeControls::always_succeeds();
    let sampler = start_sampler(FakeSource {
        controls: controls.clone(),
        owner_stopped: None,
    });
    let (handle, mut observations) = start_system_collector_with_suppression(sampler, 16, true);

    for _ in 0..100 {
        tokio::task::yield_now().await;
        std::thread::yield_now();
    }
    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (0, 0));
    assert!(drain_groups(&mut observations).is_empty());

    handle.set_suppressed(false);
    wait_for_calls(&controls, 1, 1).await;
    assert_initial_groups(&mut observations).await;
    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (1, 1));
    tokio::time::advance(Duration::from_secs(1)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (2, 1));

    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn coalesced_suppress_resume_samples_each_group_fresh_and_resets_schedules() {
    let controls = FakeControls::cpu_script([Err(retryable_error()), Ok(())]);
    let (handle, mut observations) = test_runtime(&controls, 16);
    assert_initial_groups(&mut observations).await;

    handle.set_suppressed(true);
    handle.set_suppressed(false);

    wait_for_calls(&controls, 2, 2).await;
    assert_initial_groups(&mut observations).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (2, 2));

    tokio::time::advance(Duration::from_secs(29)).await;
    tokio::task::yield_now().await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (2, 2));
    tokio::time::advance(Duration::from_secs(1)).await;
    let _ = next_for_group(&mut observations, MetricGroup::CpuMemory).await;
    assert_eq!((controls.cpu_calls(), controls.disk_calls()), (3, 2));
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn bounded_output_skips_missed_ticks_instead_of_bursting() {
    let controls = FakeControls::always_succeeds();
    let (handle, mut observations) = test_runtime(&controls, 1);
    wait_for_calls(&controls, 1, 1).await;

    tokio::time::advance(Duration::from_secs(600)).await;
    tokio::task::yield_now().await;
    assert!(controls.cpu_calls() <= 2);
    assert!(controls.disk_calls() <= 2);

    for _ in 0..100 {
        let _ = observations.try_recv();
        tokio::task::yield_now().await;
        std::thread::yield_now();
    }
    assert!(controls.cpu_calls() <= 2);
    assert!(controls.disk_calls() <= 2);
    handle.shutdown().await.expect("collector shutdown");
}

#[test]
#[should_panic(expected = "observation capacity must be greater than zero")]
fn zero_observation_capacity_is_rejected() {
    let controls = FakeControls::always_succeeds();
    let _ = test_runtime(&controls, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn receiver_drop_during_normal_wait_stops_owner_before_handle_shutdown() {
    let controls = FakeControls::always_succeeds();
    let (owner_stopped_sender, owner_stopped_receiver) = std_mpsc::channel();
    let (handle, mut observations) =
        test_runtime_with_owner_signal(&controls, 16, Some(owner_stopped_sender));
    assert_initial_groups(&mut observations).await;
    drop(observations);

    wait_for_owner_stop(owner_stopped_receiver).await;
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn receiver_drop_during_retry_wait_stops_owner_before_handle_shutdown() {
    let controls = FakeControls::cpu_script([Err(retryable_error())]);
    let (owner_stopped_sender, owner_stopped_receiver) = std_mpsc::channel();
    let (handle, mut observations) =
        test_runtime_with_owner_signal(&controls, 16, Some(owner_stopped_sender));
    assert_initial_groups(&mut observations).await;
    drop(observations);

    wait_for_owner_stop(owner_stopped_receiver).await;
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn receiver_drop_with_full_channel_stops_owner_before_handle_shutdown() {
    let controls = FakeControls::always_succeeds();
    let (owner_stopped_sender, owner_stopped_receiver) = std_mpsc::channel();
    let (handle, observations) =
        test_runtime_with_owner_signal(&controls, 1, Some(owner_stopped_sender));
    wait_for_calls(&controls, 1, 1).await;
    drop(observations);

    wait_for_owner_stop(owner_stopped_receiver).await;
    handle.shutdown().await.expect("collector shutdown");
}

#[tokio::test(start_paused = true)]
async fn shutdown_closes_output_without_post_shutdown_observations() {
    let controls = FakeControls::always_succeeds();
    let (handle, mut observations) = test_runtime(&controls, 16);
    assert_initial_groups(&mut observations).await;

    handle.shutdown().await.expect("collector shutdown");
    assert!(observations.recv().await.is_none());
    tokio::time::advance(Duration::from_secs(600)).await;
    assert!(observations.try_recv().is_err());
}
