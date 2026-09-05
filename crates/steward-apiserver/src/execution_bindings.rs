//! Deployment-owned logical-agent execution bindings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_types::{
    DisposableExecutionBinding, ExecutionProviderProfiles, ExecutionVersionProbe,
    TASK_EXECUTION_BINDING_DIGEST_DOMAIN, TASK_EXECUTION_BINDING_SCHEMA_VERSION,
};

const BINDING_SCHEMA: &str = TASK_EXECUTION_BINDING_SCHEMA_VERSION;
pub const EXECUTION_BINDING_CATALOG_API_VERSION: &str = "steward.execution-bindings/v1";
pub const CODEX_V1_ADAPTER: &str = "codex-v1";
pub const MAX_EXECUTION_BINDING_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_EXECUTION_BINDINGS: usize = 128;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogEntry {
    agent_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,
    adapter: String,
    image: String,
    executable: String,
    version_probe: ExecutionVersionProbe,
    provider_profiles: ExecutionProviderProfiles,
}

impl CatalogEntry {
    fn into_binding(self) -> Result<DisposableExecutionBinding, String> {
        let mut binding = DisposableExecutionBinding {
            schema_version: BINDING_SCHEMA.to_owned(),
            binding_id: format!("sha256:{}", "0".repeat(64)),
            binding_digest: format!("sha256:{}", "0".repeat(64)),
            agent_ref: self.agent_ref,
            display_name: self.display_name,
            adapter: self.adapter,
            image: self.image,
            executable: self.executable,
            version_probe: self.version_probe,
            provider_profiles: self.provider_profiles,
        };
        binding.validate()?;
        let mut digest = Sha256::new();
        digest.update(TASK_EXECUTION_BINDING_DIGEST_DOMAIN);
        digest.update(binding.canonical_content()?);
        let digest = format!("sha256:{:x}", digest.finalize());
        binding.binding_id.clone_from(&digest);
        binding.binding_digest = digest;
        Ok(binding)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogDocument {
    api_version: String,
    bindings: Vec<CatalogEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionBindingAdvertisement {
    pub agent_ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionBindingCatalog {
    by_agent: BTreeMap<String, DisposableExecutionBinding>,
}

impl ExecutionBindingCatalog {
    pub fn from_json(value: &str) -> Result<Self, String> {
        if value.len() > MAX_EXECUTION_BINDING_CATALOG_BYTES {
            return Err("execution binding catalog exceeds 1048576 bytes".to_owned());
        }
        let document = serde_json::from_str::<CatalogDocument>(value)
            .map_err(|error| format!("execution binding catalog is invalid: {error}"))?;
        if document.api_version != EXECUTION_BINDING_CATALOG_API_VERSION {
            return Err("unsupported execution binding catalog API version".to_owned());
        }
        if document.bindings.len() > MAX_EXECUTION_BINDINGS {
            return Err("execution binding catalog exceeds 128 bindings".to_owned());
        }
        let mut by_agent = BTreeMap::new();
        let mut by_identity = BTreeMap::<String, String>::new();
        for entry in document.bindings {
            if entry.adapter != CODEX_V1_ADAPTER {
                return Err(format!(
                    "execution binding {} uses unsupported adapter {}",
                    entry.agent_ref, entry.adapter
                ));
            }
            let binding = entry.into_binding()?;
            let binding_digest = binding.binding_digest.clone();
            if by_identity
                .insert(binding_digest.clone(), binding.agent_ref.clone())
                .is_some()
            {
                return Err(format!(
                    "execution binding identity {binding_digest} is duplicated"
                ));
            }
            let agent_ref = binding.agent_ref.clone();
            if by_agent.insert(agent_ref.clone(), binding).is_some() {
                return Err(format!(
                    "logical agent reference {agent_ref} has more than one binding"
                ));
            }
        }
        Ok(Self { by_agent })
    }

    pub fn resolve(&self, agent_ref: &str) -> Option<&DisposableExecutionBinding> {
        self.by_agent.get(agent_ref)
    }

    pub fn agent_refs(&self) -> Vec<String> {
        self.by_agent.keys().cloned().collect()
    }

    pub fn advertisements(&self) -> Vec<ExecutionBindingAdvertisement> {
        self.by_agent
            .values()
            .map(|binding| ExecutionBindingAdvertisement {
                agent_ref: binding.agent_ref.clone(),
                display_name: binding.display_name.clone(),
            })
            .collect()
    }

    pub fn validation_report(&self) -> Vec<ExecutionBindingValidation> {
        self.by_agent
            .values()
            .map(|binding| ExecutionBindingValidation {
                agent_ref: binding.agent_ref.clone(),
                binding_id: binding.binding_id.clone(),
                binding_digest: binding.binding_digest.clone(),
            })
            .collect()
    }

    pub fn validation_report_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&ExecutionBindingValidationDocument {
            api_version: EXECUTION_BINDING_CATALOG_API_VERSION,
            bindings: self.validation_report(),
        })
        .map_err(|error| format!("execution binding report cannot be serialized: {error}"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionBindingValidation {
    pub agent_ref: String,
    pub binding_id: String,
    pub binding_digest: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExecutionBindingValidationDocument {
    api_version: &'static str,
    bindings: Vec<ExecutionBindingValidation>,
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::ExecutionBindingCatalog;

    fn entry(agent_ref: &str, image_byte: char) -> Value {
        json!({
            "agentRef": agent_ref,
            "displayName": format!("Agent {agent_ref}"),
            "adapter": "codex-v1",
            "image": format!(
                "registry.example.test/agents/example@sha256:{}",
                image_byte.to_string().repeat(64)
            ),
            "executable": format!("/opt/example/bin/agent-{image_byte}"),
            "versionProbe": {
                "arguments": ["--version"],
                "expectedStdout": format!("example-agent {agent_ref}")
            },
            "providerProfiles": {
                "tools": {
                    "id": format!("example-tools-{agent_ref}"),
                    "digest": format!("sha256:{}", image_byte.to_string().repeat(64))
                },
                "inference": {
                    "id": format!("example-inference-{agent_ref}"),
                    "digest": format!("sha256:{}", if image_byte == 'a' { "b" } else { "c" }.repeat(64))
                }
            }
        })
    }

    fn catalog_json(entries: Vec<Value>) -> Result<String, String> {
        serde_json::to_string(&json!({
            "apiVersion": "steward.execution-bindings/v1",
            "bindings": entries
        }))
        .map_err(|error| error.to_string())
    }

    #[test]
    fn resolves_multiple_exact_logical_agents_to_distinct_immutable_bindings() -> Result<(), String>
    {
        let first = entry("agent@1.0.0", 'a');
        let second = entry("agent@2.0.0", 'b');
        let catalog = ExecutionBindingCatalog::from_json(&catalog_json(vec![first, second])?)?;

        let one = catalog
            .resolve("agent@1.0.0")
            .ok_or_else(|| "first logical agent did not resolve".to_owned())?;
        let two = catalog
            .resolve("agent@2.0.0")
            .ok_or_else(|| "second logical agent did not resolve".to_owned())?;
        assert_ne!(one.image, two.image);
        assert_ne!(one.executable, two.executable);
        assert_ne!(
            one.version_probe.expected_stdout,
            two.version_probe.expected_stdout
        );
        assert_ne!(one.provider_profiles, two.provider_profiles);
        assert_ne!(one.binding_digest, two.binding_digest);
        assert_eq!(one.binding_id, one.binding_digest);
        let one = serde_json::to_value(one).map_err(|error| error.to_string())?;
        assert_eq!(one.pointer("/adapter"), Some(&json!("codex-v1")));
        assert_eq!(
            one.pointer("/versionProbe/arguments"),
            Some(&json!(["--version"]))
        );
        assert_eq!(
            one.pointer("/providerProfiles/tools/id"),
            Some(&json!("example-tools-agent@1.0.0"))
        );
        assert_eq!(catalog.agent_refs(), ["agent@1.0.0", "agent@2.0.0"]);
        Ok(())
    }

    #[test]
    fn empty_versioned_catalog_is_valid_and_advertises_no_agents() -> Result<(), String> {
        let catalog = ExecutionBindingCatalog::from_json(&catalog_json(Vec::new())?)?;
        assert!(catalog.agent_refs().is_empty());
        Ok(())
    }

    #[test]
    fn runtime_parser_matches_the_helm_catalog_parity_fixtures() -> Result<(), String> {
        let fixture = |name: &str| {
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../charts/steward/testdata/execution-bindings")
                    .join(name),
            )
            .map_err(|error| format!("read catalog parity fixture {name}: {error}"))
        };
        ExecutionBindingCatalog::from_json(&fixture("valid.json")?)?;
        for invalid in [
            "invalid-agent-ref.json",
            "invalid-image.json",
            "invalid-display-name.json",
            "invalid-expected-stdout.json",
        ] {
            assert!(
                ExecutionBindingCatalog::from_json(&fixture(invalid)?).is_err(),
                "runtime parser accepted Helm-invalid parity fixture {invalid}"
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_mutable_incomplete_duplicate_or_unknown_catalog_entries() -> Result<(), String> {
        let valid = entry("agent@1.0.0", 'a');
        let mut cases = Vec::new();

        let mut mutable_image = valid.clone();
        mutable_image["image"] = json!("registry.example.test/agents/example:latest");
        cases.push(vec![mutable_image]);

        let mut malformed_image_path = valid.clone();
        malformed_image_path["image"] = json!(format!(
            "registry.example.test//example@sha256:{}",
            "a".repeat(64)
        ));
        cases.push(vec![malformed_image_path]);

        let mut malformed_agent = valid.clone();
        malformed_agent["agentRef"] = json!("-agent@1.0.0");
        cases.push(vec![malformed_agent]);

        let mut padded_display_name = valid.clone();
        padded_display_name["displayName"] = json!(" Example Agent");
        cases.push(vec![padded_display_name]);

        let mut padded_expected_stdout = valid.clone();
        padded_expected_stdout["versionProbe"]["expectedStdout"] = json!("example-agent 1.0.0 ");
        cases.push(vec![padded_expected_stdout]);

        let mut relative_executable = valid.clone();
        relative_executable["executable"] = json!("bin/agent");
        cases.push(vec![relative_executable]);

        let mut malformed_digest = valid.clone();
        malformed_digest["providerProfiles"]["tools"]["digest"] =
            json!(format!("sha256:{}", "A".repeat(64)));
        cases.push(vec![malformed_digest]);

        let mut unknown_adapter = valid.clone();
        unknown_adapter["adapter"] = json!("unknown-v1");
        cases.push(vec![unknown_adapter]);

        let mut invalid_probe = valid.clone();
        invalid_probe["versionProbe"]["arguments"] = json!([]);
        cases.push(vec![invalid_probe]);

        let mut unknown_field = valid.clone();
        unknown_field["credential"] = json!("not-allowed");
        cases.push(vec![unknown_field]);

        let duplicate_agent = entry("agent@1.0.0", 'b');
        cases.push(vec![valid.clone(), duplicate_agent]);

        for entries in cases {
            let catalog = catalog_json(entries)?;
            assert!(
                ExecutionBindingCatalog::from_json(&catalog).is_err(),
                "invalid deployment binding catalog must fail closed"
            );
        }
        Ok(())
    }

    #[test]
    fn binding_digest_changes_with_every_execution_or_profile_field() -> Result<(), String> {
        let original = entry("agent@1.0.0", 'a');
        let baseline = ExecutionBindingCatalog::from_json(&catalog_json(vec![original.clone()])?)?
            .resolve("agent@1.0.0")
            .ok_or_else(|| "binding did not resolve".to_owned())?
            .binding_digest
            .clone();
        for pointer in [
            "/image",
            "/executable",
            "/versionProbe/arguments/0",
            "/versionProbe/expectedStdout",
            "/providerProfiles/tools/id",
            "/providerProfiles/tools/digest",
            "/providerProfiles/inference/id",
            "/providerProfiles/inference/digest",
        ] {
            let mut changed = original.clone();
            let value = changed
                .pointer_mut(pointer)
                .ok_or_else(|| format!("missing test pointer {pointer}"))?;
            *value = match pointer {
                "/image" => json!(format!(
                    "registry.example.test/agents/changed@sha256:{}",
                    "d".repeat(64)
                )),
                "/executable" => json!("/opt/example/bin/changed-agent"),
                "/providerProfiles/tools/digest" | "/providerProfiles/inference/digest" => {
                    json!(format!("sha256:{}", "d".repeat(64)))
                }
                _ => json!("changed-value"),
            };
            let changed_digest = ExecutionBindingCatalog::from_json(&catalog_json(vec![changed])?)?
                .resolve("agent@1.0.0")
                .ok_or_else(|| "changed binding did not resolve".to_owned())?
                .binding_digest
                .clone();
            assert_ne!(baseline, changed_digest, "{pointer} was not content-bound");
        }

        let mut presentation_change = original;
        presentation_change["displayName"] = json!("Renamed Example Agent");
        let presentation_digest =
            ExecutionBindingCatalog::from_json(&catalog_json(vec![presentation_change])?)?
                .resolve("agent@1.0.0")
                .ok_or_else(|| "presentation-only binding did not resolve".to_owned())?
                .binding_digest
                .clone();
        assert_eq!(
            baseline, presentation_digest,
            "presentation-only displayName must not change execution identity"
        );
        Ok(())
    }
}
