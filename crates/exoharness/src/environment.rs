use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    CreateSandboxRequest, DurableFileSystem, EgressCredentialBinding, EgressPolicy,
    FileSystemMount, SandboxNetworkPolicy, SandboxProvider, SandboxResourceShape,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentDefinition {
    pub name: String,
    #[serde(default)]
    pub provider: SandboxProvider,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<SandboxResourceShape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_workdir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_system_mounts: Option<Vec<FileSystemMount>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable_file_systems: Option<Vec<DurableFileSystem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub networking: Option<SandboxNetworkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tcp_ports: Option<Vec<u16>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<EgressCredentialBinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_networking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tcp_ports: Vec<u16>,
}

impl EnvironmentDefinition {
    pub fn policy(&self) -> Option<EgressPolicy> {
        self.networking.clone().map(|networking| EgressPolicy {
            networking,
            allowed_tcp_ports: self.allowed_tcp_ports.clone(),
            credentials: self.credentials.clone(),
        })
    }

    pub fn validate_name(name: &str) -> Result<()> {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "environment name must contain 1–128 letters, digits, dashes, or underscores"
        );
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        Self::validate_name(&self.name)?;
        ensure!(
            self.networking.is_some()
                || (self.allowed_tcp_ports.is_none() && self.credentials.is_empty()),
            "environment networking is required when setting allowed_tcp_ports or credentials"
        );
        ensure!(
            self.tcp_ports.iter().all(|port| *port > 0),
            "published TCP ports must be nonzero"
        );
        ensure!(
            !self.image.trim().is_empty(),
            "environment image must not be empty"
        );
        if let Some(workdir) = &self.default_workdir {
            ensure!(
                workdir.starts_with('/'),
                "environment default_workdir must be absolute"
            );
        }
        if let Some(mounts) = &self.file_system_mounts {
            for (index, mount) in mounts.iter().enumerate() {
                ensure!(
                    std::path::Path::new(&mount.host_path).is_absolute(),
                    "environment mount host paths must be absolute paths on the runtime host"
                );
                ensure!(
                    mount.mount_path.starts_with('/'),
                    "environment mount paths must be absolute"
                );
                ensure!(
                    !mounts[..index]
                        .iter()
                        .any(|other| other.mount_path == mount.mount_path),
                    "duplicate environment mount: {}",
                    mount.mount_path
                );
            }
        }
        Ok(())
    }
}

impl From<CreateSandboxRequest> for EnvironmentDefinition {
    fn from(request: CreateSandboxRequest) -> Self {
        let (networking, allowed_tcp_ports, credentials) = match request.policy {
            Some(policy) => (
                Some(policy.networking),
                policy.allowed_tcp_ports,
                policy.credentials,
            ),
            None => (None, None, Vec::new()),
        };
        Self {
            name: request.name.unwrap_or_default(),
            provider: request.provider,
            image: request.image,
            resources: request.resources,
            default_workdir: request.default_workdir,
            file_system_mounts: request.file_system_mounts,
            durable_file_systems: request.durable_file_systems,
            networking,
            allowed_tcp_ports,
            credentials,
            enable_networking: request.enable_networking,
            idle_seconds: request.idle_seconds,
            tcp_ports: request.tcp_ports,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxProvider;

    #[test]
    fn omitted_environment_provider_defaults_to_smolvm() -> Result<()> {
        let environment: EnvironmentDefinition = serde_json::from_str(
            r#"{"name":"coding","image":"ghcr.io/exoharness/codex-sandbox:latest"}"#,
        )?;
        assert_eq!(environment.provider, SandboxProvider::Smolvm);
        environment.validate()?;
        Ok(())
    }

    #[test]
    fn published_tcp_ports_must_be_nonzero() -> Result<()> {
        let mut environment: EnvironmentDefinition =
            serde_json::from_str(r#"{"name":"dev","image":"dev","tcp_ports":[13000]}"#)?;
        environment.validate()?;
        environment.tcp_ports.clear();
        environment.validate()?;
        environment.tcp_ports.push(0);
        assert!(environment.validate().is_err());
        Ok(())
    }

    #[test]
    fn flat_environment_network_settings_reach_the_sandbox_policy() -> Result<()> {
        let environment: EnvironmentDefinition = serde_json::from_str(
            r#"{"name":"dev","image":"dev","networking":{"type":"limited","allowed_hosts":["api.openai.com"]},"allowed_tcp_ports":[443]}"#,
        )?;
        environment.validate()?;
        let policy = environment.policy().expect("explicit policy");
        assert!(matches!(
            policy.networking,
            SandboxNetworkPolicy::Limited { .. }
        ));
        assert!(policy.allows_tcp_port(443));
        assert!(!policy.allows_tcp_port(80));
        assert_eq!(
            serde_json::from_str::<EnvironmentDefinition>(&serde_json::to_string(&environment)?)?,
            environment
        );
        for source in [
            r#"{"name":"dev","config":{"image":"dev"}}"#,
            r#"{"name":"dev","image":"dev","policy":{"networking":{"type":"unrestricted"}}}"#,
        ] {
            assert!(serde_json::from_str::<EnvironmentDefinition>(source).is_err());
        }
        Ok(())
    }
}
