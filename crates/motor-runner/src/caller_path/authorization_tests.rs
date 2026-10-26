use super::*;
use std::os::unix::fs::symlink;

struct Fixture {
    scratch: cortexkit_test_support::ScratchDir,
    identity: PathBuf,
    config: crate::Config,
}

impl Fixture {
    fn new() -> Self {
        let scratch = cortexkit_test_support::ScratchDir::new("motor-caller-authorization");
        let host = scratch.join("mapped");
        // Identity callers must stay outside sandbox-reserved /tmp and
        // /motor-home, where the build server normally puts scratch folders.
        let identity = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/scratch")
            .join(format!(
                "caller-identity-{}",
                motor_protocol::Uuid::now_v7()
            ));
        fs::create_dir_all(&host).unwrap();
        fs::create_dir_all(&identity).unwrap();
        let mut config = crate::Config::new(scratch.join("state"), "main");
        config.caller_roots = vec![CallerRoot {
            caller_prefix: "/home".into(),
            host_root: host,
        }];
        config.owned_workspace_roots = vec![identity.clone()];
        Self {
            scratch,
            identity,
            config,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.identity);
    }
}

#[test]
fn mapped_authorization_spelling_and_component_table() {
    let f = Fixture::new();
    for path in [
        "/home",
        "/home/",
        "/home/u//repo",
        "/home/u/repo/",
        "/home/u/./repo",
        "/home/u/../repo",
    ] {
        assert!(authorize(&f.config, path, false).is_err(), "{path}");
    }
    assert_eq!(
        authorize(&f.config, "/home/u/repo", false).unwrap(),
        f.scratch.join("mapped/u/repo")
    );
    for path in ["/home2/x", "/homeX/x"] {
        assert!(mapped_root(&f.config, path).is_none());
        let mut identity = f.config.clone();
        identity
            .owned_workspace_roots
            .push(PathBuf::from(path).parent().unwrap().into());
        assert_eq!(
            authorize(&identity, path, false).unwrap(),
            PathBuf::from(path)
        );
    }
    for path in ["relative/path", "/home2/../home/u/repo"] {
        assert!(authorize(&f.config, path, false).is_err(), "{path}");
    }
    assert!(authorize(
        &f.config,
        f.scratch.join("mapped/u/repo").to_str().unwrap(),
        false
    )
    .is_err());
}

#[test]
fn mapped_authorization_reserved_names_and_fill_exemption() {
    let f = Fixture::new();
    for name in [
        ".motor-trash-main",
        ".motor-trash-e2e",
        ".motor-owner",
        ".motor-base-x",
    ] {
        for suffix in ["", "/secret"] {
            let path = format!("/home/{name}{suffix}");
            assert!(authorize(&f.config, &path, false).is_err(), "{path}");
        }
    }
    let checkout = fill_checkout(&f.config, "/home/repo", "/home/repo").unwrap();
    assert!(authorize(&f.config, checkout.to_str().unwrap(), true).is_ok());
    assert!(authorize(&f.config, checkout.to_str().unwrap(), false).is_err());
    let nested = fill_checkout(&f.config, "/home/u/repo", "/home/u/repo").unwrap();
    assert!(authorize(&f.config, nested.to_str().unwrap(), true).is_ok());
    assert!(authorize(&f.config, nested.to_str().unwrap(), false).is_err());
}

