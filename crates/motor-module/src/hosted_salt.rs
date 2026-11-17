use crate::vault::ModuleSecrets;
use cortexkit_claustrum_client::{CredentialErrorClass, ModuleSecret, ModuleSecretError, Secret};
use std::{
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
};

const NAME: &str = "hosted-salt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Unavailable,
    TooOld,
    NotAttested,
    Mismatch,
}

impl From<ModuleSecretError> for Failure {
    fn from(error: ModuleSecretError) -> Self {
        match error {
            ModuleSecretError::Other(error) if error.code() == "unknown_method" => Self::TooOld,
            ModuleSecretError::Other(error)
                if error.class == CredentialErrorClass::CredentialAbsent =>
            {
                Self::NotAttested
            }
            _ => Self::Unavailable,
        }
    }
}

pub(crate) struct HostedSalt {
    path: PathBuf,
    resolver: Mutex<Option<Arc<dyn ModuleSecrets>>>,
    value: tokio::sync::OnceCell<Secret<[u8; 32]>>,
}

impl HostedSalt {
    pub(crate) fn new(root: &std::path::Path) -> Self {
        Self {
            path: root.join(NAME),
            resolver: Mutex::new(None),
            value: tokio::sync::OnceCell::new(),
        }
    }

    pub(crate) fn set_resolver(&self, resolver: Arc<dyn ModuleSecrets>) {
        *self.resolver.lock().unwrap() = Some(resolver);
    }

    pub(crate) async fn get(&self) -> Result<[u8; 32], Failure> {
        let value = self
            .value
            .get_or_try_init(|| async {
                let resolver = self
                    .resolver
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or(Failure::Unavailable)?;
                self.load(resolver.as_ref()).await.map(Secret::new)
            })
            .await?;
        Ok(*value.expose())
    }

    async fn load(&self, resolver: &dyn ModuleSecrets) -> Result<[u8; 32], Failure> {
        match resolver.get(NAME).await {
            Ok(secret) => return self.finish(secret).await,
            Err(ModuleSecretError::Other(error))
                if error.class == CredentialErrorClass::CredentialAbsent =>
            {
                // The vault reports not_found both for an absent secret and when
                // it rejects this module's access. A successful listing proves
                // access was accepted before we try to create a value.
                let items = resolver.list().await?;
                if items.iter().any(|item| item.name == NAME) {
                    return self.finish(resolver.get(NAME).await?).await;
                }
            }
            Err(error) => return Err(error.into()),
        }

        let path = self.path.clone();
        let candidate = tokio::task::spawn_blocking(move || legacy_or_fresh(path))
            .await
            .map_err(|_| Failure::Unavailable)?
            .map_err(|_| Failure::Unavailable)?;
        match resolver
            .put(
                NAME,
                Secret::new(candidate.expose().to_vec()),
                None,
                Some(true),
            )
            .await
        {
            Ok(_) => {
                let secret = resolver.get(NAME).await?;
                if secret.value.expose().as_slice() != candidate.expose() {
                    return Err(Failure::Mismatch);
                }
                self.finish(secret).await
            }
            // A concurrent creator or a lost reply may already have installed
            // the authoritative value. Never replace it or compare our candidate.
            Err(ModuleSecretError::VersionConflict | ModuleSecretError::Protected) => {
                self.finish(resolver.get(NAME).await?).await
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn finish(&self, secret: ModuleSecret) -> Result<[u8; 32], Failure> {
        let value = secret
            .value
            .expose()
            .as_slice()
            .try_into()
            .map_err(|_| Failure::Mismatch)?;
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || match fs::remove_file(&path) {
            Ok(()) => {
                tracing::info!("removed legacy hosted-salt file; the vault is authoritative");
                fs::File::open(path.parent().expect("salt has a store root"))?.sync_all()
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        })
        .await
        .map_err(|_| Failure::Unavailable)?
        .map_err(|_| Failure::Unavailable)?;
        Ok(value)
    }
}

fn legacy_or_fresh(path: PathBuf) -> io::Result<Secret<[u8; 32]>> {
    use std::{
        io::Read,
        os::unix::fs::{OpenOptionsExt, PermissionsExt},
    };
    match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.permissions().mode() & 0o777 != 0o600
                || metadata.len() != 32
            {
                return Err(io::Error::other("unsafe or invalid legacy hosted salt"));
            }
            let mut value = [0; 32];
            file.read_exact(&mut value)?;
            Ok(Secret::new(value))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut value = [0; 32];
            getrandom::fill(&mut value).map_err(io::Error::other)?;
            Ok(Secret::new(value))
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
pub(crate) mod tests;
