use super::*;
use crate::vault::ModuleSecrets;
use cortexkit_claustrum_client::{
    CredentialResolverError, CredentialResolverFuture, ModuleSecretState, ModuleSecretSummary,
    ModuleSecretWritten,
};
use std::{
    collections::VecDeque,
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicUsize, Ordering},
};

pub(crate) struct FakeVault {
    value: Mutex<Option<Vec<u8>>>,
    gets: AtomicUsize,
    puts: AtomicUsize,
    lists: AtomicUsize,
    get_errors: Mutex<VecDeque<ModuleSecretError>>,
    list_error: Mutex<Option<ModuleSecretError>>,
    put_error: Mutex<Option<ModuleSecretError>>,
    competing_value: Mutex<Option<Vec<u8>>>,
    mismatch: Mutex<bool>,
}

impl FakeVault {
    pub(crate) fn assert_untouched(&self) {
        assert_eq!(self.gets.load(Ordering::SeqCst), 0);
        assert_eq!(self.puts.load(Ordering::SeqCst), 0);
        assert_eq!(self.lists.load(Ordering::SeqCst), 0);
    }
    pub(crate) fn new(value: Option<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            value: Mutex::new(value),
            gets: AtomicUsize::new(0),
            puts: AtomicUsize::new(0),
            lists: AtomicUsize::new(0),
            get_errors: Mutex::default(),
            list_error: Mutex::default(),
            put_error: Mutex::default(),
            competing_value: Mutex::default(),
            mismatch: Mutex::new(false),
        })
    }
}

fn absent() -> ModuleSecretError {
    CredentialResolverError::new(
        CredentialErrorClass::CredentialAbsent,
        "credential service rejected the request: not_found",
    )
    .into()
}

impl ModuleSecrets for FakeVault {
    fn get(
        &self,
        name: &str,
    ) -> CredentialResolverFuture<'_, Result<ModuleSecret, ModuleSecretError>> {
        assert_eq!(name, NAME);
        self.gets.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(error) = self.get_errors.lock().unwrap().pop_front() {
                return Err(error);
            }
            Ok(ModuleSecret {
                name: NAME.into(),
                version: 1,
                value: Secret::new(self.value.lock().unwrap().clone().ok_or_else(absent)?),
                protected: true,
            })
        })
    }
    fn list(
        &self,
    ) -> CredentialResolverFuture<'_, Result<Vec<ModuleSecretSummary>, ModuleSecretError>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(error) = self.list_error.lock().unwrap().take() {
                return Err(error);
            }
            Ok(self
                .value
                .lock()
                .unwrap()
                .as_ref()
                .map(|_| ModuleSecretSummary {
                    name: NAME.into(),
                    version: 1,
                    protected: true,
                    state: ModuleSecretState::Active,
                    updated_at_ms: 1,
                })
                .into_iter()
                .collect())
        })
    }
    fn put(
        &self,
        name: &str,
        value: Secret<Vec<u8>>,
        expected_version: Option<u64>,
        protected: Option<bool>,
    ) -> CredentialResolverFuture<'_, Result<ModuleSecretWritten, ModuleSecretError>> {
        assert_eq!(name, NAME);
        assert_eq!(expected_version, None);
        assert_eq!(protected, Some(true));
        assert_eq!(value.expose().len(), 32);
        self.puts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(competing) = self.competing_value.lock().unwrap().take() {
                *self.value.lock().unwrap() = Some(competing);
            }
            if let Some(error) = self.put_error.lock().unwrap().take() {
                return Err(error);
            }
            assert!(
                self.value.lock().unwrap().is_none(),
                "must never overwrite a vault value"
            );
            *self.value.lock().unwrap() = Some(if *self.mismatch.lock().unwrap() {
                vec![99; 32]
            } else {
                value.expose().clone()
            });
            Ok(ModuleSecretWritten {
                name: NAME.into(),
                version: 1,
                created: true,
            })
        })
    }
}