#[test]
fn mapped_authorization_rejects_namespace_crossings_and_symlinks() {
    let f = Fixture::new();
    symlink(&f.config.caller_roots[0].host_root, f.identity.join("link")).unwrap();
    let error = authorize(
        &f.config,
        f.identity.join("link/repo").to_str().unwrap(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("identity path resolves into a mapped namespace"),
        "{error}"
    );
    symlink(f.identity.clone(), f.scratch.join("mapped/link")).unwrap();
    assert!(authorize(&f.config, "/home/link/repo", false)
        .unwrap_err()
        .to_string()
        .contains("symlink in mapped destination"));
    symlink("missing", f.scratch.join("mapped/dangling")).unwrap();
    assert!(authorize(&f.config, "/home/dangling/repo", false).is_err());
}

fn sibling(path: &str) -> SiblingSnapshot {
    SiblingSnapshot {
        canonical_path: path.into(),
        commit: String::new(),
        tree_hash: String::new(),
        base_commit: None,
    }
}

#[test]
fn mapped_alias_node_table_never_resolves_caller_targets() {
    let f = Fixture::new();
    let identity_workspace = f.identity.join("main").to_string_lossy().into_owned();
    let identity_sibling = f.identity.join("sibling").to_string_lossy().into_owned();
    for mapped_parent in [false, true] {
        let workspace = if mapped_parent {
            "/home/u/main"
        } else {
            &identity_workspace
        };
        let sibling = sibling(if mapped_parent {
            &identity_sibling
        } else {
            "/home/u/sibling"
        });
        let parent = if mapped_parent {
            f.scratch.join("mapped/u")
        } else {
            f.identity.clone()
        };
        fs::create_dir_all(&parent).unwrap();
        let node = parent.join("sibling");
        assert!(aliases(&f.config, workspace, std::slice::from_ref(&sibling)).is_ok());
        fs::create_dir(&node).unwrap();
        assert!(aliases(&f.config, workspace, std::slice::from_ref(&sibling)).is_ok());
        fs::remove_dir(&node).unwrap();
        symlink(super::sibling(&sibling).caller.text(), &node).unwrap();
        assert_eq!(
            aliases(&f.config, workspace, std::slice::from_ref(&sibling)).is_ok(),
            !mapped_parent
        );
        fs::remove_file(&node).unwrap();
        symlink("/home/u/./sibling", &node).unwrap();
        assert!(aliases(&f.config, workspace, std::slice::from_ref(&sibling)).is_err());
        fs::remove_file(&node).unwrap();
    }
}

#[test]
fn collision_table_uses_components_and_bind_plan() {
    let f = Fixture::new();
    let reserved = vec![
        PathBuf::from("/home/ubuntu/.rustup"),
        PathBuf::from("/home/ubuntu/.bun/bin"),
    ];
    for (path, accepted) in [
        ("/home/ubuntu", false),
        ("/home/ubuntu/.rustup", false),
        ("/home/ubuntu/.rustup/x", false),
        ("/home/ubuntu/.bun", false),
        ("/home/ubuntu/project", true),
        ("/home/ubuntu/.rustup2", true),
        ("/home/ubuntu/.bun/lib", true),
    ] {
        assert_eq!(collision(path, &reserved).is_ok(), accepted, "{path}");
    }
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| {
        binds
            .borrow_mut()
            .push((f.scratch.join("source"), PathBuf::from("/home/u/new-tool")))
    });
    let result = authorize(&f.config, "/home/u/new-tool/child", false);
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| binds.borrow_mut().clear());
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains("/home/u/new-tool/child") && error.contains("/home/u/new-tool"),
        "{error}"
    );
}

#[test]
fn collision_refuses_every_request_field_and_alias_before_mutation() {
    let mut f = Fixture::new();
    f.config.isolate_jobs = true;
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| {
        binds.borrow_mut().push((
            f.scratch.join("rustup"),
            PathBuf::from("/home/ubuntu/.rustup"),
        ))
    });
    for field in ["workspace", "repository", "cwd", "sibling", "alias"] {
        let mut workspace = "/home/ubuntu/project".to_string();
        let mut repository = workspace.clone();
        let mut cwd = workspace.clone();
        let mut siblings = vec![];
        let collision_path = "/home/ubuntu/.rustup";
        match field {
            "workspace" => workspace = collision_path.into(),
            "repository" => repository = collision_path.into(),
            "cwd" => cwd = collision_path.into(),
            "sibling" => siblings.push(sibling(collision_path)),
            "alias" => siblings.push(sibling(f.identity.join(".rustup").to_str().unwrap())),
            _ => unreachable!(),
        }
        let plan = RequestPlan {
            caller_workspace_key: CallerPath(&workspace),
            host_workspace: host_str(&workspace),
            caller_repository_root: CallerPath(&repository),
            host_repository_root: host_str(&repository),
        };
        let error = authorize_request(&f.config, plan, Some(CallerPath(&cwd)), &siblings)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(collision_path) && error.contains("runner-owned sandbox destination"),
            "{field}: {error}"
        );
    }
    crate::isolation::EXTRA_RUNNER_BINDS.with(|binds| binds.borrow_mut().clear());
    assert!(!f.scratch.join("mapped/ubuntu").exists());
}
