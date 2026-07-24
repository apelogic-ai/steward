//! Vendor-neutral interfaces for every replaceable Steward plane.

use std::future::Future;

use steward_types::{AgentType, RuntimeId, RuntimeRefs};

/// Maturity derived from whether a non-fake adapter implements a port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Maturity {
    Provisional,
    Proven,
}

/// Static metadata checked by `cargo xtask ports --check`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct PortDescriptor {
    pub name: &'static str,
    pub maturity: Maturity,
}

pub const PORTS: [PortDescriptor; 8] = [
    PortDescriptor {
        name: "InferencePlane",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "ToolPlane",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "DecisionChannel",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "NotificationSink",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "SessionRelay",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "WorkloadIdentity",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "PolicySink",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "GitHostingPlane",
        maturity: Maturity::Provisional,
    },
];

/// An adapter cannot fulfill an operation or guarantee.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PortError {
    Unsupported { operation: &'static str },
    Rejected { reason: String },
    Failed { reason: String },
}

/// Desired identity for one sandbox runtime.
///
/// This is the class-B OpenShell seam, not a ninth replaceable-plane port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRequest {
    pub runtime: RuntimeId,
    pub workspace_key: String,
    pub agent_type: AgentType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxObservation {
    Absent,
    Provisioning { refs: RuntimeRefs },
    Running { refs: RuntimeRefs },
}

pub trait SandboxRuntime: Send + Sync + 'static {
    fn ensure(
        &self,
        request: &SandboxRequest,
    ) -> impl Future<Output = Result<SandboxObservation, PortError>> + Send;

    fn delete(
        &self,
        request: &SandboxRequest,
    ) -> impl Future<Output = Result<SandboxObservation, PortError>> + Send;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct InferenceCapabilities {
    pub model_allowlist: bool,
    pub spend_enforcement: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ToolCapabilities {
    pub per_principal_credentials: bool,
    pub policy_enforcement: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DecisionIntent {
    pub actor: String,
    pub evidence_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Notification {
    pub recipient: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum StreamGranularity {
    Token,
    Coalesced { interval_millis: u64 },
    Checkpoint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SessionEvent {
    Token { sequence: u64, text: String },
    ToolCallStart { sequence: u64, tool: String },
    ToolResult { sequence: u64, summary: String },
    TurnEnd { sequence: u64 },
    ParkedForApproval { sequence: u64 },
    Lagged { sequence: u64, dropped: u64 },
    SessionEnd { sequence: u64, reason: String },
}

pub trait InferencePlane {
    fn capabilities(&self) -> InferenceCapabilities;
    fn revoke_runtime(&mut self, runtime: &RuntimeId) -> Result<(), PortError>;
}

pub trait ToolPlane {
    fn capabilities(&self) -> ToolCapabilities;
    fn revoke_runtime(&mut self, runtime: &RuntimeId) -> Result<(), PortError>;
}

pub trait DecisionChannel {
    fn publish(&mut self, intent: DecisionIntent) -> Result<(), PortError>;
}

pub trait NotificationSink {
    fn notify(&mut self, notification: Notification) -> Result<(), PortError>;
}

pub trait SessionRelay {
    fn granularity(&self) -> StreamGranularity;
    fn publish(&mut self, event: SessionEvent) -> Result<(), PortError>;
}

pub trait WorkloadIdentity {
    fn attest(&mut self, runtime: &RuntimeId) -> Result<String, PortError>;
    fn revoke(&mut self, runtime: &RuntimeId) -> Result<(), PortError>;
}

pub trait PolicySink {
    fn publish_bundle(&mut self, revision: &str, bundle: &[u8]) -> Result<(), PortError>;
}

pub trait GitHostingPlane {
    fn create_snapshot(&mut self, runtime: &RuntimeId) -> Result<String, PortError>;
}
