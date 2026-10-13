use super::*;
use std::os::unix::fs::MetadataExt;

#[test]
#[ignore = "requires the trusted build-server isolation gate; see motor-runner README"]
fn isolated_retained_target_replaces_an_existing_build_script_out_dir_file() {
    let f = Fixture::new();
    fs::create_dir_all(f.workspace.join("src")).unwrap();
    fs::write(
        f.workspace.join("Cargo.toml"),
        "[package]\nname='out-dir-copy'\nversion='0.1.0'\nedition='2021'\nbuild='build.rs'\n",
    )
    .unwrap();
    fs::write(
        f.workspace.join("Cargo.lock"),
        "version = 4\n\n[[package]]\nname = 'out-dir-copy'\nversion = '0.1.0'\n",
    )
    .unwrap();
    fs::write(
        f.workspace.join("src/lib.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/bindings.rs\"));\n",
    )
    .unwrap();
    fs::write(
        f.workspace.join("source.rs"),
        "pub fn generated() { let _ = 1; }\n",
    )
    .unwrap();
    fs::write(
        f.workspace.join("build.rs"),
        "use std::{env, fs, path::Path};\nfn main() {\n  println!(\"cargo:rerun-if-changed=source.rs\");\n  let manifest = env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n  let out = env::var(\"OUT_DIR\").unwrap();\n  fs::write(Path::new(&manifest).join(\"out-dir.txt\"), &out).unwrap();\n  fs::copy(Path::new(&manifest).join(\"source.rs\"), Path::new(&out).join(\"bindings.rs\")).unwrap();\n}\n",
    )
    .unwrap();

    let target = f.workspace.join("target");
    let first = Command::new("cargo")
        .args(["check", "--locked", "--manifest-path"])
        .arg(f.workspace.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target)
        .current_dir(&f.workspace)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "could not build the target fixture: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let destination = PathBuf::from(fs::read_to_string(f.workspace.join("out-dir.txt")).unwrap())
        .join("bindings.rs");
    assert!(destination.starts_with(&target));
    assert_eq!(
        fs::metadata(&destination).unwrap().uid(),
        unsafe { libc::geteuid() },
        "retained target should contain a daemon-owned existing OUT_DIR file"
    );
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        "pub fn generated() { let _ = 1; }\n"
    );

    fs::write(
        f.workspace.join("source.rs"),
        "pub fn generated() { let _ = 2; }\n",
    )
    .unwrap();
    let sandbox = f.sandbox();
    let job_uid = Command::new("/usr/bin/id")
        .args(["-u", USER])
        .output()
        .unwrap();
    let job_uid = String::from_utf8_lossy(&job_uid.stdout)
        .trim()
        .parse::<u32>()
        .unwrap();
    assert_eq!(fs::metadata(&destination).unwrap().uid(), job_uid);
    let (status, stdout, stderr) = output(
        &sandbox,
        "cargo check --locked --manifest-path Cargo.toml",
        &f.workspace,
    );
    assert!(status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        fs::read_to_string(&destination).unwrap(),
        "pub fn generated() { let _ = 2; }\n"
    );
    restore_runner_access(&f.scratch).unwrap();
}
