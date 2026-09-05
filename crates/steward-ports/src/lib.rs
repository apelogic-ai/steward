//! Vendor-neutral interfaces for every replaceable Steward plane.

use std::future::Future;

use steward_types::{
    AgentType, Budget, DisposableExecutionBinding, ModelRef, RuntimeId, RuntimeRefs, SpendSummary,
    ToolGrant,
};

/// Maximum raw tar body accepted by the Task input endpoint.
pub const MAX_TASK_INPUT_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum raw tar body returned by a Task runtime and persisted as output.
pub const MAX_TASK_OUTPUT_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

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
        maturity: Maturity::Proven,
    },
    PortDescriptor {
        name: "ToolPlane",
        maturity: Maturity::Provisional,
    },
    PortDescriptor {
        name: "DecisionChannel",
        maturity: Maturity::Proven,
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
        maturity: Maturity::Proven,
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

/// An untrusted workload assertion. Deliberately implements neither `Debug` nor `Display`.
pub struct SvidAssertion(String);

impl SvidAssertion {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for SvidAssertion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedWorkload {
    pub spiffe_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SvidValidationError {
    Rejected,
    Expired,
    Unavailable,
}

/// Desired identity for one sandbox runtime.
///
/// This is the class-B OpenShell seam, not a ninth replaceable-plane port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxExecutionClass {
    /// An ordinary or long-running agent runtime.
    Agent,
    /// A short-lived, server-authored provider-control operation.
    ProviderControl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxRequest {
    pub runtime: RuntimeId,
    pub workspace_key: String,
    pub execution_class: SandboxExecutionClass,
    pub agent_type: AgentType,
    pub models: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub refs: RuntimeRefs,
    /// Immutable deployment binding persisted with a disposable Task.
    ///
    /// Legacy and resident runtimes do not carry this value.
    pub execution_binding: Option<DisposableExecutionBinding>,
}

/// Controller-owned execution bindings for a short-lived provider-control runtime.
///
/// These values are persisted with the operation and compared with the live adapter
/// configuration before provisioning or execution. They are never sourced from a browser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderControlExecutionBindings {
    pub artifact_trust_mode: String,
    pub bridge_image_digest: String,
    pub mcp_gw_origin: String,
    pub mcp_gw_version: String,
    pub namespace: String,
    pub runtime_class: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxObservation {
    Absent,
    Provisioning { refs: RuntimeRefs },
    Running { refs: RuntimeRefs },
}

pub trait SandboxRuntime: Send + Sync + 'static {
    fn provider_control_bindings(&self) -> Option<ProviderControlExecutionBindings> {
        None
    }

    fn ensure(
        &self,
        request: &SandboxRequest,
    ) -> impl Future<Output = Result<SandboxObservation, PortError>> + Send;

    fn delete(
        &self,
        request: &SandboxRequest,
    ) -> impl Future<Output = Result<SandboxObservation, PortError>> + Send;
}

/// One server-selected single-shot command over an opaque input archive.
///
/// This extends the class-B sandbox seam; it is not a replaceable plane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxTaskRequest {
    pub runtime: RuntimeId,
    pub refs: RuntimeRefs,
    /// Server-derived execution class. Browsers and task payloads cannot select it.
    pub execution_class: SandboxExecutionClass,
    /// The controller reads this only from the persisted runtime spec; callers cannot choose it.
    pub agent_type: AgentType,
    pub command: Vec<String>,
    /// The exact deployment binding persisted when the Task was reserved.
    pub execution_binding: Option<DisposableExecutionBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxTaskOutput {
    pub archive: Vec<u8>,
}

pub trait SandboxTaskRuntime: Send + Sync + 'static {
    fn provider_control_bindings(&self) -> Option<ProviderControlExecutionBindings> {
        None
    }

    fn run_task(
        &self,
        request: &SandboxTaskRequest,
        input_archive: &[u8],
    ) -> impl Future<Output = Result<SandboxTaskOutput, PortError>> + Send;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct InferenceCapabilities {
    pub model_allowlist: bool,
    pub spend_enforcement: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InferenceRequest {
    pub runtime: RuntimeId,
    pub models: Vec<ModelRef>,
    pub budget: Budget,
}

/// A bearer credential returned by an inference plane.
///
/// Deliberately implements neither `Debug` nor `Display`.
pub struct InferenceCredential(String);

impl InferenceCredential {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

pub struct ProvisionedInference {
    pub reference: String,
    pub credential: InferenceCredential,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InferenceObservation {
    Absent,
    Active {
        reference: String,
        spend: SpendSummary,
    },
    Exhausted {
        reference: String,
        spend: SpendSummary,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ToolCapabilities {
    pub per_principal_credentials: bool,
    pub policy_enforcement: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionRequest {
    pub request_id: String,
    pub runtime_uid: String,
    pub actor: String,
    pub member_role: String,
    pub counterexample: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionReference {
    pub key: String,
    pub evidence_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DecisionResolution {
    pub request_id: String,
    pub key: String,
    pub decided_by: String,
    pub rationale: String,
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

pub trait InferencePlane: Send + Sync + 'static {
    fn capabilities(&self) -> InferenceCapabilities;

    fn validate_configuration(
        &self,
        models: &[ModelRef],
        budget: &Budget,
    ) -> impl Future<Output = Result<(), PortError>> + Send;

    fn provision(
        &self,
        request: &InferenceRequest,
    ) -> impl Future<Output = Result<ProvisionedInference, PortError>> + Send;

    fn reconcile_configuration(
        &self,
        request: &InferenceRequest,
    ) -> impl Future<Output = Result<(), PortError>> + Send;

    fn observe(
        &self,
        request: &InferenceRequest,
    ) -> impl Future<Output = Result<InferenceObservation, PortError>> + Send;

    fn revoke(
        &self,
        request: &InferenceRequest,
    ) -> impl Future<Output = Result<(), PortError>> + Send;
}

pub trait ToolPlane {
    fn capabilities(&self) -> ToolCapabilities;
    fn revoke_runtime(&mut self, runtime: &RuntimeId) -> Result<(), PortError>;
}

pub trait DecisionChannel: Send + Sync + 'static {
    fn request(
        &self,
        request: &DecisionRequest,
    ) -> impl Future<Output = Result<DecisionReference, PortError>> + Send;

    fn record_resolution(
        &self,
        resolution: &DecisionResolution,
    ) -> impl Future<Output = Result<(), PortError>> + Send;
}

pub trait NotificationSink {
    fn notify(&mut self, notification: Notification) -> Result<(), PortError>;
}

pub trait SessionRelay {
    fn granularity(&self) -> StreamGranularity;
    fn publish(&mut self, event: SessionEvent) -> Result<(), PortError>;
}

pub trait WorkloadIdentity: Send + Sync + 'static {
    fn validate(
        &self,
        audience: &str,
        assertion: &SvidAssertion,
    ) -> impl Future<Output = Result<ValidatedWorkload, SvidValidationError>> + Send;
}

pub trait PolicySink {
    fn publish_bundle(&mut self, revision: &str, bundle: &[u8]) -> Result<(), PortError>;
}

pub trait GitHostingPlane {
    fn create_snapshot(&mut self, runtime: &RuntimeId) -> Result<String, PortError>;
}
