//! Deterministic in-memory implementation of Steward's vendor-neutral ports.

use std::sync::Mutex;

use steward_ports::{
    DecisionChannel, DecisionReference, DecisionRequest, DecisionResolution, GitHostingPlane,
    InferenceCapabilities, InferencePlane, Notification, NotificationSink, PolicySink, PortError,
    SessionEvent, SessionRelay, StreamGranularity, SvidAssertion, SvidValidationError,
    ToolCapabilities, ToolPlane, ValidatedWorkload, WorkloadIdentity,
};
use steward_types::RuntimeId;

pub const IMPLEMENTED_PORTS: [&str; 8] = [
    "InferencePlane",
    "ToolPlane",
    "DecisionChannel",
    "NotificationSink",
    "SessionRelay",
    "WorkloadIdentity",
    "PolicySink",
    "GitHostingPlane",
];

#[derive(Debug, Default)]
pub struct FakeAdapter {
    pub decision_requests: Mutex<Vec<DecisionRequest>>,
    pub decision_resolutions: Mutex<Vec<DecisionResolution>>,
    pub notifications: Vec<Notification>,
    pub events: Vec<SessionEvent>,
    pub revoked_runtimes: Vec<RuntimeId>,
    pub policy_revisions: Vec<String>,
}

impl InferencePlane for FakeAdapter {
    fn capabilities(&self) -> InferenceCapabilities {
        let mut capabilities = InferenceCapabilities::default();
        capabilities.model_allowlist = true;
        capabilities.spend_enforcement = true;
        capabilities
    }

    fn revoke_runtime(&mut self, runtime: &RuntimeId) -> Result<(), PortError> {
        self.revoked_runtimes.push(runtime.clone());
        Ok(())
    }
}

impl ToolPlane for FakeAdapter {
    fn capabilities(&self) -> ToolCapabilities {
        let mut capabilities = ToolCapabilities::default();
        capabilities.per_principal_credentials = true;
        capabilities.policy_enforcement = true;
        capabilities
    }

    fn revoke_runtime(&mut self, runtime: &RuntimeId) -> Result<(), PortError> {
        self.revoked_runtimes.push(runtime.clone());
        Ok(())
    }
}

impl DecisionChannel for FakeAdapter {
    async fn request(&self, request: &DecisionRequest) -> Result<DecisionReference, PortError> {
        self.decision_requests
            .lock()
            .map_err(|_| PortError::Failed {
                reason: "fake decision-request lock was poisoned".to_owned(),
            })?
            .push(request.clone());
        Ok(DecisionReference {
            key: "PROJ-123".to_owned(),
            evidence_url: "https://jira.example.com/browse/PROJ-123".to_owned(),
        })
    }

    async fn record_resolution(&self, resolution: &DecisionResolution) -> Result<(), PortError> {
        self.decision_resolutions
            .lock()
            .map_err(|_| PortError::Failed {
                reason: "fake decision-resolution lock was poisoned".to_owned(),
            })?
            .push(resolution.clone());
        Ok(())
    }
}

impl NotificationSink for FakeAdapter {
    fn notify(&mut self, notification: Notification) -> Result<(), PortError> {
        self.notifications.push(notification);
        Ok(())
    }
}

impl SessionRelay for FakeAdapter {
    fn granularity(&self) -> StreamGranularity {
        StreamGranularity::Token
    }

    fn publish(&mut self, event: SessionEvent) -> Result<(), PortError> {
        self.events.push(event);
        Ok(())
    }
}

impl WorkloadIdentity for FakeAdapter {
    async fn validate(
        &self,
        _audience: &str,
        _assertion: &SvidAssertion,
    ) -> Result<ValidatedWorkload, SvidValidationError> {
        Ok(ValidatedWorkload {
            spiffe_id: "spiffe://steward.test/runtime/runtime-a".to_owned(),
        })
    }
}

impl PolicySink for FakeAdapter {
    fn publish_bundle(&mut self, revision: &str, _bundle: &[u8]) -> Result<(), PortError> {
        self.policy_revisions.push(revision.to_owned());
        Ok(())
    }
}

impl GitHostingPlane for FakeAdapter {
    fn create_snapshot(&mut self, runtime: &RuntimeId) -> Result<String, PortError> {
        Ok(format!("snapshot-{}", runtime.0))
    }
}
