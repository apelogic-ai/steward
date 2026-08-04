//! Vendor-neutral domain types shared by Steward components.

use std::borrow::Cow;

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, CustomResourceExt};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

pub const PENDING_APPROVAL_ANNOTATION: &str = "agents.apelogic.ai/pending-approval";

pub fn runtime_activated_condition(observed_generation: i64) -> Condition {
    Condition {
        type_: "Activated".to_owned(),
        status: "True".to_owned(),
        observed_generation: Some(observed_generation),
        last_transition_time: Time(k8s_openapi::jiff::Timestamp::now()),
        reason: "PendingApprovalReleased".to_owned(),
        message: "standing delegation TTL starts at hold release".to_owned(),
    }
}

/// Stable identity for one runtime instance.
#[derive(Clone, Debug, Eq, Hash, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuntimeId(pub String);

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct Email(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Principal {
    User {
        acting_user: Email,
    },
    Service {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        acting_user: Option<Email>,
    },
}

impl JsonSchema for Principal {
    fn schema_name() -> Cow<'static, str> {
        "Principal".into()
    }

    fn schema_id() -> Cow<'static, str> {
        concat!(module_path!(), "::Principal").into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let email = generator.subschema_for::<Email>();
        json_schema!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["user", "service"]
                },
                "actingUser": email,
                "name": {
                    "type": "string",
                    "minLength": 1
                }
            },
            "required": ["kind"],
            "additionalProperties": false,
            "x-kubernetes-validations": [{
                "rule": "(self.kind == 'user' && has(self.actingUser) && !has(self.name)) || (self.kind == 'service' && has(self.name))",
                "message": "user principals require actingUser and no name; service principals require name and may carry actingUser"
            }]
        })
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentType {
    pub name: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolGrant {
    pub provider: String,
    pub resource: String,
    pub action: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Budget {
    pub monthly_limit: String,
    pub currency: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct Duration(pub String);

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(transparent)]
pub struct BindingRef(pub String);

#[derive(
    CustomResource, Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize, utoipa::ToSchema,
)]
#[kube(
    group = "agents.apelogic.ai",
    version = "v1alpha1",
    kind = "AgentRuntime",
    namespaced,
    status = "AgentRuntimeStatus",
    shortname = "ar",
    schema = "derived"
)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeSpec {
    pub principal: Principal,
    pub owner: Email,
    pub agent_type: AgentType,
    pub llms: Vec<ModelRef>,
    pub tools: Vec<ToolGrant>,
    pub budget: Budget,
    pub ttl: Duration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Vec<BindingRef>>,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentRuntimeStatus {
    pub phase: Phase,
    pub observed_generation: i64,
    pub spec_digest: String,
    pub refs: RuntimeRefs,
    pub conditions: Vec<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<SpendSummary>,
}

#[derive(Clone, Copy, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Pending,
    Admitted,
    Provisioning,
    Running,
    Suspended,
    Terminating,
    Terminated,
    Failed,
}

#[derive(Clone, Debug, Default, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeRefs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub litellm_key: Option<String>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpendSummary {
    pub observed_amount: String,
    pub currency: String,
}

pub fn agent_runtime_crd() -> CustomResourceDefinition {
    AgentRuntime::crd()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Email, Principal, agent_runtime_crd};

    #[test]
    fn principal_wire_shape_is_exclusive_and_structurally_validated() -> Result<(), String> {
        let user = serde_json::to_value(Principal::User {
            acting_user: Email("alice@example.com".to_owned()),
        })
        .map_err(|error| format!("failed to serialize user principal: {error}"))?;
        assert_eq!(
            user,
            json!({"kind": "user", "actingUser": "alice@example.com"})
        );
        assert!(
            serde_json::from_value::<Principal>(
                json!({"kind": "user", "actingUser": "alice@example.com", "name": "service-a"})
            )
            .is_err(),
            "a principal must not carry both user and service identity"
        );

        let delegated_service = serde_json::from_value::<Principal>(json!({
            "kind": "service",
            "name": "steward-run",
            "actingUser": "alice@example.com"
        }))
        .map_err(|error| format!("delegated service principal must deserialize: {error}"))?;
        let delegated_service = serde_json::to_value(delegated_service)
            .map_err(|error| format!("failed to serialize delegated service principal: {error}"))?;
        assert_eq!(
            delegated_service,
            json!({
                "kind": "service",
                "name": "steward-run",
                "actingUser": "alice@example.com"
            })
        );
        let pure_service = serde_json::to_value(Principal::Service {
            name: "scheduled-scanner".to_owned(),
            acting_user: None,
        })
        .map_err(|error| format!("failed to serialize pure service principal: {error}"))?;
        assert_eq!(
            pure_service,
            json!({"kind": "service", "name": "scheduled-scanner"})
        );

        let crd = serde_json::to_value(agent_runtime_crd())
            .map_err(|error| format!("failed to inspect AgentRuntime CRD: {error}"))?;
        let validations = crd
            .pointer(
                "/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/principal/x-kubernetes-validations",
            )
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| "Principal CRD schema must carry an exclusivity rule".to_owned())?;
        assert_eq!(
            validations.len(),
            1,
            "Principal CRD schema must have one authoritative exclusivity rule"
        );
        let rule = validations[0]
            .get("rule")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "Principal CRD validation must contain a CEL rule".to_owned())?;
        assert!(
            rule.contains("self.kind == 'service' && has(self.name)"),
            "service principals must require a service name"
        );
        assert!(
            !rule.contains("has(self.name) && !has(self.actingUser)"),
            "service principals must allow an optional resolved acting user"
        );
        Ok(())
    }
}
