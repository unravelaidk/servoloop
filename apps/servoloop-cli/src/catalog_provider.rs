//! Persisted catalog connection metadata. Contains references, never secrets.
use serde::{Deserialize, Serialize};
use servoloop_providers::{CatalogProvider, KeyPolicy, ProviderSpec, Secret};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogConnection {
    pub id: String,
    pub name: String,
    pub npm: String,
    pub endpoint: String,
    pub credential_env: Vec<String>,
}

impl From<&CatalogProvider> for CatalogConnection {
    fn from(provider: &CatalogProvider) -> Self {
        Self {
            id: provider.id.clone(),
            name: provider.name.clone(),
            npm: provider.npm.clone(),
            endpoint: provider.base_url.clone().unwrap_or_default(),
            credential_env: provider.env.clone(),
        }
    }
}

impl CatalogConnection {
    /// Compatibility is a protocol/adapter property, not a provider-ID allowlist.
    /// Unrecognized adapters remain visible in the picker but cannot execute.
    pub fn supported(&self) -> bool {
        matches!(
            self.npm.as_str(),
            "@ai-sdk/openai-compatible" | "@ai-sdk/openai" | "@openrouter/ai-sdk-provider"
        )
    }

    pub fn spec(&self, endpoint_override: Option<String>) -> Result<ProviderSpec, String> {
        if self.id.is_empty()
            || self.id.len() > 128
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            return Err("Catalog provider identifier is invalid.".into());
        }
        if !self.supported() {
            return Err(format!("Adapter `{}` is not supported. Only OpenAI-compatible Chat Completions is implemented.", self.npm));
        }
        let endpoint = endpoint_override.unwrap_or_else(|| self.endpoint.clone());
        let url = url::Url::parse(&endpoint)
            .map_err(|_| "Enter a valid HTTP(S) endpoint for this provider.")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Provider endpoint must be HTTP(S), without embedded credentials, query parameters, or fragments.".into());
        }
        // Do not read arbitrary environment variables named by remote data.
        // Multi-variable authentication schemes need their own adapter.
        if self
            .credential_env
            .iter()
            .any(|name| !credential_reference(name))
        {
            return Err("Catalog uses an unsupported credential reference. No environment values were read.".into());
        }
        let mut key = None;
        for name in &self.credential_env {
            if let Ok(value) = std::env::var(name) {
                if !value.is_empty() {
                    crate::output::register_secret(&value);
                    if key.is_none() {
                        key = Some(Secret::new(value));
                    }
                }
            }
        }
        let mut spec = ProviderSpec::custom(self.id.clone(), self.name.clone(), endpoint, key);
        spec.key_policy = if self.credential_env.is_empty() {
            KeyPolicy::Keyless
        } else {
            KeyPolicy::Required
        };
        Ok(spec)
    }
}

fn credential_reference(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && (name.ends_with("_API_KEY")
            || name.ends_with("_TOKEN")
            || name.ends_with("_SECRET")
            || name == "API_KEY")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_catalog_id_builds_owned_spec_without_static_registration() {
        let profile = CatalogConnection {
            id: "new-lab-provider".into(),
            name: "New Lab".into(),
            npm: "@ai-sdk/openai-compatible".into(),
            endpoint: "http://127.0.0.1:1234/v1".into(),
            credential_env: vec![],
        };
        let spec = profile.spec(None).unwrap();
        assert_eq!(spec.id, "new-lab-provider");
        assert_eq!(spec.resolve_endpoint(), profile.endpoint);
        assert!(spec.validate().is_ok());
        let mut unsupported = profile.clone();
        unsupported.npm = "@ai-sdk/anthropic".into();
        assert!(unsupported.spec(None).is_err());
        unsupported.npm = profile.npm.clone();
        unsupported.credential_env = vec!["HOME".into()];
        assert!(unsupported.spec(None).is_err());
    }
}
