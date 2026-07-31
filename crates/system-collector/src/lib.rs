#![forbid(unsafe_code)]

mod sampler_actor;
mod source;

pub use sampler_actor::{
    start_sampler, MetricGroup, SamplerHandle, SystemMetricsSource, SystemSampleError,
    SystemSampleErrorKind,
};
pub use source::SysinfoMetricsSource;
