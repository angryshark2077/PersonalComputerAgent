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
    const fn running() -> Self {
        Self {
            mode: ControlMode::Running,
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
    assert!(
        observation_capacity > 0,
        "observation capacity must be greater than zero"
    );
    let (observations, receiver) = mpsc::channel(observation_capacity);
    let (control, control_receiver) = watch::channel(ControlState::running());
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
            result = sampler.sample(group) => result,
        };
        if control.borrow().mode != ControlMode::Running {
            continue;
        }

        let succeeded = result.is_ok();
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
        match send_observation(observation, &observations, &mut control).await {
            WaitOutcome::Ready => {}
            WaitOutcome::ControlChanged => continue,
            WaitOutcome::ReceiverClosed => return,
        }

        if succeeded {
            retry_index = 0;
            retry_delay = None;
            schedule = normal_schedule(period);
        } else {
            retry_delay = Some(RETRY_DELAYS[retry_index.min(RETRY_DELAYS.len() - 1)]);
            retry_index = retry_index.saturating_add(1);
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
