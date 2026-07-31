use pca_domain::{CpuMemorySample, DiskSample, SystemMetricSample};
use std::{fmt, thread};
use tokio::sync::{mpsc, oneshot};

const REQUEST_QUEUE_CAPACITY: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricGroup {
    CpuMemory,
    Disk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSampleErrorKind {
    Retryable,
    Unsupported,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSampleError {
    pub kind: SystemSampleErrorKind,
    pub code: &'static str,
    pub message: String,
}

impl SystemSampleError {
    pub(crate) fn new(
        kind: SystemSampleErrorKind,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for SystemSampleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for SystemSampleError {}

pub trait SystemMetricsSource: Send + 'static {
    /// Samples checked host and Agent CPU/memory metrics.
    ///
    /// # Errors
    ///
    /// Returns a typed source error when required system data is unavailable or invalid.
    fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, SystemSampleError>;

    /// Samples checked PCA data-volume disk metrics.
    ///
    /// # Errors
    ///
    /// Returns a typed source error when the data volume is unavailable or invalid.
    fn sample_disk(&mut self) -> Result<DiskSample, SystemSampleError>;
}

enum SampleRequest {
    Sample {
        group: MetricGroup,
        response: oneshot::Sender<Result<SystemMetricSample, SystemSampleError>>,
    },
}

pub struct SamplerHandle {
    requests: Option<mpsc::Sender<SampleRequest>>,
    owner_stopped: Option<oneshot::Receiver<()>>,
    owner_thread: Option<thread::JoinHandle<()>>,
}

impl SamplerHandle {
    /// Requests one metric group from the dedicated sampling thread.
    ///
    /// # Errors
    ///
    /// Returns a typed fatal error if the owner thread is no longer available, or
    /// forwards the source's typed sampling error.
    pub async fn sample(
        &self,
        group: MetricGroup,
    ) -> Result<SystemMetricSample, SystemSampleError> {
        let requests = self.requests.as_ref().ok_or_else(actor_stopped_error)?;
        let (response, receiver) = oneshot::channel();
        requests
            .send(SampleRequest::Sample { group, response })
            .await
            .map_err(|_| actor_stopped_error())?;
        receiver.await.map_err(|_| actor_stopped_error())?
    }

    /// Closes the request queue and joins the owner thread off the async worker.
    ///
    /// # Errors
    ///
    /// Returns a typed fatal error if the owner thread panicked or its join task failed.
    pub async fn shutdown(mut self) -> Result<(), SystemSampleError> {
        self.requests.take();
        let owner_signaled = match self.owner_stopped.take() {
            Some(receiver) => receiver.await.is_ok(),
            None => false,
        };
        let owner_thread = self.owner_thread.take().ok_or_else(actor_stopped_error)?;
        let joined = tokio::task::spawn_blocking(move || owner_thread.join().is_ok())
            .await
            .map_err(|error| {
                SystemSampleError::new(
                    SystemSampleErrorKind::Fatal,
                    "SYSTEM_SAMPLER_JOIN_FAILED",
                    error.to_string(),
                )
            })?;
        if owner_signaled && joined {
            Ok(())
        } else {
            Err(SystemSampleError::new(
                SystemSampleErrorKind::Fatal,
                "SYSTEM_SAMPLER_STOP_FAILED",
                "the system sampler owner thread stopped unexpectedly",
            ))
        }
    }
}

/// Starts a bounded sampler actor on one named owner thread.
///
/// # Panics
///
/// Panics if the operating system cannot create the dedicated owner thread.
pub fn start_sampler<S: SystemMetricsSource>(mut source: S) -> SamplerHandle {
    let (requests, mut receiver) = mpsc::channel(REQUEST_QUEUE_CAPACITY);
    let (owner_stopped, stopped_receiver) = oneshot::channel();
    let owner_thread = thread::Builder::new()
        .name("pca-system-sampler".to_owned())
        .spawn(move || {
            while let Some(SampleRequest::Sample { group, response }) = receiver.blocking_recv() {
                if response.is_closed() {
                    continue;
                }
                let result = match group {
                    MetricGroup::CpuMemory => source
                        .sample_cpu_memory()
                        .map(SystemMetricSample::CpuMemory),
                    MetricGroup::Disk => source.sample_disk().map(SystemMetricSample::Disk),
                };
                let _ = response.send(result);
            }
            let _ = owner_stopped.send(());
        })
        .expect("system sampler owner thread must start");

    SamplerHandle {
        requests: Some(requests),
        owner_stopped: Some(stopped_receiver),
        owner_thread: Some(owner_thread),
    }
}

fn actor_stopped_error() -> SystemSampleError {
    SystemSampleError::new(
        SystemSampleErrorKind::Fatal,
        "SYSTEM_SAMPLER_STOPPED",
        "the system sampler owner thread is not available",
    )
}

#[cfg(test)]
mod tests {
    use super::{start_sampler, MetricGroup, SystemMetricsSource};
    use pca_domain::{
        AgentCpuMemory, CpuMemorySample, DiskSample, DiskScope, HostCpuMemory, SystemMetricSample,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    };

    struct FakeSource {
        cpu_calls: Arc<AtomicUsize>,
        disk_calls: Arc<AtomicUsize>,
    }

    impl SystemMetricsSource for FakeSource {
        fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, super::SystemSampleError> {
            self.cpu_calls.fetch_add(1, Ordering::SeqCst);
            Ok(CpuMemorySample::try_new(
                200,
                8,
                HostCpuMemory::try_new(25.0, 1_000, 400).expect("host fixture"),
                AgentCpuMemory::try_new(5.0, 20).expect("agent fixture"),
            )
            .expect("CPU fixture"))
        }

        fn sample_disk(&mut self) -> Result<DiskSample, super::SystemSampleError> {
            self.disk_calls.fetch_add(1, Ordering::SeqCst);
            Ok(DiskSample::try_new(
                DiskScope::PcaDataVolume,
                1_000,
                500,
                50.0,
                true,
                2_147_483_648,
                Some("DISK_SPACE_LOW".to_owned()),
            )
            .expect("disk fixture"))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn actor_returns_only_the_requested_metric_group() {
        let cpu_calls = Arc::new(AtomicUsize::new(0));
        let disk_calls = Arc::new(AtomicUsize::new(0));
        let sampler = start_sampler(FakeSource {
            cpu_calls: Arc::clone(&cpu_calls),
            disk_calls: Arc::clone(&disk_calls),
        });

        let sample = sampler
            .sample(MetricGroup::Disk)
            .await
            .expect("sample disk");

        assert!(matches!(sample, SystemMetricSample::Disk(_)));
        assert_eq!(cpu_calls.load(Ordering::SeqCst), 0);
        assert_eq!(disk_calls.load(Ordering::SeqCst), 1);
        sampler.shutdown().await.expect("shut down actor");
    }

    struct BlockingSource {
        cpu_calls: Arc<AtomicUsize>,
        disk_calls: Arc<AtomicUsize>,
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl SystemMetricsSource for BlockingSource {
        fn sample_cpu_memory(&mut self) -> Result<CpuMemorySample, super::SystemSampleError> {
            self.cpu_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(entered) = self.entered.take() {
                entered.send(()).expect("report blocked sample");
                self.release.recv().expect("release blocked sample");
            }
            Ok(CpuMemorySample::try_new(
                200,
                8,
                HostCpuMemory::try_new(25.0, 1_000, 400).expect("host fixture"),
                AgentCpuMemory::try_new(5.0, 20).expect("agent fixture"),
            )
            .expect("CPU fixture"))
        }

        fn sample_disk(&mut self) -> Result<DiskSample, super::SystemSampleError> {
            self.disk_calls.fetch_add(1, Ordering::SeqCst);
            Ok(DiskSample::try_new(
                DiskScope::PcaDataVolume,
                1_000,
                500,
                50.0,
                true,
                2_147_483_648,
                Some("DISK_SPACE_LOW".to_owned()),
            )
            .expect("disk fixture"))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_queued_response_is_skipped_before_sampling() {
        let cpu_calls = Arc::new(AtomicUsize::new(0));
        let disk_calls = Arc::new(AtomicUsize::new(0));
        let (entered_sender, entered_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let sampler = Arc::new(start_sampler(BlockingSource {
            cpu_calls: Arc::clone(&cpu_calls),
            disk_calls: Arc::clone(&disk_calls),
            entered: Some(entered_sender),
            release: release_receiver,
        }));

        let first_sampler = Arc::clone(&sampler);
        let first = tokio::spawn(async move { first_sampler.sample(MetricGroup::CpuMemory).await });
        tokio::task::spawn_blocking(move || entered_receiver.recv())
            .await
            .expect("wait task")
            .expect("first sample entered");

        let canceled_sampler = Arc::clone(&sampler);
        let canceled =
            tokio::spawn(async move { canceled_sampler.sample(MetricGroup::Disk).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        canceled.abort();
        release_sender.send(()).expect("release first sample");
        first.await.expect("first task").expect("first sample");

        sampler
            .sample(MetricGroup::CpuMemory)
            .await
            .expect("barrier sample");

        assert_eq!(cpu_calls.load(Ordering::SeqCst), 2);
        assert_eq!(disk_calls.load(Ordering::SeqCst), 0);
        Arc::try_unwrap(sampler)
            .unwrap_or_else(|_| panic!("all sampler references dropped"))
            .shutdown()
            .await
            .expect("shut down actor");
    }
}
