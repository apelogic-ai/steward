//! Deployment-owned logical-agent execution bindings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use steward_types::{DisposableExecutionBinding, TASK_EXECUTION_BINDING_SCHEMA_VERSION};

const BINDING_SCHEMA: &str = TASK_EXECUTION_BINDING_SCHEMA_VERSION;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogEntry {
    binding_id: String,
    binding_digest: String,
    agent_ref: String,
    image: String,
    executable: String,
    expected_version: String,
    native_profile: String,
}

impl CatalogEntry {
    fn content_digest(&self) -> String {
        let mut digest = Sha256::new();
        for value in [
            BINDING_SCHEMA,
            &self.agent_ref,
            &self.image,
            &self.executable,
            &self.expected_version,
            &self.native_profile,
        ] {
            digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            digest.update(value.as_bytes());
        }
        format!("sha256:{:x}", digest.finalize())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionBindingCatalog {
    by_agent: BTreeMap<String, DisposableExecutionBinding>,
}

impl ExecutionBindingCatalog {
    pub fn from_json(value: &str) -> Result<Self, String> {
        let entries = serde_json::from_str::<Vec<CatalogEntry>>(value)
            .map_err(|error| format!("execution binding catalog is invalid: {error}"))?;
        if entries.is_empty() {
            return Err("execution binding catalog must contain at least one binding".to_owned());
        }
        let mut by_agent = BTreeMap::new();
        let mut by_identity = BTreeMap::<String, String>::new();
        for entry in entries {
            if entry.binding_digest != entry.content_digest()
                || entry.binding_id != entry.binding_digest
            {
                return Err(format!(
                    "execution binding {} digest does not match its immutable content",
                    entry.binding_id
                ));
            }
            if by_identity
                .insert(entry.binding_id.clone(), entry.binding_digest.clone())
                .is_some()
            {
                return Err(format!(
                    "execution binding identity {} is duplicated",
                    entry.binding_id
                ));
            }
            let agent_ref = entry.agent_ref.clone();
            let binding = DisposableExecutionBinding {
                schema_version: BINDING_SCHEMA.to_owned(),
                binding_id: entry.binding_id,
                binding_digest: entry.binding_digest,
                agent_ref: agent_ref.clone(),
                image: entry.image,
                executable: entry.executable,
                expected_version: entry.expected_version,
                native_profile: entry.native_profile,
            };
            binding.validate()?;
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
}

#[cfg(test)]
mod tests {
    use super::{CatalogEntry, ExecutionBindingCatalog};

    fn entry(agent_ref: &str, image_byte: char) -> CatalogEntry {
        let mut entry = CatalogEntry {
            binding_id: String::new(),
            binding_digest: String::new(),
            agent_ref: agent_ref.to_owned(),
            image: format!(
                "registry.example.test/steward/agent@sha256:{}",
                image_byte.to_string().repeat(64)
            ),
            executable: "/usr/local/bin/agent".to_owned(),
            expected_version: format!(
                "agent-cli {}",
                agent_ref
                    .rsplit_once('@')
                    .map_or("", |(_, version)| version)
            ),
            native_profile: "steward-runtime-providers@1.3.0".to_owned(),
        };
        entry.binding_digest = entry.content_digest();
        entry.binding_id = entry.binding_digest.clone();
        entry
    }

    fn catalog_json(entries: &[CatalogEntry]) -> Result<String, String> {
        serde_json::to_string(entries).map_err(|error| error.to_string())
    }

    #[test]
    fn resolves_multiple_exact_logical_agents_to_distinct_immutable_bindings() -> Result<(), String>
    {
        let first = entry("agent@1.0.0", 'a');
        let second = entry("agent@2.0.0", 'b');
        let catalog = ExecutionBindingCatalog::from_json(&catalog_json(&[first, second])?)?;

        let one = catalog
            .resolve("agent@1.0.0")
            .ok_or_else(|| "first logical agent did not resolve".to_owned())?;
        let two = catalog
            .resolve("agent@2.0.0")
            .ok_or_else(|| "second logical agent did not resolve".to_owned())?;
        assert_ne!(one.image, two.image);
        assert_ne!(one.binding_digest, two.binding_digest);
        assert_eq!(catalog.agent_refs(), ["agent@1.0.0", "agent@2.0.0"]);
        Ok(())
    }

    #[test]
    fn rejects_mutable_incomplete_duplicate_or_rebound_catalog_entries() -> Result<(), String> {
        let valid = entry("agent@1.0.0", 'a');
        let mut cases = Vec::new();

        let mut mutable_image = valid.clone();
        mutable_image.image = "registry.example.test/steward/agent:latest".to_owned();
        mutable_image.binding_digest = mutable_image.content_digest();
        cases.push(vec![mutable_image]);

        let mut latest_agent = valid.clone();
        latest_agent.agent_ref = "agent@latest".to_owned();
        latest_agent.binding_digest = latest_agent.content_digest();
        cases.push(vec![latest_agent]);

        let mut malformed_digest = valid.clone();
        malformed_digest.binding_digest = format!("sha256:{}", "A".repeat(64));
        cases.push(vec![malformed_digest]);

        let mut rebound = valid.clone();
        rebound.expected_version = "agent-cli 9.9.9".to_owned();
        cases.push(vec![rebound]);

        let duplicate_agent = entry("agent@1.0.0", 'b');
        cases.push(vec![valid.clone(), duplicate_agent]);

        let mut duplicate_identity = entry("agent@2.0.0", 'b');
        duplicate_identity.binding_id = valid.binding_id.clone();
        cases.push(vec![valid, duplicate_identity]);

        for entries in cases {
            assert!(
                ExecutionBindingCatalog::from_json(&catalog_json(&entries)?).is_err(),
                "invalid deployment binding catalog must fail closed"
            );
        }
        Ok(())
    }
}
