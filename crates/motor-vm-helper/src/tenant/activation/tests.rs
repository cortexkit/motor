use super::*;
use serde_json::json;

const VECTOR: &[u8] = include_bytes!("../../../tests/vectors/runner-args-staged.json");

#[test]
fn staged_vector_and_guest_argument_grammar_are_strict_and_bounded() {
    let activation = ResizeActivation::parse(VECTOR).unwrap();
    assert!(activation.quota_grew);
    assert!(activation
        .runner_args
        .contains("--max-weight 64 --rustc-slots 64"));
    for (from, to) in [
        ("--backend host", "--backend vm"),
        ("--tenant-vm", ""),
        ("--job-network veth", "--job-network none"),
        ("--max-weight 64", "--max-weight 0"),
        ("--max-weight 64", "--max-weight 4294967296"),
        ("--rustc-slots 64", "--rustc-slots 63"),
        ("--sccache-size-gib 5", ""),
        ("--sccache-size-gib 5", "--sccache-size-gib -1"),
        ("--free-space-floor-bytes 2147483648", ""),
        ("2147483648", "18446744073709551616"),
        ("--workspace-root /home", "--workspace-root /other"),
        ("--owned-workspace-root /Users", ""),
    ] {
        let args = activation.runner_args.replace(from, to);
        assert!(validate_runner_args(&args).is_err(), "{args}");
    }
    for suffix in [
        " --unknown 1",
        " --backend",
        " --state-root /other",
        " $(touch /tenant/pwn)",
    ] {
        assert!(validate_runner_args(&format!("{}{suffix}", activation.runner_args)).is_err());
    }
    validate_runner_args(&format!(
        "{} --state-root /tenant/state",
        activation.runner_args
    ))
    .unwrap();
    let exact = format!(
        "{:<width$}",
        activation.runner_args,
        width = MAX_RUNNER_ARGS
    );
    validate_runner_args(&exact).unwrap();
    assert!(validate_runner_args(&(exact + " ")).is_err());
    for bad in [
        json!({}),
        json!({"runner_args":activation.runner_args}),
        json!({"quota_grew":true}),
        json!({"runner_args":activation.runner_args,"quota_grew":null}),
        json!({"runner_args":activation.runner_args,"quota_grew":"true"}),
        json!({"runner_args":activation.runner_args,"quota_grew":true,"extra":0}),
    ] {
        assert!(
            ResizeActivation::parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
            "{bad}"
        );
    }
    assert!(ResizeActivation::parse(b"{").is_err());
}
