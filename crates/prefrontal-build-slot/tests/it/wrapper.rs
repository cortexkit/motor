#[test]
fn native_wrapper_integration() {
    run_lifecycle_test("original");
}

fn run_lifecycle_test(name: &str) {
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/it/lifecycle.py"
        ))
        .arg(name)
        .env(
            "BUILD_SLOT_BINARY",
            env!("CARGO_BIN_EXE_prefrontal-build-slot"),
        )
        .status()
        .expect("run native wrapper integration tests");
    assert!(status.success(), "native wrapper integration tests failed");
}

#[test]
fn background_descendant_cannot_pin_slot() {
    run_lifecycle_test("LifecycleTests.test_background_descendant_cannot_pin_slot");
}

#[test]
fn compile_exit_code_is_preserved() {
    run_lifecycle_test("LifecycleTests.test_compile_exit_code_is_preserved");
}

#[test]
fn cache_path_trims_surrounding_whitespace() {
    run_lifecycle_test("LifecycleTests.test_cache_path_trims_surrounding_whitespace");
}

#[test]
fn signal_reaches_child_and_launcher_reaps_it() {
    run_lifecycle_test("LifecycleTests.test_signal_reaches_child_and_launcher_reaps_it");
}

#[test]
fn ignored_signal_cannot_leave_child_running() {
    run_lifecycle_test("LifecycleTests.test_ignored_signal_cannot_leave_child_running");
}

#[test]
fn waiter_reads_cache_after_acquiring_slot() {
    run_lifecycle_test("LifecycleTests.test_waiter_reads_cache_after_acquiring_slot");
}
