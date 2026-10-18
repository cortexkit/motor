use super::*;
use crate::hosted::account::tests::{token, FakeVault};

#[tokio::test]
async fn hosted_account_token_is_acquired_for_lease_reattach_and_release() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let mut route = h.route.clone();
    route.token_source = BrokerTokenSource::CkAccount;
    let storage = stored(&scratch, id, route.clone());
    let vault = FakeVault::new((0..3).map(|_| Ok(token("hosted-test-bearer"))).collect());
    let client = Client::new(route).with_account_tokens(Some(vault.client()));
    let (channel, _) = client
        .run(id, &Harness::request(&scratch.0), &storage)
        .await
        .unwrap();
    drop(channel);
    drop(client.attach(id, &storage).await.unwrap());
    client.release(id).await.unwrap();
    assert_eq!(vault.requests.lock().unwrap().len(), 3);
    assert_eq!(h.requests.lock().unwrap().len(), 3);
    let job = serde_json::to_string(&storage.job(id)).unwrap();
    assert!(!job.contains("hosted-test-bearer"));
}

#[tokio::test]
async fn hosted_broker_receives_the_new_vault_bearer_not_a_cached_one() {
    let h = Harness::new(Behavior::Normal).await;
    let scratch = Scratch::new();
    let id = Uuid::now_v7();
    let mut route = h.route.clone();
    route.token_source = BrokerTokenSource::CkAccount;
    let storage = stored(&scratch, id, route.clone());
    let vault = FakeVault::new(vec![
        Ok(token("hosted-test-bearer")),
        Ok(token("rotated-fixture")),
    ]);
    let client = Client::new(route).with_account_tokens(Some(vault.client()));
    drop(
        client
            .run(id, &Harness::request(&scratch.0), &storage)
            .await
            .unwrap(),
    );
    assert!(matches!(
        client.attach(id, &storage).await,
        Err(Error::Auth)
    ));
    assert_eq!(vault.requests.lock().unwrap().len(), 2);
}