// Existing hosted protocol fixtures opt into an in-memory vault explicitly.
pub(crate) fn storage(root: &std::path::Path) -> io::Result<crate::storage::Storage> {
    let storage = crate::storage::Storage::open(root)?;
    storage
        .hosted_salt
        .set_resolver(FakeVault::new(Some(vec![7; 32])));
    Ok(storage)
}

pub(crate) fn motor(
    config: crate::Config,
    root: &std::path::Path,
    connector: Arc<dyn crate::Connector>,
) -> io::Result<crate::Motor> {
    crate::Motor::with_connector(config, root, connector)
        .map(|motor| motor.with_module_secrets(FakeVault::new(Some(vec![7; 32]))))
}

pub(crate) fn direct_motor(
    config: crate::Config,
    root: &std::path::Path,
    connector: Arc<dyn crate::Connector>,
) -> io::Result<crate::Motor> {
    crate::Motor::with_test_fixture_direct_route(config, root, connector)
        .map(|motor| motor.with_module_secrets(FakeVault::new(Some(vec![7; 32]))))
}

fn legacy(root: &std::path::Path, value: &[u8]) -> PathBuf {
    let path = root.join(NAME);
    fs::write(&path, value).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    path
}

#[tokio::test]
async fn migration_stores_protected_verifies_and_deletes_file() {
    let root = cortexkit_test_support::ScratchDir::new("salt-migration");
    let path = legacy(root.path(), &[41; 32]);
    let vault = FakeVault::new(None);
    let salt = HostedSalt::new(root.path());
    salt.set_resolver(vault.clone());
    assert_eq!(salt.get().await.unwrap(), [41; 32]);
    assert_eq!(
        vault.value.lock().unwrap().as_deref(),
        Some([41; 32].as_slice())
    );
    assert_eq!(vault.puts.load(Ordering::SeqCst), 1);
    assert_eq!(vault.gets.load(Ordering::SeqCst), 2);
    assert!(!path.exists());
    assert_eq!(salt.get().await.unwrap(), [41; 32]);
    assert_eq!(vault.gets.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn vault_wins_even_over_an_invalid_legacy_file() {
    for valid in [true, false] {
        let root = cortexkit_test_support::ScratchDir::new("salt-vault-wins");
        let path = legacy(
            root.path(),
            if valid {
                &[41; 32]
            } else {
                b"invalid old salt"
            },
        );
        if !valid {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let salt = HostedSalt::new(root.path());
        let vault = FakeVault::new(Some(vec![27; 32]));
        salt.set_resolver(vault.clone());
        assert_eq!(salt.get().await.unwrap(), [27; 32]);
        assert!(!path.exists());
        assert_eq!(vault.puts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn fresh_install_stores_protected_and_caches_only_success() {
    let root = cortexkit_test_support::ScratchDir::new("salt-fresh");
    let salt = HostedSalt::new(root.path());
    let vault = FakeVault::new(None);
    salt.set_resolver(vault.clone());
    let value = salt.get().await.unwrap();
    assert_ne!(value, [0; 32]);
    assert_eq!(
        vault.value.lock().unwrap().as_deref(),
        Some(value.as_slice())
    );
    assert_eq!(salt.get().await.unwrap(), value);
    assert_eq!(vault.puts.load(Ordering::SeqCst), 1);
    assert!(!root.join(NAME).exists());
    let restarted = HostedSalt::new(root.path());
    restarted.set_resolver(vault.clone());
    assert_eq!(restarted.get().await.unwrap(), value);
    assert_eq!(vault.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_reply_or_concurrent_creation_uses_vault_without_overwriting() {
    for error in [
        ModuleSecretError::VersionConflict,
        ModuleSecretError::Protected,
    ] {
        let root = cortexkit_test_support::ScratchDir::new("salt-conflict");
        let path = legacy(root.path(), &[41; 32]);
        let vault = FakeVault::new(None);
        *vault.put_error.lock().unwrap() = Some(error);
        *vault.competing_value.lock().unwrap() = Some(vec![57; 32]);
        let salt = HostedSalt::new(root.path());
        salt.set_resolver(vault.clone());
        assert_eq!(salt.get().await.unwrap(), [57; 32]);
        assert_eq!(vault.puts.load(Ordering::SeqCst), 1);
        assert!(!path.exists());
    }
}

#[tokio::test]
async fn compare_mismatch_keeps_file_and_does_not_cache_failure() {
    let root = cortexkit_test_support::ScratchDir::new("salt-mismatch");
    let path = legacy(root.path(), &[41; 32]);
    let salt = HostedSalt::new(root.path());
    let vault = FakeVault::new(None);
    *vault.mismatch.lock().unwrap() = true;
    salt.set_resolver(vault.clone());
    assert_eq!(salt.get().await, Err(Failure::Mismatch));
    assert_eq!(fs::read(&path).unwrap(), vec![41; 32]);
    // The next attempt reads the now-authoritative vault, rather than retrying a put.
    assert_eq!(salt.get().await.unwrap(), [99; 32]);
    assert!(!path.exists());
    assert_eq!(vault.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unattested_list_or_put_never_falls_back_to_legacy_file() {
    for on_put in [false, true] {
        let root = cortexkit_test_support::ScratchDir::new("salt-attestation");
        let path = legacy(root.path(), &[41; 32]);
        let vault = FakeVault::new(None);
        if on_put {
            *vault.put_error.lock().unwrap() = Some(absent());
        } else {
            *vault.list_error.lock().unwrap() = Some(absent());
        }
        let salt = HostedSalt::new(root.path());
        salt.set_resolver(vault.clone());
        assert_eq!(salt.get().await, Err(Failure::NotAttested));
        assert!(path.exists());
        assert!(vault.value.lock().unwrap().is_none());
        assert_eq!(vault.puts.load(Ordering::SeqCst), usize::from(on_put));
    }
}

#[tokio::test]
async fn unsafe_legacy_files_are_not_migrated() {
    for kind in ["mode", "length", "symlink", "directory"] {
        let root = cortexkit_test_support::ScratchDir::new("salt-unsafe");
        let path = legacy(root.path(), &[41; 32]);
        match kind {
            "mode" => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            "length" => fs::write(&path, b"short").unwrap(),
            "symlink" => {
                fs::rename(&path, root.join("target")).unwrap();
                std::os::unix::fs::symlink(root.join("target"), &path).unwrap();
            }
            _ => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
            }
        }
        let vault = FakeVault::new(None);
        let salt = HostedSalt::new(root.path());
        salt.set_resolver(vault.clone());
        assert_eq!(salt.get().await, Err(Failure::Unavailable), "{kind}");
        assert_eq!(vault.puts.load(Ordering::SeqCst), 0);
        assert!(fs::symlink_metadata(path).is_ok());
    }
}

#[tokio::test]
async fn concurrent_first_uses_resolve_once() {
    let root = cortexkit_test_support::ScratchDir::new("salt-concurrent");
    let vault = FakeVault::new(None);
    let salt = Arc::new(HostedSalt::new(root.path()));
    salt.set_resolver(vault.clone());
    let mut tasks = Vec::new();
    for _ in 0..12 {
        let salt = salt.clone();
        tasks.push(tokio::spawn(async move { salt.get().await.unwrap() }));
    }
    let value = tasks.remove(0).await.unwrap();
    for task in tasks {
        assert_eq!(task.await.unwrap(), value);
    }
    assert_eq!(vault.gets.load(Ordering::SeqCst), 2);
    assert_eq!(vault.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn vault_failure_refuses_before_booking_and_next_submission_retries() {
    use crate::hosted::{
        boat_tests::{committed_repo, records},
        tests::harness::{Behavior, Harness, NoSsh},
    };
    use motor_protocol::{Outcome, Ran, RefusalReason};
    let h = Harness::new(Behavior::CompleteRun).await;
    let (scratch, repo) = committed_repo();
    let vault = FakeVault::new(Some(vec![31; 32]));
    vault
        .get_errors
        .lock()
        .unwrap()
        .push_back(CredentialResolverError::unavailable().into());
    let ssh = Arc::new(NoSsh(AtomicUsize::new(0)));
    let motor = crate::Motor::with_connector(h.config(), &scratch.0.join("state"), ssh.clone())
        .unwrap()
        .with_module_secrets(vault.clone());
    vault.assert_untouched();
    let (_, terminal) = records(motor.run(Harness::request(&repo)).unwrap()).await;
    assert_eq!(
        terminal.outcome,
        Outcome::RefusedBeforeStart {
            reason: RefusalReason::Unreachable
        }
    );
    assert_eq!(terminal.ran, Some(Ran::None));
    assert!(terminal
        .refusal_detail
        .unwrap()
        .contains("hosted_vault_unavailable: the vault"));
    assert!(h.requests.lock().unwrap().is_empty());
    assert_eq!(ssh.0.load(Ordering::SeqCst), 0);
    assert_eq!(vault.puts.load(Ordering::SeqCst), 0);
    let (_, terminal) = records(motor.run(Harness::request(&repo)).unwrap()).await;
    assert_eq!(terminal.outcome, Outcome::Exit { code: 23 });
    assert_eq!(vault.gets.load(Ordering::SeqCst), 2);
    assert_eq!(h.lease_bodies.lock().unwrap().len(), 1);
    motor.close().await;
}

#[tokio::test]
async fn vault_refusal_details_distinguish_old_attestation_limit_and_mismatch() {
    use crate::hosted::{
        boat_tests::{committed_repo, records},
        tests::harness::{Behavior, Harness, NoSsh},
    };
    use motor_protocol::{Outcome, Ran, RefusalReason};
    for (mode, code, detail, reason) in [
        (
            "old",
            "hosted_vault_too_old",
            "the vault is too old for module secrets; update claustrum",
            RefusalReason::RunnerAuthenticationFailed,
        ),
        (
            "attestation",
            "hosted_vault_not_attested",
            "the vault didn't accept ck-motor as its own module",
            RefusalReason::RunnerAuthenticationFailed,
        ),
        (
            "limit",
            "hosted_vault_unavailable",
            "the vault is unavailable",
            RefusalReason::Unreachable,
        ),
        (
            "mismatch",
            "hosted_vault_mismatch",
            "the placement key read back from the vault did not match",
            RefusalReason::Unreachable,
        ),
    ] {
        let h = Harness::new(Behavior::CompleteRun).await;
        let (scratch, repo) = committed_repo();
        let root = scratch.0.join("state");
        let vault = FakeVault::new(None);
        match mode {
            "old" => vault.get_errors.lock().unwrap().push_back(
                cortexkit_claustrum_client::decode_wire_error(
                    br#"{"code":"unknown_method","message":"unknown method"}"#,
                )
                .into(),
            ),
            "attestation" => *vault.list_error.lock().unwrap() = Some(absent()),
            "limit" => *vault.put_error.lock().unwrap() = Some(ModuleSecretError::Limit),
            _ => *vault.mismatch.lock().unwrap() = true,
        }
        let motor =
            crate::Motor::with_connector(h.config(), &root, Arc::new(NoSsh(AtomicUsize::new(0))))
                .unwrap()
                .with_module_secrets(vault.clone());
        let path = legacy(&root, &[41; 32]);
        let stream = motor.run(Harness::request(&repo)).unwrap();
        let id = stream.job_id;
        let (_, terminal) = records(stream).await;
        assert_eq!(terminal.outcome, Outcome::RefusedBeforeStart { reason });
        assert_eq!(terminal.ran, Some(Ran::None));
        assert!(
            terminal.refusal_detail.as_deref().unwrap().contains(detail),
            "{terminal:?}"
        );
        assert_eq!(
            motor.hosted_state(id).unwrap().hosted_code.as_deref(),
            Some(code)
        );
        assert!(h.requests.lock().unwrap().is_empty());
        assert_eq!(fs::read(path).unwrap(), vec![41; 32]);
        assert_eq!(
            vault.puts.load(Ordering::SeqCst),
            usize::from(mode == "limit" || mode == "mismatch")
        );
        motor.close().await;
    }
}
