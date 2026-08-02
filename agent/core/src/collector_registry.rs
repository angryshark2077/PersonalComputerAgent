use pca_agentd::communication::{OUTBOX_HIGH_WATER, OUTBOX_LOW_WATER};
use pca_domain::{CollectorState, CollectorStatus, SystemMetricSample};
use pca_system_collector::{MetricGroup, SystemSampleError, SystemSampleErrorKind};
use uuid::Uuid;

const COLLECTOR_KEY: &str = "system";
const COLLECTOR_VERSION: &str = env!("CARGO_PKG_VERSION");
const COLLECTOR_DEGRADED: &str = "COLLECTOR_DEGRADED";
const COLLECTOR_UNSUPPORTED: &str = "COLLECTOR_UNSUPPORTED";
const COLLECTOR_INIT_FAILED: &str = "COLLECTOR_INIT_FAILED";
pub(crate) const DISK_SPACE_LOW: &str = "DISK_SPACE_LOW";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CollectorIdentity {
    pub(crate) workspace_id: Uuid,
    pub(crate) device_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CollectorStatusTransition {
    pub(crate) previous_status: CollectorStatus,
    pub(crate) status: CollectorStatus,
    pub(crate) desired_config_revision: u64,
    pub(crate) applied_config_revision: u64,
    pub(crate) reason: &'static str,
    pub(crate) error_code: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskHealthChange {
    pub(crate) active: bool,
    pub(crate) available_bytes: u64,
    pub(crate) threshold_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryUpdate {
    pub(crate) state: CollectorState,
    pub(crate) transition: Option<CollectorStatusTransition>,
    pub(crate) health_change: Option<DiskHealthChange>,
    pub(crate) sampling_suppressed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupState {
    Pending,
    Healthy,
    RetryableFailure,
    Unsupported,
    Fatal,
}

pub(crate) struct CollectorRegistry {
    state: CollectorState,
    identity_available: bool,
    cpu_memory: GroupState,
    disk: GroupState,
    backpressure: bool,
    persistence_failed: bool,
    disk_low: Option<bool>,
}

impl CollectorRegistry {
    pub(crate) fn restore(
        prior: Option<CollectorState>,
        identity_available: bool,
        _outbox_depth: u64,
        now_ms: i64,
    ) -> (Self, RegistryUpdate) {
        let previous_status = prior
            .as_ref()
            .map_or(CollectorStatus::Disabled, |state| state.status);
        let mut state = prior.unwrap_or_else(|| CollectorState {
            collector_key: COLLECTOR_KEY.to_owned(),
            collector_version: COLLECTOR_VERSION.to_owned(),
            status: CollectorStatus::Disabled,
            desired_config_revision: 0,
            applied_config_revision: 0,
            last_event_at_ms: None,
            last_health_at_ms: None,
            last_error_code: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        });
        COLLECTOR_KEY.clone_into(&mut state.collector_key);
        COLLECTOR_VERSION.clone_into(&mut state.collector_version);
        state.status = if identity_available {
            CollectorStatus::Initializing
        } else {
            CollectorStatus::Disabled
        };
        if !identity_available {
            state.desired_config_revision = 0;
            state.applied_config_revision = 0;
        }
        state.last_error_code = None;
        state.updated_at_ms = now_ms;

        let mut registry = Self {
            state,
            identity_available,
            cpu_memory: GroupState::Pending,
            disk: GroupState::Pending,
            backpressure: false,
            persistence_failed: false,
            disk_low: None,
        };
        registry.refresh_status();
        let transition = (previous_status != registry.state.status && identity_available)
            .then(|| registry.transition(previous_status, "identity_available"));
        let update = registry.update(transition, None);
        (registry, update)
    }

    pub(crate) fn record_sample(
        &mut self,
        sample: &SystemMetricSample,
        occurred_at_ms: i64,
    ) -> RegistryUpdate {
        let previous_status = self.state.status;
        match sample {
            SystemMetricSample::CpuMemory(_) => self.cpu_memory = GroupState::Healthy,
            SystemMetricSample::Disk(_) => self.disk = GroupState::Healthy,
        }
        self.state.last_event_at_ms = Some(occurred_at_ms);
        self.state.last_health_at_ms = Some(occurred_at_ms);
        self.state.updated_at_ms = occurred_at_ms;
        self.refresh_status();

        let transition = (previous_status != self.state.status).then(|| {
            let reason = if previous_status == CollectorStatus::Initializing
                && self.state.status == CollectorStatus::Running
            {
                "initial_samples_succeeded"
            } else {
                "sampling_recovered"
            };
            self.transition(previous_status, reason)
        });
        let health_change = match sample {
            SystemMetricSample::CpuMemory(_) => None,
            SystemMetricSample::Disk(disk) => {
                let active = disk.low_space();
                let changed = match self.disk_low {
                    None => active,
                    Some(previous) => previous != active,
                };
                self.disk_low = Some(active);
                changed.then(|| DiskHealthChange {
                    active,
                    available_bytes: disk.available_bytes(),
                    threshold_bytes: disk.low_space_threshold_bytes(),
                })
            }
        };

        self.update(transition, health_change)
    }

    pub(crate) fn record_failure(
        &mut self,
        group: MetricGroup,
        error: &SystemSampleError,
        observed_at_ms: i64,
    ) -> RegistryUpdate {
        let previous_status = self.state.status;
        let group_state = match error.kind {
            SystemSampleErrorKind::Retryable => GroupState::RetryableFailure,
            SystemSampleErrorKind::Unsupported => GroupState::Unsupported,
            SystemSampleErrorKind::Fatal => GroupState::Fatal,
        };
        match group {
            MetricGroup::CpuMemory => self.cpu_memory = group_state,
            MetricGroup::Disk => self.disk = group_state,
        }
        self.state.last_health_at_ms = Some(observed_at_ms);
        self.state.updated_at_ms = observed_at_ms;
        self.refresh_status();
        let reason = match error.kind {
            SystemSampleErrorKind::Retryable => "sampling_failed",
            SystemSampleErrorKind::Unsupported => "collector_unsupported",
            SystemSampleErrorKind::Fatal => "collector_error",
        };
        let transition = (previous_status != self.state.status)
            .then(|| self.transition(previous_status, reason));
        self.update(transition, None)
    }

    pub(crate) fn apply_outbox_depth(&mut self, depth: u64, now_ms: i64) -> RegistryUpdate {
        let previous_status = self.state.status;
        let previous_backpressure = self.backpressure;
        if depth > OUTBOX_HIGH_WATER {
            self.backpressure = true;
        } else if depth < OUTBOX_LOW_WATER {
            self.backpressure = false;
        }
        self.state.updated_at_ms = now_ms;
        self.refresh_status();
        let transition = (previous_status != self.state.status).then(|| {
            let reason = if !previous_backpressure && self.backpressure {
                "outbox_backpressure"
            } else {
                "outbox_recovered"
            };
            self.transition(previous_status, reason)
        });
        self.update(transition, None)
    }

    pub(crate) fn record_persistence_failure(&mut self, now_ms: i64) -> RegistryUpdate {
        let previous_status = self.state.status;
        self.persistence_failed = true;
        self.state.updated_at_ms = now_ms;
        self.refresh_status();
        let transition = (previous_status != self.state.status)
            .then(|| self.transition(previous_status, "persistence_failed"));
        self.update(transition, None)
    }

    pub(crate) fn record_persistence_recovery(&mut self, now_ms: i64) -> RegistryUpdate {
        let previous_status = self.state.status;
        self.persistence_failed = false;
        self.state.updated_at_ms = now_ms;
        self.refresh_status();
        let transition = (previous_status != self.state.status)
            .then(|| self.transition(previous_status, "persistence_recovered"));
        self.update(transition, None)
    }

    pub(crate) const fn persistence_failed(&self) -> bool {
        self.persistence_failed
    }

    pub(crate) const fn sampling_suppressed(&self) -> bool {
        self.persistence_failed || self.backpressure
    }

    fn refresh_status(&mut self) {
        self.state.status = if !self.identity_available {
            CollectorStatus::Disabled
        } else if matches!(self.cpu_memory, GroupState::Fatal)
            || matches!(self.disk, GroupState::Fatal)
        {
            CollectorStatus::Error
        } else if matches!(self.cpu_memory, GroupState::Unsupported)
            || matches!(self.disk, GroupState::Unsupported)
        {
            CollectorStatus::Unsupported
        } else if self.persistence_failed
            || self.backpressure
            || matches!(self.cpu_memory, GroupState::RetryableFailure)
            || matches!(self.disk, GroupState::RetryableFailure)
        {
            CollectorStatus::Degraded
        } else if self.cpu_memory == GroupState::Healthy && self.disk == GroupState::Healthy {
            CollectorStatus::Running
        } else {
            CollectorStatus::Initializing
        };
        self.state.last_error_code = self.active_error_code().map(str::to_owned);
    }

    fn active_error_code(&self) -> Option<&'static str> {
        if matches!(self.cpu_memory, GroupState::Fatal) || matches!(self.disk, GroupState::Fatal) {
            Some(COLLECTOR_INIT_FAILED)
        } else if matches!(self.cpu_memory, GroupState::Unsupported)
            || matches!(self.disk, GroupState::Unsupported)
        {
            Some(COLLECTOR_UNSUPPORTED)
        } else if self.persistence_failed
            || self.backpressure
            || matches!(self.cpu_memory, GroupState::RetryableFailure)
            || matches!(self.disk, GroupState::RetryableFailure)
        {
            Some(COLLECTOR_DEGRADED)
        } else {
            None
        }
    }

    fn transition(
        &self,
        previous_status: CollectorStatus,
        reason: &'static str,
    ) -> CollectorStatusTransition {
        CollectorStatusTransition {
            previous_status,
            status: self.state.status,
            desired_config_revision: self.state.desired_config_revision,
            applied_config_revision: self.state.applied_config_revision,
            reason,
            error_code: self.active_error_code(),
        }
    }

    fn update(
        &self,
        transition: Option<CollectorStatusTransition>,
        health_change: Option<DiskHealthChange>,
    ) -> RegistryUpdate {
        RegistryUpdate {
            state: self.state.clone(),
            transition,
            health_change,
            sampling_suppressed: self.sampling_suppressed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CollectorRegistry, DISK_SPACE_LOW};
    use pca_domain::{
        AgentCpuMemory, CollectorState, CollectorStatus, CpuMemorySample, DiskSample, DiskScope,
        HostCpuMemory, SystemMetricSample,
    };
    use pca_system_collector::{MetricGroup, SystemSampleError, SystemSampleErrorKind};

    const NOW: i64 = 1_700_000_000_000;

    fn prior_state(status: CollectorStatus) -> CollectorState {
        CollectorState {
            collector_key: "system".to_owned(),
            collector_version: "0.1.0".to_owned(),
            status,
            desired_config_revision: 7,
            applied_config_revision: 6,
            last_event_at_ms: Some(NOW - 3_000),
            last_health_at_ms: Some(NOW - 2_000),
            last_error_code: Some("OLD_ERROR".to_owned()),
            created_at_ms: NOW - 10_000,
            updated_at_ms: NOW - 1_000,
        }
    }

    fn cpu_memory_sample() -> SystemMetricSample {
        SystemMetricSample::CpuMemory(
            CpuMemorySample::try_new(
                30_000,
                8,
                HostCpuMemory::try_new(25.0, 16_000, 8_000).expect("host fixture"),
                AgentCpuMemory::try_new(2.0, 128).expect("agent fixture"),
            )
            .expect("CPU fixture"),
        )
    }

    fn disk_sample(available_bytes: u64) -> SystemMetricSample {
        let total_bytes = 8_589_934_592;
        let low_space = available_bytes < 2_147_483_648;
        let total_kib = u32::try_from(total_bytes / 1_024).expect("fixture total KiB");
        let available_kib = u32::try_from(available_bytes / 1_024).expect("fixture available KiB");
        let used_percent = (f64::from(total_kib - available_kib) / f64::from(total_kib)) * 100.0;
        SystemMetricSample::Disk(
            DiskSample::try_new(
                DiskScope::PcaDataVolume,
                total_bytes,
                available_bytes,
                used_percent,
                low_space,
                2_147_483_648,
                low_space.then(|| DISK_SPACE_LOW.to_owned()),
            )
            .expect("disk fixture"),
        )
    }

    fn sampling_error(kind: SystemSampleErrorKind) -> SystemSampleError {
        SystemSampleError {
            kind,
            code: "SOURCE_DETAIL_REDACTED",
            message: "fixture failure".to_owned(),
        }
    }

    fn running_registry() -> CollectorRegistry {
        let (mut registry, initial) = CollectorRegistry::restore(None, true, 0, NOW);
        assert_eq!(initial.state.status, CollectorStatus::Initializing);
        registry.record_sample(&cpu_memory_sample(), NOW + 1);
        let running = registry.record_sample(&disk_sample(4_294_967_296), NOW + 2);
        assert_eq!(running.state.status, CollectorStatus::Running);
        registry
    }

    #[test]
    fn unpaired_restore_is_disabled_without_transition_and_resets_revisions() {
        let (_registry, update) =
            CollectorRegistry::restore(Some(prior_state(CollectorStatus::Running)), false, 0, NOW);

        assert_eq!(update.state.desired_config_revision, 0);
        assert_eq!(update.state.applied_config_revision, 0);
        assert_eq!(update.state.status, CollectorStatus::Disabled);
        assert_eq!(update.state.updated_at_ms, NOW);
        assert!(update.transition.is_none());
        assert!(update.health_change.is_none());
        assert!(!update.sampling_suppressed);
    }

    #[test]
    fn paired_restore_recomputes_initializing_and_waits_for_both_groups() {
        let (mut registry, initial) = CollectorRegistry::restore(
            Some(prior_state(CollectorStatus::Running)),
            true,
            9_000,
            NOW,
        );

        assert_eq!(initial.state.status, CollectorStatus::Initializing);
        assert_eq!(
            initial
                .transition
                .as_ref()
                .expect("startup transition")
                .previous_status,
            CollectorStatus::Running
        );
        assert_eq!(
            initial
                .transition
                .as_ref()
                .expect("startup transition")
                .reason,
            "identity_available"
        );
        assert!(!initial.sampling_suppressed);

        let first = registry.record_sample(&cpu_memory_sample(), NOW + 1);
        assert_eq!(first.state.status, CollectorStatus::Initializing);
        assert!(first.transition.is_none());

        let second = registry.record_sample(&disk_sample(4_294_967_296), NOW + 2);
        assert_eq!(second.state.status, CollectorStatus::Running);
        assert_eq!(
            second.transition.expect("running transition").reason,
            "initial_samples_succeeded"
        );
    }

    #[test]
    fn backpressure_uses_hysteresis_and_restart_uses_high_water_only() {
        let mut registry = running_registry();

        assert!(
            !registry
                .apply_outbox_depth(10_000, NOW + 3)
                .sampling_suppressed
        );
        let entered = registry.apply_outbox_depth(10_001, NOW + 4);
        assert!(entered.sampling_suppressed);
        assert_eq!(
            entered.transition.expect("degraded transition").reason,
            "outbox_backpressure"
        );
        assert!(
            registry
                .apply_outbox_depth(8_000, NOW + 5)
                .sampling_suppressed
        );
        let recovered = registry.apply_outbox_depth(7_999, NOW + 6);
        assert!(!recovered.sampling_suppressed);
        assert_eq!(
            recovered.transition.expect("running transition").reason,
            "outbox_recovered"
        );

        assert!(
            !CollectorRegistry::restore(
                Some(prior_state(CollectorStatus::Degraded)),
                true,
                9_000,
                NOW,
            )
            .1
            .sampling_suppressed
        );
    }

    #[test]
    fn one_recovery_does_not_clear_another_degradation_reason() {
        let mut registry = running_registry();
        let failed = registry.record_failure(
            MetricGroup::CpuMemory,
            &sampling_error(SystemSampleErrorKind::Retryable),
            NOW + 3,
        );
        assert_eq!(failed.state.status, CollectorStatus::Degraded);
        registry.apply_outbox_depth(10_001, NOW + 4);

        let sampled = registry.record_sample(&cpu_memory_sample(), NOW + 5);

        assert_eq!(sampled.state.status, CollectorStatus::Degraded);
        assert!(sampled.transition.is_none());
        assert_eq!(
            sampled.state.last_error_code.as_deref(),
            Some("COLLECTOR_DEGRADED")
        );
    }

    #[test]
    fn failure_classification_and_complete_recovery_are_canonical() {
        let cases = [
            (
                SystemSampleErrorKind::Retryable,
                CollectorStatus::Degraded,
                "sampling_failed",
                "COLLECTOR_DEGRADED",
            ),
            (
                SystemSampleErrorKind::Unsupported,
                CollectorStatus::Unsupported,
                "collector_unsupported",
                "COLLECTOR_UNSUPPORTED",
            ),
            (
                SystemSampleErrorKind::Fatal,
                CollectorStatus::Error,
                "collector_error",
                "COLLECTOR_INIT_FAILED",
            ),
        ];

        for (kind, expected_status, expected_reason, expected_code) in cases {
            let mut registry = running_registry();
            let first = registry.record_failure(MetricGroup::Disk, &sampling_error(kind), NOW + 3);
            let transition = first.transition.expect("status edge");
            assert_eq!(transition.status, expected_status);
            assert_eq!(transition.reason, expected_reason);
            assert_eq!(transition.error_code, Some(expected_code));
            assert_eq!(first.state.last_error_code.as_deref(), Some(expected_code));

            let repeated =
                registry.record_failure(MetricGroup::Disk, &sampling_error(kind), NOW + 4);
            assert!(repeated.transition.is_none());
            assert_eq!(repeated.state.last_event_at_ms, Some(NOW + 2));
            assert_eq!(repeated.state.last_health_at_ms, Some(NOW + 4));

            let recovered = registry.record_sample(&disk_sample(4_294_967_296), NOW + 5);
            assert_eq!(recovered.state.status, CollectorStatus::Running);
            assert_eq!(
                recovered.transition.expect("recovery edge").reason,
                "sampling_recovered"
            );
            assert_eq!(recovered.state.last_error_code, None);
        }
    }

    #[test]
    fn disk_health_emits_only_edges_and_reasserts_low_after_restart() {
        let (mut registry, _) = CollectorRegistry::restore(None, true, 0, NOW);

        let healthy = registry.record_sample(&disk_sample(4_294_967_296), NOW + 1);
        assert!(healthy.health_change.is_none());

        let low = registry.record_sample(&disk_sample(1_073_741_824), NOW + 2);
        let active = low.health_change.expect("low disk edge");
        assert!(active.active);
        assert_eq!(active.available_bytes, 1_073_741_824);
        assert_eq!(active.threshold_bytes, 2_147_483_648);
        assert_eq!(low.state.last_event_at_ms, Some(NOW + 2));

        let repeated = registry.record_sample(&disk_sample(536_870_912), NOW + 3);
        assert!(repeated.health_change.is_none());

        let recovered = registry.record_sample(&disk_sample(4_294_967_296), NOW + 4);
        assert!(!recovered.health_change.expect("disk recovery edge").active);

        let (mut restarted, _) =
            CollectorRegistry::restore(Some(recovered.state), true, 0, NOW + 5);
        assert!(
            restarted
                .record_sample(&disk_sample(1_073_741_824), NOW + 6)
                .health_change
                .expect("restart reassertion")
                .active
        );
    }

    #[test]
    fn persistence_reason_suppresses_sampling_until_its_own_recovery() {
        let mut registry = running_registry();
        registry.record_failure(
            MetricGroup::CpuMemory,
            &sampling_error(SystemSampleErrorKind::Retryable),
            NOW + 3,
        );

        let failed = registry.record_persistence_failure(NOW + 4);
        assert!(failed.sampling_suppressed);
        assert_eq!(failed.state.status, CollectorStatus::Degraded);
        assert!(failed.transition.is_none());

        let recovered = registry.record_persistence_recovery(NOW + 5);
        assert!(!recovered.sampling_suppressed);
        assert_eq!(recovered.state.status, CollectorStatus::Degraded);
        assert!(recovered.transition.is_none());
        assert_eq!(
            recovered.state.last_error_code.as_deref(),
            Some("COLLECTOR_DEGRADED")
        );
    }
}
