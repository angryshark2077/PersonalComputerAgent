use crate::{MetricGroup, SamplerHandle, SystemSampleError, SystemSampleErrorKind};
use pca_domain::SystemMetricSample;
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};

pub const CPU_MEMORY_INTERVAL: Duration = Duration::from_secs(30);
pub const DISK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SAMPLE_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
pub const RETRY_DELAYS: [Duration; 5] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(300),
];

#[derive(Debug, Clone, PartialEq)]
pub enum SystemObservation {
    Sampled {
        sample: SystemMetricSample,
        observed_at_ms: i64,
    },
    Failed {
        group: MetricGroup,
        error: SystemSampleError,
        observed_at_ms: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlMode {
    Running,
    Suppressed,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ControlState {
    mode: ControlMode,
    resume_generation: u64,
}

impl ControlState {
    const fn new(suppressed: bool) -> Self {
        Self {
            mode: if suppressed {
                ControlMode::Suppressed
            } else {
                ControlMode::Running
            },
            resume_generation: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Ready,
    ControlChanged,
    ReceiverClosed,
}

pub struct SystemCollectorHandle {
    control: watch::Sender<ControlState>,
    supervisor: Option<JoinHandle<Result<(), SystemSampleError>>>,
}

impl SystemCollectorHandle {
    /// Suppresses or resumes both metric groups without blocking on sampling work.
    pub fn set_suppressed(&self, suppressed: bool) {
        self.control
            .send_if_modified(|current| match (current.mode, suppressed) {
                (ControlMode::Shutdown | ControlMode::Suppressed, true)
                | (ControlMode::Shutdown | ControlMode::Running, false) => false,
                (ControlMode::Running, true) => {
                    current.mode = ControlMode::Suppressed;
                    true
                }
                (ControlMode::Suppressed, false) => {
                    current.mode = ControlMode::Running;
                    current.resume_generation = current.resume_generation.wrapping_add(1);
                    true
                }
            });
    }

    /// Stops both schedules and joins the dedicated sampler owner thread.
    ///
    /// # Errors
    ///
    /// Returns a typed fatal error if a runtime task or the sampler owner fails to stop.
    pub async fn shutdown(mut self) -> Result<(), SystemSampleError> {
        request_shutdown(&self.control);
        let supervisor = self.supervisor.take().ok_or_else(runtime_stopped_error)?;
        supervisor.await.map_err(|error| {
            SystemSampleError::new(
                SystemSampleErrorKind::Fatal,
                "SYSTEM_COLLECTOR_JOIN_FAILED",
                error.to_string(),
            )
        })?
    }
}

impl Drop for SystemCollectorHandle {
    fn drop(&mut self) {
        request_shutdown(&self.control);
    }
}

/// Starts independent CPU/memory and disk schedules over one bounded sampler actor.
///
/// Both schedules sample immediately. Dropping the observation receiver stops both
/// schedules and the sampler owner.
///
/// # Panics
///
/// Panics when `observation_capacity` is zero.
#[must_use]
pub fn start_system_collector(
    sampler: SamplerHandle,
    observation_capacity: usize,
) -> (SystemCollectorHandle, mpsc::Receiver<SystemObservation>) {
    start_system_collector_with_suppression(sampler, observation_capacity, false)
}

/// Starts both metric schedules atomically in either running or suppressed mode.
///
/// An initially suppressed collector makes no sampler request until the handle
/// observes a later resume edge.
///
/// # Panics
///
/// Panics when `observation_capacity` is zero.
#[must_use]
pub fn start_system_collector_with_suppression(
    sampler: SamplerHandle,
    observation_capacity: usize,
    initially_suppressed: bool,
) -> (SystemCollectorHandle, mpsc::Receiver<SystemObservation>) {
    assert!(
        observation_capacity > 0,
        "observation capacity must be greater than zero"
    );
    let (observations, receiver) = mpsc::channel(observation_capacity);
    let (control, control_receiver) = watch::channel(ControlState::new(initially_suppressed));
    let supervisor = tokio::spawn(supervise(sampler, observations, control_receiver));

    (
        SystemCollectorHandle {
            control,
            supervisor: Some(supervisor),
        },
        receiver,
    )
}

async fn supervise(
    sampler: SamplerHandle,
    observations: mpsc::Sender<SystemObservation>,
    control: watch::Receiver<ControlState>,
) -> Result<(), SystemSampleError> {
    let sampler = Arc::new(sampler);
    let cpu_task = tokio::spawn(run_group(
        MetricGroup::CpuMemory,
        CPU_MEMORY_INTERVAL,
        Arc::clone(&sampler),
        observations.clone(),
        control.clone(),
    ));
    let disk_task = tokio::spawn(run_group(
        MetricGroup::Disk,
        DISK_INTERVAL,
        Arc::clone(&sampler),
        observations,
        control,
    ));

    let (cpu_result, disk_result) = tokio::join!(cpu_task, disk_task);
    let task_error = cpu_result.err().or_else(|| disk_result.err()).map(|error| {
        SystemSampleError::new(
            SystemSampleErrorKind::Fatal,
            "SYSTEM_COLLECTOR_TASK_FAILED",
            error.to_string(),
        )
    });
    let sampler = Arc::try_unwrap(sampler).map_err(|_| {
        SystemSampleError::new(
            SystemSampleErrorKind::Fatal,
            "SYSTEM_SAMPLER_REFERENCE_LEAKED",
            "system collector tasks retained the sampler after stopping",
        )
    })?;
    let shutdown_result = sampler.shutdown().await;

    if let Some(error) = task_error {
        Err(error)
    } else {
        shutdown_result
    }
}

async fn run_group(
    group: MetricGroup,
    period: Duration,
    sampler: Arc<SamplerHandle>,
    observations: mpsc::Sender<SystemObservation>,
    mut control: watch::Receiver<ControlState>,
) {
    let mut schedule = normal_schedule(period);
    let mut retry_index = 0_usize;
    let mut retry_delay = None;
    let mut sample_now = true;
    let mut seen_resume_generation = 0_u64;

    loop {
        let mut current_control = *control.borrow_and_update();
        match current_control.mode {
            ControlMode::Shutdown => return,
            ControlMode::Suppressed => {
                let Some(running_control) = wait_until_running(&mut control, &observations).await
                else {
                    return;
                };
                current_control = running_control;
            }
            ControlMode::Running => {}
        }
        if current_control.resume_generation != seen_resume_generation {
            seen_resume_generation = current_control.resume_generation;
            retry_index = 0;
            retry_delay = None;
            schedule = normal_schedule(period);
            sample_now = true;
        }

        if !sample_now {
            let outcome = if let Some(delay) = retry_delay.take() {
                wait_for_retry(delay, &mut control, &observations).await
            } else {
                wait_for_schedule(&mut schedule, &mut control, &observations).await
            };
            match outcome {
                WaitOutcome::Ready => {}
                WaitOutcome::ControlChanged => continue,
                WaitOutcome::ReceiverClosed => return,
            }
        }

        let result = tokio::select! {
            biased;
            changed = control.changed() => {
                if changed.is_err() {
                    return;
                }
                continue;
            }
            () = observations.closed() => return,
            result = time::timeout(SAMPLE_OPERATION_TIMEOUT, sampler.sample(group)) => {
                result.unwrap_or_else(|_| Err(SystemSampleError::new(
                    SystemSampleErrorKind::Fatal,
                    "SYSTEM_SAMPLE_TIMEOUT",
                    "the system metrics source did not return before the operation deadline",
                )))
            },
        };
        if control.borrow().mode != ControlMode::Running {
            continue;
        }

        let error_kind = result.as_ref().err().map(|error| error.kind);
        let observation = match result {
            Ok(sample) => SystemObservation::Sampled {
                sample,
                observed_at_ms: observed_at_ms(),
            },
            Err(error) => SystemObservation::Failed {
                group,
                error,
                observed_at_ms: observed_at_ms(),
            },
        };
        let terminal = matches!(
            error_kind,
            Some(SystemSampleErrorKind::Unsupported | SystemSampleErrorKind::Fatal)
        );
        match send_observation(observation, &observations, &mut control).await {
            WaitOutcome::Ready => {}
            WaitOutcome::ControlChanged if terminal => return,
            WaitOutcome::ControlChanged => continue,
            WaitOutcome::ReceiverClosed => return,
        }

        match error_kind {
            None => {
                retry_index = 0;
                retry_delay = None;
                schedule = normal_schedule(period);
            }
            Some(SystemSampleErrorKind::Retryable) => {
                retry_delay = Some(RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)]);
                retry_index = retry_index.saturating_add(1);
            }
            Some(SystemSampleErrorKind::Unsupported | SystemSampleErrorKind::Fatal) => return,
        }
        sample_now = false;
    }
}

async fn wait_until_running(
    control: &mut watch::Receiver<ControlState>,
    observations: &mpsc::Sender<SystemObservation>,
) -> Option<ControlState> {
    loop {
        let current_control = *control.borrow_and_update();
        match current_control.mode {
            ControlMode::Running => return Some(current_control),
            ControlMode::Shutdown => return None,
            ControlMode::Suppressed => {}
        }
        tokio::select! {
            biased;
            changed = control.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            () = observations.closed() => return None,
        }
    }
}

async fn wait_for_retry(
    delay: Duration,
    control: &mut watch::Receiver<ControlState>,
    observations: &mpsc::Sender<SystemObservation>,
) -> WaitOutcome {
    tokio::select! {
        biased;
        changed = control.changed() => if changed.is_ok() {
            WaitOutcome::ControlChanged
        } else {
            WaitOutcome::ReceiverClosed
        },
        () = observations.closed() => WaitOutcome::ReceiverClosed,
        () = time::sleep(delay) => WaitOutcome::Ready,
    }
}

async fn wait_for_schedule(
    schedule: &mut time::Interval,
    control: &mut watch::Receiver<ControlState>,
    observations: &mpsc::Sender<SystemObservation>,
) -> WaitOutcome {
    tokio::select! {
        biased;
        changed = control.changed() => if changed.is_ok() {
            WaitOutcome::ControlChanged
        } else {
            WaitOutcome::ReceiverClosed
        },
        () = observations.closed() => WaitOutcome::ReceiverClosed,
        _ = schedule.tick() => WaitOutcome::Ready,
    }
}

async fn send_observation(
    observation: SystemObservation,
    observations: &mpsc::Sender<SystemObservation>,
    control: &mut watch::Receiver<ControlState>,
) -> WaitOutcome {
    tokio::select! {
        biased;
        changed = control.changed() => if changed.is_ok() {
            WaitOutcome::ControlChanged
        } else {
            WaitOutcome::ReceiverClosed
        },
        sent = observations.send(observation) => if sent.is_ok() {
            WaitOutcome::Ready
        } else {
            WaitOutcome::ReceiverClosed
        },
    }
}

fn request_shutdown(control: &watch::Sender<ControlState>) {
    control.send_if_modified(|current| {
        if current.mode == ControlMode::Shutdown {
            false
        } else {
            current.mode = ControlMode::Shutdown;
            true
        }
    });
}

fn normal_schedule(period: Duration) -> time::Interval {
    let mut schedule = time::interval_at(time::Instant::now() + period, period);
    schedule.set_missed_tick_behavior(MissedTickBehavior::Skip);
    schedule
}

fn observed_at_ms() -> i64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

fn runtime_stopped_error() -> SystemSampleError {
    SystemSampleError::new(
        SystemSampleErrorKind::Fatal,
        "SYSTEM_COLLECTOR_STOPPED",
        "the system collector runtime is not available",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use pca_domain::{CpuMemorySample, DiskSample};

    use crate::{start_sampler, SystemMetricsSource};

    use super::{start_system_collector, SystemObservation, SAMPLE_OPERATION_TIMEOUT};

    struct BlockingSource {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl BlockingSource {
        fn blocked_error(&mut self) -> crate::SystemSampleError {
            if let Some(entered) = self.entered.take() {
                entered.send(()).expect("report blocked system sample");
            }
            self.release.recv().expect("release blocked system sample");
            crate::SystemSampleError::new(
                crate::SystemSampleErrorKind::Fatal,
                "TEST_RELEASED",
                "blocking fixture released",
            )
        }
    }

    impl SystemMetricsSource for BlockingSource {
        fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, crate::SystemSampleError> {
            Err(self.blocked_error())
        }

        fn sample_disk(&mut self) -> Result<DiskSample, crate::SystemSampleError> {
            Err(self.blocked_error())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn blocked_system_source_becomes_a_fatal_timeout() {
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let sampler = start_sampler(BlockingSource {
            entered: Some(entered_sender),
            release: release_receiver,
        });
        let (collector, mut observations) = start_system_collector(sampler, 4);
        tokio::task::spawn_blocking(move || entered_receiver.recv())
            .await
            .expect("wait task")
            .expect("system sample entered");

        tokio::time::advance(SAMPLE_OPERATION_TIMEOUT).await;
        let observation = observations.recv().await.expect("timeout observation");
        assert!(matches!(
            observation,
            SystemObservation::Failed { error, .. } if error.code == "SYSTEM_SAMPLE_TIMEOUT"
        ));

        release_sender.send(()).expect("release system sampler");
        collector.shutdown().await.expect("shut down collector");
    }
}
