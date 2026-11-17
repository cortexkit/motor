use cortexkit_claustrum_client::{
    ClaustrumCredentialResolver, CredentialPublicKey, CredentialResolver, CredentialResolverError,
    CredentialResolverFuture, CredentialSignature, ModuleSecret, ModuleSecretError,
    ModuleSecretSummary, ModuleSecretWritten, ResolvedCredential, Secret,
};
use std::{path::PathBuf, sync::Arc};

/// The module-private vault operations needed by hosted placement.
pub trait ModuleSecrets: Send + Sync {
    fn get(
        &self,
        name: &str,
    ) -> CredentialResolverFuture<'_, Result<ModuleSecret, ModuleSecretError>>;
    fn list(
        &self,
    ) -> CredentialResolverFuture<'_, Result<Vec<ModuleSecretSummary>, ModuleSecretError>>;
    fn put(
        &self,
        name: &str,
        value: Secret<Vec<u8>>,
        expected_version: Option<u64>,
        protected: Option<bool>,
    ) -> CredentialResolverFuture<'_, Result<ModuleSecretWritten, ModuleSecretError>>;
}

/// Opens an own-module SDK consumer only when a credential or placement key is needed.
pub struct VaultResolver {
    connection: PathBuf,
    resolver: tokio::sync::OnceCell<ClaustrumCredentialResolver>,
}

impl VaultResolver {
    pub fn new(connection: PathBuf) -> Self {
        Self {
            connection,
            resolver: tokio::sync::OnceCell::new(),
        }
    }

    async fn resolver(&self) -> Result<&ClaustrumCredentialResolver, CredentialResolverError> {
        self.resolver
            .get_or_try_init(|| async {
                // Serving enables SDK proof that the caller is this supervised
                // module before jobs reach this lazy consumer. An identity
                // override would conflict with that connection-bound proof.
                let consumer =
                    subc_client_rs::SubcConsumer::connect(&self.connection, Default::default())
                        .await
                        .map_err(|_| CredentialResolverError::unavailable())?;
                Ok(ClaustrumCredentialResolver::with_consumer(Arc::new(
                    consumer,
                )))
            })
            .await
    }
}

impl ModuleSecrets for VaultResolver {
    fn get(
        &self,
        name: &str,
    ) -> CredentialResolverFuture<'_, Result<ModuleSecret, ModuleSecretError>> {
        let name = name.to_owned();
        Box::pin(async move { self.resolver().await?.module_secret_get(&name).await })
    }
    fn list(
        &self,
    ) -> CredentialResolverFuture<'_, Result<Vec<ModuleSecretSummary>, ModuleSecretError>> {
        Box::pin(async move { self.resolver().await?.module_secret_list().await })
    }
    fn put(
        &self,
        name: &str,
        value: Secret<Vec<u8>>,
        expected_version: Option<u64>,
        protected: Option<bool>,
    ) -> CredentialResolverFuture<'_, Result<ModuleSecretWritten, ModuleSecretError>> {
        let name = name.to_owned();
        Box::pin(async move {
            self.resolver()
                .await?
                .module_secret_put(&name, value, expected_version, protected)
                .await
        })
    }
}

impl CredentialResolver for VaultResolver {
    fn resolve<'a>(
        &'a self,
        id: &'a str,
        version: Option<u64>,
        ui: bool,
    ) -> CredentialResolverFuture<'a, Result<ResolvedCredential, CredentialResolverError>> {
        Box::pin(async move { self.resolver().await?.resolve(id, version, ui).await })
    }
    fn get_scoped<'a>(
        &'a self,
        id: &'a str,
    ) -> CredentialResolverFuture<'a, Result<ResolvedCredential, CredentialResolverError>> {
        Box::pin(async move { self.resolver().await?.get_scoped(id).await })
    }
    fn public_key<'a>(
        &'a self,
        id: &'a str,
    ) -> CredentialResolverFuture<'a, Result<CredentialPublicKey, CredentialResolverError>> {
        Box::pin(async move { self.resolver().await?.public_key(id).await })
    }
    fn sign<'a>(
        &'a self,
        id: &'a str,
        data: &'a [u8],
    ) -> CredentialResolverFuture<'a, Result<CredentialSignature, CredentialResolverError>> {
        Box::pin(async move { self.resolver().await?.sign(id, data).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_consumer_opens_are_not_cached_for_ssh_or_hosted() {
        let root = cortexkit_test_support::ScratchDir::new("vault-consumer-open");
        let resolver = VaultResolver::new(root.join("missing-connection"));
        assert!(!resolver.resolver.initialized());
        assert!(resolver
            .public_key(crate::RUNNER_CREDENTIAL_ID)
            .await
            .is_err());
        assert!(!resolver.resolver.initialized());
        assert!(resolver.get("hosted-salt").await.is_err());
        assert!(!resolver.resolver.initialized());
        assert!(resolver
            .public_key(crate::RUNNER_CREDENTIAL_ID)
            .await
            .is_err());
        assert!(!resolver.resolver.initialized());
    }
}
