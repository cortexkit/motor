use super::*;

const ROLES: [ToolchainRole; 3] = [
    ToolchainRole::BuildServer,
    ToolchainRole::Boat,
    ToolchainRole::Tenant,
];

fn manifest() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../../script/runner/toolchain.json")).unwrap()
}

/// The path a role's image installs `tool` at, read from the manifest the
/// image recipes install from.
fn manifest_path(manifest: &serde_json::Value, role: ToolchainRole, tool: &str) -> PathBuf {
    let spec = &manifest["tools"][tool];
    spec["path_by_role"][role.name()]
        .as_str()
        .or(spec["path"].as_str())
        .unwrap_or_else(|| panic!("toolchain.json has no path for {tool}"))
        .into()
}

#[test]
fn each_roles_javascript_tools_are_where_the_toolchain_manifest_installs_them() {
    let manifest = manifest();
    for role in ROLES {
        let tools = JavascriptTools::for_role(role);
        for tool in ["bun", "node", "npm", "pnpm"] {
            assert_eq!(
                tools.executable(tool),
                manifest_path(&manifest, role, tool),
                "{} {tool}",
                role.name()
            );
        }
    }
}

#[test]
fn jobs_on_every_role_find_bun_where_the_runner_mounts_it() {
    let manifest = manifest();
    let job_bun = manifest["tools"]["bun"]["job_path"]
        .as_str()
        .or(manifest["tools"]["bun"]["path"].as_str())
        .unwrap();
    assert_eq!(Path::new(job_bun), Path::new(JOB_BUN_DIR).join("bun"));
    for role in ROLES {
        let path = manifest["job_environment_by_role"][role.name()]["PATH"]
            .as_str()
            .unwrap();
        // The build server's jobs also get /motor-tools, its compile wrapper
        // directory, ahead of everything else.
        assert_eq!(
            path.split(':').find(|entry| *entry != "/motor-tools"),
            Some(JOB_BUN_DIR),
            "{} job PATH",
            role.name()
        );
    }
}

/// The tenant agent binds the tenant's own directories over these roots
/// before it starts the runner, which then sees none of the image's files
/// beneath them. That is how Bun at /home/ubuntu/.bun/bin went missing.
#[test]
fn tenant_tools_are_outside_the_directories_the_tenant_agent_binds_over() {
    let tools = JavascriptTools::for_role(ToolchainRole::Tenant);
    for tool in ["bun", "node", "npm", "pnpm"] {
        let path = tools.executable(tool);
        for root in crate::tenant_agent::ROOTS {
            assert!(
                !path.starts_with(root),
                "tenant {tool} at {} is hidden under {root}",
                path.display()
            );
        }
    }
}

#[test]
fn role_follows_the_serve_flags() {
    let mut config = crate::engine::Config::new("/nonexistent", "role");
    assert_eq!(ToolchainRole::of(&config), ToolchainRole::BuildServer);
    config.tenant_vm = true;
    assert_eq!(ToolchainRole::of(&config), ToolchainRole::Tenant);
    // Boat runs with both flags; its image is laid out like the build server.
    config.boat_profile = true;
    assert_eq!(ToolchainRole::of(&config), ToolchainRole::Boat);
}

#[test]
fn require_refuses_by_the_configured_path_and_searches_nowhere_else() {
    let scratch = Scratch::new();
    let bun_dir = scratch.0.join("usr/local/lib/motor/bun/bin");
    let node_dir = scratch.0.join("usr/local/bin");
    fs_dirs(&[&bun_dir, &node_dir]);
    let tools = JavascriptTools::at(ToolchainRole::Tenant, &bun_dir, &node_dir);
    let error = tools.require("bun").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "bun not installed at {}",
            bun_dir.join("bun").display()
        )),
        "{message}"
    );
    assert!(message.contains("tenant toolchain layout"), "{message}");
    // A file that is not executable is not a tool either.
    std::fs::write(bun_dir.join("bun"), "#!/bin/sh\n").unwrap();
    assert!(tools.require("bun").is_err());
    crate::test_helpers::write_test_executable(&bun_dir.join("bun"), b"#!/bin/sh\n").unwrap();
    assert_eq!(tools.require("bun").unwrap(), bun_dir.join("bun"));
    assert!(tools
        .require("npm")
        .unwrap_err()
        .to_string()
        .contains(&node_dir.join("npm").display().to_string()));
}

#[test]
fn managers_search_the_configured_directories_then_fixed_system_ones() {
    let tenant = JavascriptTools::for_role(ToolchainRole::Tenant);
    assert_eq!(
        tenant.search_path(),
        "/usr/local/lib/motor/bun/bin:/usr/local/bin:/usr/bin:/bin"
    );
    let server = JavascriptTools::for_role(ToolchainRole::BuildServer);
    assert_eq!(
        server.search_path(),
        "/home/ubuntu/.bun/bin:/usr/local/bin:/usr/bin:/bin"
    );
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        Self(
            cortexkit_test_support::scratch_root()
                .join(format!("motor-javascript-tools-{}", uuid::Uuid::now_v7())),
        )
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fs_dirs(paths: &[&Path]) {
    for path in paths {
        std::fs::create_dir_all(path).unwrap();
    }
}
