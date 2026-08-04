#![forbid(unsafe_code)]

use std::sync::{
    atomic::{AtomicBool, Ordering},
    RwLock,
};

pub mod auth;
mod client;
pub mod framing;
pub mod supervisor;

pub use supervisor::{
    screen_capture_command_channel, ScreenCaptureCommandHandle, ScreenCaptureCommandReceiver,
};

pub use client::{
    BridgeClient, BridgeClientConfig, BridgeClientError, DeviceLocationObservation,
    NetworkObservation, PlatformLifecycleEvent, ScreenCaptureResult, ScreenCaptureStatus,
    ScreenContext, PROTOCOL_VERSION,
};

#[derive(Debug, Default)]
pub struct NetworkObservationState {
    enabled: AtomicBool,
    latest: RwLock<Option<NetworkObservation>>,
}

impl NetworkObservationState {
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
        if !enabled {
            if let Ok(mut latest) = self.latest.write() {
                *latest = None;
            }
        }
    }

    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn replace(&self, observation: NetworkObservation) {
        if let Ok(mut latest) = self.latest.write() {
            *latest = Some(observation);
        }
    }

    #[must_use]
    pub fn current_if_enabled(&self) -> Option<NetworkObservation> {
        if !self.is_enabled() {
            return None;
        }
        self.latest.read().ok().and_then(|latest| latest.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{NetworkObservation, NetworkObservationState};

    #[test]
    fn network_state_stays_private_until_enabled_and_disables_immediately() {
        let state = NetworkObservationState::default();
        state.replace(NetworkObservation {
            interface_type: "wired".to_owned(),
            wifi_identity_available: false,
            ssid: None,
            bssid: None,
            local_ipv4: Some("192.168.1.5".to_owned()),
            local_ipv6: None,
            location: None,
        });
        assert!(state.current_if_enabled().is_none());
        state.set_enabled(true);
        assert_eq!(
            state.current_if_enabled().unwrap().local_ipv4.as_deref(),
            Some("192.168.1.5")
        );
        state.set_enabled(false);
        assert!(state.current_if_enabled().is_none());
        state.set_enabled(true);
        assert!(state.current_if_enabled().is_none());
    }
}
