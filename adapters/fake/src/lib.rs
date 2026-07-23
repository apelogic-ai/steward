//! Deterministic in-memory implementation of Steward's vendor-neutral ports.

use steward_ports::{
    DecisionChannel, DecisionIntent, GitHostingPlane, InferenceCapabilities, InferencePlane,
    Notification, NotificationSink, PolicySink, PortError, SessionEvent, SessionRelay,
    StreamGranularity, ToolCapabilities, ToolPlane, WorkloadIdentity,
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
    pub decisions: Vec<DecisionIntent>,
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
    fn publish(&mut self, intent: DecisionIntent) -> Result<(), PortError> {
        self.decisions.push(intent);
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
    fn attest(&mut self, runtime: &RuntimeId) -> Result<String, PortError> {
        Ok(format!("spiffe://steward.test/runtime/{}", runtime.0))
    }

    fn revoke(&mut self, runtime: &RuntimeId) -> Result<(), PortError> {
        self.revoked_runtimes.push(runtime.clone());
        Ok(())
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
