#![forbid(unsafe_code)]

mod runtime;
mod sampler_actor;
mod source;

pub use runtime::{
    start_system_collector, SystemCollectorHandle, SystemObservation, CPU_MEMORY_INTERVAL,
    DISK_INTERVAL, RETRY_DELAYS,
};
pub use sampler_actor::{
    start_sampler, MetricGroup, SamplerHandle, SystemMetricsSource, SystemSampleError,
    SystemSampleErrorKind,
};
pub use source::SysinfoMetricsSource;
