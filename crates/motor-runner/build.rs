use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!value.is_empty()).then_some(value)
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
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
    let commit = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=GIT_COMMIT={commit}");
}
