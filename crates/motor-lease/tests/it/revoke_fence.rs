use super::common::*;
use motor_lease::*;
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn vectors() -> [Value; 2] {
    [
        serde_json::from_str(include_str!(
            "../../contracts/cloud/host-revoke-fence-off.json"
        ))
        .unwrap(),
        serde_json::from_str(include_str!(
            "../../contracts/cloud/host-revoke-fence-on.json"
        ))
        .unwrap(),
    ]
}

fn exchange<'a>(vector: &'a Value, label: &str) -> &'a Value {
    vector["exchanges"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["label"] == label)
        .unwrap()
}

#[test]
fn fence_vectors_parse_every_heartbeat_and_preserve_fence_off_bytes() {
    for (on, vector) in vectors().iter().enumerate() {
        for e in vector["exchanges"].as_array().unwrap() {
            if !e["request"]["pathname"]
                .as_str()
                .unwrap()
                .ends_with("/heartbeat")
            {
                continue;
            }
            let request: Heartbeat =
                serde_json::from_str(e["request"]["body_bytes"].as_str().unwrap()).unwrap();
            let reply: HeartbeatReply =
                serde_json::from_str(e["reply"]["body_bytes"].as_str().unwrap()).unwrap();
            assert_eq!(reply.revoked.is_some(), on == 1);
            assert_eq!(
                serde_json::to_string(&reply).unwrap(),
                e["reply"]["body_bytes"]
            );
            if e["request"]["body"].get("revoked_ack").is_none() {
                assert_eq!(
                    serde_json::to_string(&request).unwrap(),
                    e["request"]["body_bytes"]
                );
            }
            if e["label"] == "ack_full_cap" {
                assert_eq!(request.revoked_ack.len(), REVOKED_LIMIT);
                assert_eq!(
                    serde_json::to_string(&request).unwrap(),
                    e["request"]["body_bytes"]
                );
            }
            if e["label"] == "at_reply_cap" && on == 1 {
                assert_eq!(reply.revoked.unwrap().len(), REVOKED_LIMIT);
            }
        }
    }
}

#[test]
fn fake_serves_vector_revocations_and_idempotent_full_cap_ack() {
    let [_, vector] = vectors();
    let cap: HeartbeatReply =
        serde_json::from_value(exchange(&vector, "at_reply_cap")["reply"]["body"].clone()).unwrap();
    let mut b = broker();
    b.knobs.revocations = Some(BTreeMap::from([(HOST.into(), cap.revoked.unwrap())]));
    let mut hb: Heartbeat =
        serde_json::from_value(exchange(&vector, "at_reply_cap")["request"]["body"].clone())
            .unwrap();
    let now = vector["clock_ms"].as_u64().unwrap();
    let reply = host_post(&mut b, "heartbeat", &hb, 1, now);
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        exchange(&vector, "at_reply_cap")["reply"]["body"]
    );
    hb.revoked_ack = serde_json::from_value(
        exchange(&vector, "ack_full_cap")["request"]["body"]["revoked_ack"].clone(),
    )
    .unwrap();
    for counter in [2, 3] {
        let reply = host_post(&mut b, "heartbeat", &hb, counter, now);
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body["revoked"], json!([]));
    }
    hb.revoked_ack.push(hb.revoked_ack[0].clone());
    assert_eq!(host_post(&mut b, "heartbeat", &hb, 4, now).status, 400);
    hb.revoked_ack.truncate(2);
    hb.revoked_ack[1] = hb.revoked_ack[0].clone();
    assert_eq!(host_post(&mut b, "heartbeat", &hb, 5, now).status, 400);
    hb.revoked_ack = vec!["not-a-job".into()];
    assert_eq!(host_post(&mut b, "heartbeat", &hb, 6, now).status, 400);
    hb.revoked_ack.clear();
    b.knobs.revocations = None;
    assert!(host_post(&mut b, "heartbeat", &hb, 7, now)
        .body
        .get("revoked")
        .is_none());
}

#[test]
fn malformed_revocations_are_errors_not_empty_lists() {
    let job = "019e4567-89ab-7cde-8012-3456789abcde";
    let good = json!({"job_id":job,"until_ms":"1792195500000"});
    for list in [
        Value::Null,
        json!({}),
        json!([{"job_id":job,"until_ms":1792195500000_u64}]),
        json!([{"job_id":job,"until_ms":"01792195500000"}]),
        json!([{"job_id":job,"until_ms":"1e3"}]),
        json!([{"job_id":job,"until_ms":"9007199254740992"}]),
        json!([{"job_id":"bad","until_ms":"1792195500000"}]),
        json!([{"job_id":job,"until_ms":"1792195500000","extra":true}]),
        json!([good.clone(), good.clone()]),
        json!(vec![good.clone(); 257]),
    ] {
        assert!(serde_json::from_value::<HeartbeatReply>(
            json!({"ignored":[],"closed_subjects":[],"revoked":list})
        )
        .is_err());
    }
    let duplicate = format!(
        r#"{{"ignored":[],"closed_subjects":[],"revoked":[{{"job_id":"{job}","job_id":"{job}","until_ms":"1"}}]}}"#
    );
    assert!(serde_json::from_str::<HeartbeatReply>(&duplicate).is_err());
}
