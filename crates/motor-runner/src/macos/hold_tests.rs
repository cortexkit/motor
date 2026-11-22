use super::*;
use crate::test_helpers::CanonicalScratchDir;

fn millis(value: &str) -> u64 {
    (OffsetDateTime::parse(value, &Rfc3339)
        .unwrap()
        .unix_timestamp_nanos()
        / 1_000_000) as u64
}

fn fixture(window: Option<&str>) -> (CanonicalScratchDir, State) {
    let root = CanonicalScratchDir::new("macos-hold");
    let paths = (root.join("hold"), root.join("grant"));
    std::fs::create_dir(&paths.0).unwrap();
    std::fs::create_dir(&paths.1).unwrap();
    let state = State::new(
        window.map(|s| Window::try_from(s.to_owned()).unwrap()),
        Some(paths),
    );
    (root, state)
}

fn write(state: &State, expires_at: &str) {
    write_for(state, "nightly-check", expires_at);
}

fn write_for(state: &State, owner: &str, expires_at: &str) {
    std::fs::write(
        state.hold_dir.join("hold.json"),
        serde_json::to_vec(&Hold {
            owner: owner.into(),
            expires_at: expires_at.into(),
        })
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn outside_window_expired_and_past_end_holds_are_ignored() {
    let (_root, mut state) = fixture(Some("03:00-05:00"));
    write(&state, "2026-01-01T04:00:00Z");
    assert!(!state.refresh(millis("2026-01-01T02:59:59Z"), true));
    assert!(state.refresh(millis("2026-01-01T03:00:00Z"), true));
    assert!(!state.refresh(millis("2026-01-01T04:00:00Z"), true));
    write(&state, "2026-01-01T05:00:00.000000001Z");
    assert!(!state.refresh(millis("2026-01-01T03:00:00Z"), true));
    write(&state, "2026-01-01T05:00:00+00:00");
    assert!(state.refresh(millis("2026-01-01T04:59:59Z"), true));
    assert!(!state.refresh(millis("2026-01-01T05:00:00Z"), true));
    assert!(!state.grant_dir.join("granted.json").exists());
    state.window = None;
    assert!(!state.refresh(millis("2026-01-01T04:00:00Z"), true));
}

#[test]
fn overnight_window_uses_the_correct_occurrence_end() {
    let (_root, mut state) = fixture(Some("23:00-01:00"));
    write(&state, "2026-01-02T01:00:00Z");
    for now in ["2026-01-01T23:00:00Z", "2026-01-02T00:30:00Z"] {
        assert!(state.refresh(millis(now), true));
    }
    assert!(!state.refresh(millis("2026-01-02T01:00:00Z"), true));
    assert!(!state.refresh(millis("2026-01-02T22:59:59Z"), true));
}

#[test]
fn always_honours_holds_at_any_utc_time() {
    let (_root, mut state) = fixture(Some("always"));
    for (now, expiry) in [
        ("2026-01-01T23:59:00Z", "2026-01-02T00:30:00Z"),
        ("2026-01-02T00:01:00Z", "2026-01-02T03:01:00Z"),
        ("2026-01-02T04:00:00Z", "2026-01-02T05:30:00Z"),
        ("2026-01-02T13:00:00Z", "2026-01-02T14:30:00Z"),
    ] {
        write(&state, expiry);
        let now_ms = millis(now);
        assert!(state.refresh(now_ms, true), "{now} to {expiry}");
        assert!(state.status(now_ms).unwrap().ends_with("(granted: true)"));
    }
}

#[test]
fn always_rejects_expired_and_more_than_three_hours_ahead() {
    let (_root, mut state) = fixture(Some("always"));
    let now = millis("2026-01-01T13:00:00Z");
    for expiry in [
        "2026-01-01T12:59:59Z",
        "2026-01-01T13:00:00Z",
        "2026-01-01T16:00:00.000000001Z",
    ] {
        write(&state, expiry);
        assert!(!state.refresh(now, true), "{expiry}");
    }
    assert!(!state.grant_dir.join("granted.json").exists());
}

#[test]
fn malformed_nonregular_oversized_and_symlink_holds_are_ignored_once() {
    let (_root, mut state) = fixture(Some("03:00-05:00"));
    let now = millis("2026-01-01T03:30:00Z");
    let path = state.hold_dir.join("hold.json");
    let malformed = [
        "not json",
        r#"{}"#,
        r#"{"owner":"ok","expires_at":"2026-01-01T04:00:00Z","extra":1}"#,
        r#"{"owner":"ok","owner":"other","expires_at":"2026-01-01T04:00:00Z"}"#,
        r#"{"owner":"Bad_owner","expires_at":"2026-01-01T04:00:00Z"}"#,
        r#"{"owner":"","expires_at":"2026-01-01T04:00:00Z"}"#,
        r#"{"owner":"ok","expires_at":"2026-02-30T04:00:00Z"}"#,
        r#"{"owner":"ok","expires_at":"2026-01-01T04:00:00+01:00"}"#,
        r#"{"owner":"ok","expires_at":"2026-01-01T04:00:00-00:00"}"#,
    ];
    for _ in 0..2 {
        for text in malformed {
            std::fs::write(&path, text).unwrap();
            assert!(!state.refresh(now, true), "{text}");
        }
    }
    assert_eq!(state.bad.len(), malformed.len());
    std::fs::write(&path, " ".repeat(4097)).unwrap();
    assert!(!state.refresh(now, true));
    let count = state.bad.len();
    std::fs::write(&path, " ".repeat(4097)).unwrap();
    assert!(!state.refresh(now, true));
    assert_eq!(
        state.bad.len(),
        count,
        "metadata changes must not repeat the warning"
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(!state.refresh(now, true));
    std::fs::remove_dir(&path).unwrap();
    let target = state.hold_dir.join("real.json");
    write(&state, "2026-01-01T04:00:00Z");
    std::fs::rename(&path, &target).unwrap();
    std::os::unix::fs::symlink(target, &path).unwrap();
    assert!(!state.refresh(now, true));
    assert!(!state.grant_dir.join("granted.json").exists());
}

#[test]
fn grant_copies_fields_only_when_idle_and_deletion_removes_it() {
    let (_root, mut state) = fixture(Some("03:00-05:00"));
    let now = millis("2026-01-01T03:30:00Z");
    write(&state, "2026-01-01T04:00:00.123+00:00");
    assert!(state.refresh(now, false));
    assert!(!state.grant_dir.join("granted.json").exists());
    assert!(state.status(now).unwrap().ends_with("(granted: false)"));
    assert!(state.refresh(now, true));
    assert_eq!(
        std::fs::read(state.hold_dir.join("hold.json")).unwrap(),
        std::fs::read(state.grant_dir.join("granted.json")).unwrap()
    );
    assert!(state.status(now).unwrap().ends_with("(granted: true)"));
    assert_eq!(
        std::fs::metadata(state.grant_dir.join("granted.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o644,
    );
    assert_eq!(std::fs::read_dir(&state.grant_dir).unwrap().count(), 1);
    std::fs::remove_file(state.hold_dir.join("hold.json")).unwrap();
    assert!(!state.refresh(now, true));
    assert!(!state.grant_dir.join("granted.json").exists());
}

#[test]
fn replacement_after_refusal_leaves_a_full_fleet_turn_even_without_a_retry() {
    for observe_gap in [false, true] {
        let (_root, mut state) = fixture(Some("always"));
        let now = millis("2026-01-01T03:30:00Z");
        let grant = state.grant_dir.join("granted.json");
        write(&state, "2026-01-01T04:00:00Z");
        assert!(state.refresh(now, true));
        assert!(state.admission_refusal(now).is_some());
        assert!(grant.exists());

        let turn_start = now + 9_000;
        if observe_gap {
            std::fs::remove_file(state.hold_dir.join("hold.json")).unwrap();
            assert!(!state.refresh(turn_start, true));
        }
        write_for(&state, "next-check", "2026-01-01T04:00:00Z");
        let replacement_seen = turn_start + if observe_gap { 1_000 } else { 0 };
        assert!(!state.refresh(replacement_seen, true));
        assert!(state.status(replacement_seen).is_none());
        assert!(!grant.exists());

        assert!(!state.refresh(turn_start + 59_999, true));
        assert!(state.refresh(turn_start + 60_000, true));
        assert_eq!(
            state.status(turn_start + 60_000).as_deref(),
            Some("held for next-check until 2026-01-01T04:00:00Z (granted: true)")
        );
        assert!(grant.exists());
    }
}

#[test]
fn replacement_without_refusal_does_not_start_a_fleet_turn() {
    let (_root, mut state) = fixture(Some("always"));
    let now = millis("2026-01-01T03:30:00Z");
    write(&state, "2026-01-01T04:00:00Z");
    assert!(state.refresh(now, true));
    assert!(state.status(now).is_some());

    write_for(&state, "next-check", "2026-01-01T04:00:00Z");
    assert!(state.refresh(now, true));
    assert_eq!(
        state.status(now).as_deref(),
        Some("held for next-check until 2026-01-01T04:00:00Z (granted: true)")
    );
}

#[test]
fn expiry_rewrite_preserves_the_refusal_without_starting_a_turn() {
    let (_root, mut state) = fixture(Some("always"));
    let now = millis("2026-01-01T03:30:00Z");
    write(&state, "2026-01-01T04:00:00Z");
    assert!(state.refresh(now, true));
    assert!(state.admission_refusal(now).is_some());

    write(&state, "2026-01-01T04:30:00Z");
    assert!(state.refresh(now, true));
    assert_eq!(
        state.status(now).as_deref(),
        Some("held for nightly-check until 2026-01-01T04:30:00Z (granted: true)")
    );

    write_for(&state, "next-check", "2026-01-01T04:30:00Z");
    assert!(!state.refresh(now, true));
}

#[test]
fn expired_hold_leaves_a_fleet_turn_after_refusal() {
    let (_root, mut state) = fixture(Some("always"));
    let expires = millis("2026-01-01T04:00:00Z");
    write(&state, "2026-01-01T04:00:00Z");
    assert!(state.refresh(expires - 1, true));
    assert!(state.admission_refusal(expires - 1).is_some());
    assert!(!state.refresh(expires, true));

    write_for(&state, "next-check", "2026-01-01T04:30:00Z");
    assert!(!state.refresh(expires + 1, true));
    assert!(state.refresh(expires + 60_000, true));
}

#[test]
fn window_rejects_equal_endpoints_and_invalid_clock_fields_at_load() {
    for value in [
        "01:00-01:00",
        "3:00-05:00",
        "24:00-05:00",
        "03:60-05:00",
        "03:00-05:00\n",
        "ä:00-05:00",
    ] {
        let text = format!("window = {value:?}");
        #[derive(Deserialize)]
        struct Config {
            #[serde(rename = "window")]
            _window: Window,
        }
        let error = toml::from_str::<Config>(&text).err().unwrap();
        assert!(error.to_string().contains("distinct endpoints"), "{error}");
    }
}

#[test]
fn window_accepts_only_exact_always_at_load() {
    #[derive(Deserialize)]
    struct Config {
        #[serde(rename = "window")]
        _window: Window,
    }
    assert!(toml::from_str::<Config>(r#"window = "always""#).is_ok());
    for value in ["Always", "always ", "anytime"] {
        let text = format!("window = {value:?}");
        assert!(toml::from_str::<Config>(&text).is_err(), "{value:?}");
    }
}

#[test]
fn older_observation_cannot_override_a_newer_hold_or_deletion() {
    let (_root, mut state) = fixture(Some("03:00-05:00"));
    let now = millis("2026-01-01T03:30:00Z");
    let absent = state.observe(now);
    write(&state, "2026-01-01T04:00:00Z");
    let present = state.observe(now);
    assert!(state.apply(now, true, present.clone()));
    assert!(state.apply(now, true, absent));
    std::fs::remove_file(state.hold_dir.join("hold.json")).unwrap();
    let _deleted = state.observe(now);
    assert!(!state.apply(now, true, present));
    assert!(!state.grant_dir.join("granted.json").exists());
}
