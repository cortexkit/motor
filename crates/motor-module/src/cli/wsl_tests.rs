use super::*;
use crate::wsl::{Checks, Probe};

fn aliases(path: &Path) -> std::io::Result<PathBuf> {
    match path.to_str().unwrap() {
        "/home/u/link" => Ok("/mnt/c/project".into()),
        "/mnt/c/link" => Ok("/home/u/project".into()),
        _ => Err(std::io::ErrorKind::NotFound.into()),
    }
}

#[tokio::test]
async fn wsl_cli_refuses_both_alias_directions_before_request_or_connection() {
    let checks = Checks::test(Probe::Interop).with_canonicalize(aliases);
    for path in ["/home/u/link", "/mnt/c/link"] {
        let words = ["--cwd", path, "--return-path", "/out", "--", "true"].map(str::to_string);
        let Invocation::Run(args) = parse_args_checked(&words, &checks).unwrap() else {
            panic!("expected run arguments");
        };
        assert_eq!(build_request_checked(&args, &checks).unwrap_err(), format!("ck-motor: this checkout is on a Windows drive ({path}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there."));
        assert_eq!(main_checked(&words, &checks).await, 200);
    }
}

#[test]
fn wsl_cli_checks_siblings_before_canonicalizing_and_checks_default_cwd() {
    let checks = Checks::test(Probe::DistroName).with_canonicalize(aliases);
    let words = [
        "--cwd",
        "/home/u/project",
        "--sibling",
        "/mnt/C/lib",
        "--",
        "true",
    ]
    .map(str::to_string);
    let Invocation::Run(args) = parse_args(&words).unwrap() else {
        panic!("expected run arguments");
    };
    assert_eq!(build_request_checked(&args, &checks).unwrap_err(), "ck-motor: this checkout is on a Windows drive (/mnt/C/lib). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there.");

    let Invocation::Run(args) = parse_args(&["--".into(), "true".into()]).unwrap() else {
        panic!("expected run arguments");
    };
    let checks = Checks::test(Probe::InteropLate).with_canonicalize(|path| {
        assert_eq!(path, std::env::current_dir().unwrap());
        Ok("/mnt/c/project".into())
    });
    assert_eq!(build_request_checked(&args, &checks).unwrap_err(), format!("ck-motor: this checkout is on a Windows drive ({}). Under WSL, move it into the WSL filesystem (for example under ~/) and run ck motor there.", std::env::current_dir().unwrap().display()));
}
