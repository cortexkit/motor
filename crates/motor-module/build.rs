use sha2::{Digest, Sha256};
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    Some(value)
}

fn lock_digest() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let contents = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(contents)))
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    for input in ["Cargo.toml", "Cargo.lock"] {
        let path = workspace.join(input);
        if path.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    for input in ["crates", "scripts"] {
        let path = workspace.join(input);
        if path.is_dir() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    if let Some(head) = git(&["rev-parse", "--path-format=absolute", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={head}");
    }
    if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(path) = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            &reference,
        ]) {
            // A packed branch ref has no loose file. Naming a missing path makes
            // cargo rerun this script on every build, so name the loose ref only
            // when it exists, and watch packed-refs (where packed refs move) when that exists.
            if std::path::Path::new(&path).is_file() {
                println!("cargo:rerun-if-changed={path}");
            }
        }
        if let Some(packed) = git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "packed-refs",
        ]) {
            if std::path::Path::new(&packed).is_file() {
                println!("cargo:rerun-if-changed={packed}");
            }
        }
    }
    let revision = git(&["rev-parse", "HEAD"]);
    let status = git(&["status", "--porcelain"]);
    let commit = revision
        .as_deref()
        .filter(|revision| !revision.is_empty())
        .unwrap_or("unknown");
    println!("cargo:rustc-env=GIT_COMMIT={commit}");
    println!(
        "cargo:rustc-env=CK_BUILD_GIT_SHA={}",
        revision.as_deref().unwrap_or_default()
    );
    let tree_state = match status {
        Some(status) if status.is_empty() => "clean",
        Some(_) => "dirty",
        None => "",
    };
    println!("cargo:rustc-env=CK_BUILD_GIT_STATUS={tree_state}");
    println!(
        "cargo:rustc-env=CK_BUILD_LOCK_DIGEST={}",
        lock_digest().unwrap_or_default()
    );
}
