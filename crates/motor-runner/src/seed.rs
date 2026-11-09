//! Reflink seeding is a recoverable transaction, never a copy fallback. An
//! intent authorizes cleanup by inode, not by an untrusted workspace name.
use crate::build_base::{self, Compatibility, Record};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString, OsStr},
    fs::{self, File},
    io::{self, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(file: &File) -> io::Result<Self> {
        let stat = file.metadata()?;
        Ok(Self {
            device: stat.dev(),
            inode: stat.ino(),
        })
    }
}

#[derive(Serialize, Deserialize)]
struct Intent {
    staging: String,
    directories: Option<[Identity; 2]>,
}

fn name(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(io::Error::other)
}

fn open(parent: &File, entry: &CStr, flags: i32) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            entry.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn directory(parent: &File, entry: &CStr) -> io::Result<File> {
    open(parent, entry, libc::O_RDONLY | libc::O_DIRECTORY)
}

fn existing_directory(parent: &File, entry: &CStr) -> io::Result<Option<File>> {
    match directory(parent, entry) {
        Ok(directory) => Ok(Some(directory)),
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn root(path: &Path) -> io::Result<File> {
    let pin = File::from(crate::permissions::physical_root(path)?);
    directory(&pin, c".")
}

fn children(directory: &File) -> io::Result<Vec<std::ffi::OsString>> {
    // proc names only the pinned directory, never an entry supplied by a job.
    fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect()
}

fn unlink(parent: &File, entry: &CStr, flags: i32) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent.as_raw_fd(), entry.as_ptr(), flags) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn clear(directory: &File, device: u64) -> io::Result<()> {
    for entry in children(directory)? {
        let entry = name(&entry)?;
        let pin = open(directory, &entry, libc::O_PATH)?;
        let stat = pin.metadata()?;
        if stat.dev() != device {
            return Err(io::Error::other("mount in seed directory"));
        }
        if stat.is_dir() {
            let child = self::directory(directory, &entry)?;
            if Identity::of(&child)? != Identity::of(&pin)? {
                return Err(io::Error::other("seed directory replaced"));
            }
            clear(&child, device)?;
            unlink(directory, &entry, libc::AT_REMOVEDIR)?;
        } else {
            unlink(directory, &entry, 0)?;
        }
    }
    Ok(())
}

fn intent_path(state: &Path, workspace: &Path) -> PathBuf {
    state
        .join("seed-intents")
        .join(crate::workspace::name(workspace.to_str().unwrap()))
}

fn save(parent: &File, entry: &CStr, intent: &Intent) -> io::Result<()> {
    let mut file = open(parent, entry, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL)?;
    file.write_all(&serde_json::to_vec(intent)?)?;
    file.sync_all()?;
    parent.sync_all()
}

fn remove_intent(parent: &File, entry: &CStr) -> io::Result<()> {
    unlink(parent, entry, 0)?;
    parent.sync_all()
}

/// Before populate, and after any failed seed, remove only directories the
/// transaction recorded. In particular, EEXIST never grants cleanup authority.
pub(crate) fn settle(state: &Path, workspace: &Path) -> io::Result<()> {
    let path = intent_path(state, workspace);
    let parent = match root(path.parent().unwrap()) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let entry = name(path.file_name().unwrap())?;
    let file = match open(&parent, &entry, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let intent: Intent = serde_json::from_reader(file)?;
    if !intent.staging.starts_with(".motor-seed-")
        || Path::new(&intent.staging).components().count() != 1
    {
        return Err(io::Error::other("invalid seed intent staging name"));
    }
    if let Some([new, empty]) = intent.directories {
        let workspace = root(workspace)?;
        if let Some(target) = existing_directory(&workspace, c"target")? {
            if Identity::of(&target)? == new {
                clear(&target, new.device)?;
            }
        }
        let staging = name(OsStr::new(&intent.staging))?;
        if let Some(staged) = existing_directory(&workspace, &staging)? {
            let identity = Identity::of(&staged)?;
            if identity == new || identity == empty {
                clear(&staged, identity.device)?;
                unlink(&workspace, &staging, libc::AT_REMOVEDIR)?;
            }
        }
    }
    remove_intent(&parent, &entry)
}

pub(crate) fn pin_target(workspace: &Path) -> io::Result<File> {
    directory(&root(workspace)?, c"target")
}

/// Sandbox preparation can lose a cache transport after seeding. Check the
/// environment launch actually uses and discard incompatible artifacts through
/// the still-pinned seeded directory, never a workspace pathname.
pub(crate) fn confirm_environment(
    target: &File,
    planned: &std::collections::BTreeMap<String, String>,
    prepared: &std::collections::BTreeMap<String, String>,
    job: motor_protocol::Uuid,
) -> io::Result<bool> {
    use crate::compile_cache::environment_fingerprint;
    if environment_fingerprint(planned) == environment_fingerprint(prepared) {
        return Ok(true);
    }
    let names: std::collections::BTreeSet<_> = planned.keys().chain(prepared.keys()).collect();
    let differed: Vec<_> = names
        .into_iter()
        .filter(|name| {
            let single = |env: &std::collections::BTreeMap<String, String>| {
                env.get(*name)
                    .map(|value| ((*name).clone(), value.clone()))
                    .into_iter()
                    .collect()
            };
            environment_fingerprint(&single(planned)) != environment_fingerprint(&single(prepared))
        })
        .collect();
    tracing::warn!(job_id = %job, inputs = ?differed, "build base seed environment changed during sandbox preparation; building without seed");
    clear(target, Identity::of(target)?.device)?;
    Ok(false)
}

fn exchange(parent: &File, staging: &CStr) -> io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            staging.as_ptr(),
            parent.as_raw_fd(),
            c"target".as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    parent.sync_all()
}

type Tree = std::collections::BTreeMap<PathBuf, (String, String)>;

fn tree(mirror: &Path, tree: &str) -> io::Result<Tree> {
    let bytes = crate::workspace::git_bytes(mirror, &["ls-tree", "-r", "-t", "-z", tree])?;
    let mut result = Tree::new();
    for entry in bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let tab = entry
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| io::Error::other("invalid seed tree entry"))?;
        let fields: Vec<_> = std::str::from_utf8(&entry[..tab])
            .map_err(io::Error::other)?
            .split_whitespace()
            .collect();
        let path = PathBuf::from(OsStr::from_bytes(&entry[tab + 1..]));
        if fields.len() != 3
            || !matches!(fields[0], "040000" | "100644" | "100755" | "120000")
            || path.as_os_str().is_empty()
            || !path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
            || matches!(path.components().next(), Some(std::path::Component::Normal(n)) if n == ".git" || n == "target")
        {
            return Err(io::Error::other("unsupported seed tree entry"));
        }
        result.insert(path, (fields[0].into(), fields[2].into()));
    }
    Ok(result)
}

fn only_tracked(root: &File, prefix: &Path, entries: &Tree, device: u64) -> io::Result<bool> {
    for entry in children(root)? {
        if prefix.as_os_str().is_empty() && (entry == ".git" || entry == "target") {
            continue;
        }
        let path = prefix.join(&entry);
        let Some((mode, _)) = entries.get(&path) else {
            return Ok(false);
        };
        let entry = name(&entry)?;
        let pin = open(root, &entry, libc::O_PATH)?;
        let stat = pin.metadata()?;
        if stat.dev() != device {
            return Ok(false);
        }
        match mode.as_str() {
            "040000" if stat.is_dir() => {
                let child = directory(root, &entry)?;
                if Identity::of(&child)? != Identity::of(&pin)?
                    || !only_tracked(&child, &path, entries, device)?
                {
                    return Ok(false);
                }
            }
            "120000" if stat.is_symlink() => {}
            "100644" | "100755" if stat.is_file() => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

struct Attempt<'a> {
    state: &'a Path,
    workspace: &'a Path,
    store: &'a Path,
    mirror: &'a Path,
    tree: &'a str,
    fixed: &'a Path,
    compatibility: &'a Compatibility,
}

/// Operations are supplied explicitly so portable tests can interrupt
/// transaction boundaries and inject filesystem failures.
struct Operations<'a> {
    clone: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
    repair: &'a dyn Fn(&Path) -> io::Result<()>,
    times: &'a dyn Fn(&Record) -> io::Result<()>,
    hook: &'a dyn Fn(&str),
    staging: String,
}

fn record_matches(
    record: &Record,
    attempt: &Attempt<'_>,
    base_device: u64,
    target_device: u64,
) -> io::Result<bool> {
    let compatible = attempt.compatibility.for_seed().map_err(io::Error::other)?;
    Ok(record.format == 1
        && record.fixed_path == attempt.fixed
        && record.inputs == *compatible
        && base_device == target_device)
}

fn clone_contents(
    source: &Path,
    staged: &File,
    clone: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    // The reflink backend replaces its destination directory. Clone beneath
    // the recorded staging inode, then move the entries out without replacing
    // that inode; an interrupted clone remains inside its cleanup boundary.
    let container = format!("clone-{}", motor_protocol::Uuid::now_v7());
    let destination = PathBuf::from(format!(
        "/proc/self/fd/{}/{}",
        staged.as_raw_fd(),
        container
    ));
    clone(source, &destination)?;
    let container = name(OsStr::new(&container))?;
    let cloned = directory(staged, &container)?;
    for child in children(&cloned)? {
        let child = name(&child)?;
        if unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                cloned.as_raw_fd(),
                child.as_ptr(),
                staged.as_raw_fd(),
                child.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    unlink(staged, &container, libc::AT_REMOVEDIR)?;
    staged.sync_all()
}

fn run(attempt: &Attempt<'_>, ops: &Operations<'_>) -> io::Result<bool> {
    let workspace = root(attempt.workspace)?;
    // mkdirat refuses an existing link or directory; never create target through
    // a workspace entry, even in a checkout that passed content verification.
    if unsafe { libc::mkdirat(workspace.as_raw_fd(), c"target".as_ptr(), 0o755) } != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(error);
        }
    }
    let target = directory(&workspace, c"target")?;
    if !children(&target)?.is_empty() {
        return Ok(false);
    }
    let entries = tree(attempt.mirror, attempt.tree)?;
    if !only_tracked(
        &workspace,
        Path::new(""),
        &entries,
        Identity::of(&workspace)?.device,
    )? {
        return Ok(false);
    }
    let base = match build_base::pin_current(attempt.store) {
        Ok(base) => base,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let record: Record =
        match serde_json::from_reader(open(&base.directory, c"base.json", libc::O_RDONLY)?) {
            Ok(record) => record,
            Err(_) => return Ok(false),
        };
    if !record_matches(
        &record,
        attempt,
        Identity::of(&base.directory)?.device,
        Identity::of(&target)?.device,
    )? {
        return Ok(false);
    }
    let old = tree(attempt.mirror, &record.tree)?;
    if entries
        .iter()
        .chain(old.iter())
        .any(|(path, (mode, _))| mode == "120000" && entries.get(path) != old.get(path))
    {
        return Ok(false);
    }
    (ops.hook)("validated");
    let intent_path = intent_path(attempt.state, attempt.workspace);
    fs::create_dir_all(intent_path.parent().unwrap())?;
    let intents = root(intent_path.parent().unwrap())?;
    let intent_name = name(intent_path.file_name().unwrap())?;
    let mut intent = Intent {
        staging: ops.staging.clone(),
        directories: None,
    };
    save(&intents, &intent_name, &intent)?;
    (ops.hook)("intent");
    let staged_name = name(OsStr::new(&intent.staging))?;
    if unsafe { libc::mkdirat(workspace.as_raw_fd(), staged_name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let staged = directory(&workspace, &staged_name)?;
    (ops.hook)("mkdir");
    intent.directories = Some([Identity::of(&staged)?, Identity::of(&target)?]);
    let temporary = name(OsStr::new(&format!(
        "{}.{}",
        intent_path.file_name().unwrap().to_string_lossy(),
        motor_protocol::Uuid::now_v7()
    )))?;
    save(&intents, &temporary, &intent)?;
    if unsafe {
        libc::renameat(
            intents.as_raw_fd(),
            temporary.as_ptr(),
            intents.as_raw_fd(),
            intent_name.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    intents.sync_all()?;
    let source = PathBuf::from(format!(
        "/proc/self/fd/{}/target",
        base.directory.as_raw_fd()
    ));
    clone_contents(&source, &staged, ops.clone)?;
    drop(base);
    if Identity::of(&directory(&workspace, &staged_name)?)? != Identity::of(&staged)?
        || Identity::of(&directory(&workspace, c"target")?)? != Identity::of(&target)?
    {
        return Err(io::Error::other("seed directory replaced before exchange"));
    }
    (ops.hook)("cloned");
    exchange(&workspace, &staged_name)?;
    (ops.hook)("swapped");
    unlink(&workspace, &staged_name, libc::AT_REMOVEDIR)?;
    (ops.repair)(&attempt.workspace.join("target"))?;
    (ops.times)(&record)?;
    remove_intent(&intents, &intent_name)?;
    (ops.hook)("committed");
    Ok(true)
}

pub(crate) fn seed(
    config: &crate::Config,
    trees: &crate::workspace::Trees,
    setup: &crate::backend::Setup,
    fixed: Option<&crate::caller_path::FixedPaths>,
    env: &std::collections::BTreeMap<String, String>,
) -> io::Result<bool> {
    let request = &setup.request;
    let Some(fixed) =
        fixed.filter(|_| config.isolate_jobs && !setup.base && request.siblings.is_empty())
    else {
        return Ok(false);
    };
    let workspace = crate::caller_path::host_run(config, request).host_workspace;
    let repository = crate::caller_path::request(request)
        .caller_repository_root
        .text();
    let mirror = trees.mirror(repository);
    let compatibility = build_base::compatibility(
        &build_base::RunnerToolchain::probe(),
        &mirror,
        &request.tree_hash,
        env,
        &PathBuf::from(std::env::var_os("CARGO_HOME").unwrap_or_default()).join("bin/sccache"),
    );
    let state = config.state_dir();
    let store = build_base::repository_store(&state, repository);
    execute(
        &Attempt {
            state: &state,
            workspace: &workspace,
            store: &store,
            mirror: &mirror,
            tree: &request.tree_hash,
            fixed: &fixed.checkout,
            compatibility: &compatibility,
        },
        &Operations {
            clone: &crate::platform::clone_target,
            repair: &|target| {
                let repair = crate::isolation::prepare_job_writable_target(&state, target);
                // Repair can have changed some owners before failing. Restore the
                // runner's access before either the time step or intent cleanup.
                crate::isolation::restore_runner_access(target)?;
                repair
            },
            times: &|record| {
                #[cfg(test)]
                if config.seed_without_times {
                    return Ok(());
                }
                crate::seed_times::apply(&workspace, &mirror, &request.tree_hash, record)
                    .map(|_| ())
            },
            hook: &|_| {},
            staging: format!(".motor-seed-{}", motor_protocol::Uuid::now_v7()),
        },
        setup.id,
    )
}

fn execute(
    attempt: &Attempt<'_>,
    operations: &Operations<'_>,
    job: motor_protocol::Uuid,
) -> io::Result<bool> {
    match run(attempt, operations) {
        Ok(seeded) => Ok(seeded),
        Err(error) => {
            let cleanup = settle(attempt.state, attempt.workspace);
            tracing::warn!(job_id = %job, %error, ?cleanup, "build base seed failed; building without seed");
            // Never launch with partial artifacts if cleanup itself failed.
            cleanup?;
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        build_base::{CompatibleInputs, Timestamp, ToolchainIdentity},
        workspace,
    };
    use std::{cell::Cell, os::unix::fs::symlink};

    struct Fixture {
        scratch: cortexkit_test_support::ScratchDir,
        workspace: PathBuf,
        store: PathBuf,
        record: Record,
        compatibility: Compatibility,
    }

    impl Fixture {
        fn new() -> Self {
            let scratch = cortexkit_test_support::ScratchDir::new("seeding");
            let workspace = scratch.join("workspace");
            fs::create_dir(&workspace).unwrap();
            workspace::git(&workspace, &["init", "-q"]).unwrap();
            fs::write(workspace.join("source"), "tracked").unwrap();
            fs::write(
                workspace.join(".gitignore"),
                "/target\n.motor-seed-b\nignored\n",
            )
            .unwrap();
            fs::create_dir(workspace.join(".motor-seed-a")).unwrap();
            fs::write(workspace.join(".motor-seed-a/tracked"), "keep").unwrap();
            workspace::git(&workspace, &["add", "."]).unwrap();
            let tree = workspace::git(&workspace, &["write-tree"])
                .unwrap()
                .trim()
                .to_owned();
            let inputs = CompatibleInputs {
                toolchain: ToolchainIdentity {
                    rustc_verbose: "runner rustc".into(),
                    rust_toolchain: None,
                    rust_toolchain_toml: None,
                    wrapper: None,
                },
                environment: crate::compile_cache::environment_fingerprint(&Default::default()),
            };
            let record = Record {
                format: 1,
                commit: "unused".into(),
                tree,
                inputs: inputs.clone(),
                fixed_path: "/w/account/key/repo".into(),
                fill_start: Timestamp {
                    seconds: 1_000_000_000,
                    nanos: 0,
                },
                newest_mtime: Timestamp {
                    seconds: 1_000_000_001,
                    nanos: 0,
                },
            };
            let store = scratch.join("store");
            fs::create_dir_all(store.join("current/target")).unwrap();
            fs::write(store.join("current/target/artifact"), "old generation").unwrap();
            let fixture = Self {
                scratch,
                workspace,
                store,
                record,
                compatibility: Compatibility::Available(inputs),
            };
            fixture.save_record();
            fixture
        }

        fn save_record(&self) {
            fs::write(
                self.store.join("current/base.json"),
                serde_json::to_vec(&self.record).unwrap(),
            )
            .unwrap();
        }

        fn attempt(&self) -> Attempt<'_> {
            Attempt {
                state: &self.scratch,
                workspace: &self.workspace,
                store: &self.store,
                mirror: &self.workspace,
                tree: &self.record.tree,
                fixed: Path::new("/w/account/key/repo"),
                compatibility: &self.compatibility,
            }
        }

        fn snapshot(&self) -> String {
            workspace::git(&self.workspace, &["add", "-A"]).unwrap();
            workspace::git(&self.workspace, &["write-tree"])
                .unwrap()
                .trim()
                .into()
        }

        fn ops(&self) -> Operations<'static> {
            Operations {
                clone: &clone_file,
                repair: &|_| Ok(()),
                times: &|_| Ok(()),
                hook: &|_| {},
                staging: ".motor-seed-test".into(),
            }
        }

        fn assert_settled(&self) {
            assert!(children(&root(&self.workspace.join("target")).unwrap())
                .unwrap()
                .is_empty());
            assert!(!intent_path(&self.scratch, &self.workspace).exists());
            assert!(!self.workspace.join(".motor-seed-test").exists());
        }
    }

    fn clone_file(source: &Path, target: &Path) -> io::Result<()> {
        // This test clone checks the descriptor path contract. Production uses
        // the reflink-only backend; a separate test exercises that backend.
        assert!(source.starts_with("/proc/self/fd"));
        fs::create_dir_all(target)?;
        fs::write(target.join("artifact"), fs::read(source.join("artifact"))?)
    }

    #[test]
    fn seed_commits_once_and_kept_folder_is_not_time_set() {
        let fixture = Fixture::new();
        let times = Cell::new(0);
        let apply = |record: &Record| {
            times.set(times.get() + 1);
            crate::seed_times::apply(
                &fixture.workspace,
                &fixture.workspace,
                &fixture.record.tree,
                record,
            )
            .map(|_| ())
        };
        let mut ops = fixture.ops();
        ops.times = &apply;
        assert!(run(&fixture.attempt(), &ops).unwrap());
        assert_eq!(
            fs::read_to_string(fixture.workspace.join("target/artifact")).unwrap(),
            "old generation"
        );
        assert!(!intent_path(&fixture.scratch, &fixture.workspace).exists());
        assert_eq!(
            fs::metadata(fixture.workspace.join("source"))
                .unwrap()
                .mtime(),
            fixture.record.fill_start.seconds
        );
        assert!(!run(&fixture.attempt(), &ops).unwrap());
        assert_eq!(times.get(), 1);
    }

    #[test]
    fn reflink_backend_clones_into_pinned_staging_directory() {
        let fixture = Fixture::new();
        let mut ops = fixture.ops();
        ops.clone = &crate::platform::clone_target;
        assert!(run(&fixture.attempt(), &ops).unwrap());
        fs::write(
            fixture.workspace.join("target/artifact"),
            "job changed bytes",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(fixture.store.join("current/target/artifact")).unwrap(),
            "old generation"
        );
    }

    #[test]
    fn real_reflink_remains_within_recorded_cleanup_boundary() {
        let fixture = Fixture::new();
        let mut ops = fixture.ops();
        ops.clone = &crate::platform::clone_target;
        ops.times = &|_| Err(io::Error::other("time step interrupted"));
        assert!(!execute(&fixture.attempt(), &ops, motor_protocol::Uuid::now_v7()).unwrap());
        fixture.assert_settled();
    }

    #[test]
    fn eligibility_refuses_incompatible_records_and_leftovers() {
        for case in [
            "nonempty",
            "ignored",
            "format",
            "path",
            "rustc",
            "toolchain",
            "wrapper",
            "environment",
            "missing",
        ] {
            let mut fixture = Fixture::new();
            match case {
                "nonempty" => {
                    fs::create_dir(fixture.workspace.join("target")).unwrap();
                    fs::write(fixture.workspace.join("target/kept"), "keep").unwrap();
                }
                "ignored" => {
                    fs::write(fixture.workspace.join("ignored"), "keep").unwrap();
                }
                "format" => fixture.record.format = 2,
                "path" => fixture.record.fixed_path = "/w/other".into(),
                "rustc" => fixture.record.inputs.toolchain.rustc_verbose.push('x'),
                "toolchain" => {
                    fixture.record.inputs.toolchain.rust_toolchain = Some("different".into())
                }
                "wrapper" => {
                    fixture.record.inputs.toolchain.wrapper =
                        Some(build_base::WrapperIdentity::Supervisor {
                            source_sha256: "different".into(),
                        })
                }
                "environment" => fixture.record.inputs.environment.push('x'),
                "missing" => fs::remove_dir_all(fixture.store.join("current")).unwrap(),
                _ => unreachable!(),
            }
            if case != "missing" {
                fixture.save_record();
            }
            let mut ops = fixture.ops();
            ops.clone = &|_, _| panic!("ineligible seed reached clone");
            assert!(!run(&fixture.attempt(), &ops).unwrap(), "{case}");
            assert!(!intent_path(&fixture.scratch, &fixture.workspace).exists());
        }
    }

    #[test]
    fn concurrency_and_incremental_environment_differences_still_seed() {
        for variable in [
            "CARGO_BUILD_JOBS",
            "RUST_TEST_THREADS",
            "NEXTEST_TEST_THREADS",
            "CARGO_INCREMENTAL",
        ] {
            let mut fixture = Fixture::new();
            let mut inputs = fixture.record.inputs.clone();
            inputs.environment = crate::compile_cache::environment_fingerprint(
                &[(variable.into(), "8".into())].into(),
            );
            fixture.compatibility = Compatibility::Available(inputs);
            assert!(
                run(&fixture.attempt(), &fixture.ops()).unwrap(),
                "{variable}"
            );
        }
    }

    #[test]
    fn symlink_add_remove_and_retarget_refuse_seed() {
        for case in ["added", "removed", "retargeted"] {
            let mut fixture = Fixture::new();
            if case != "added" {
                symlink("source", fixture.workspace.join("link")).unwrap();
                fixture.record.tree = fixture.snapshot();
                fixture.save_record();
                fs::remove_file(fixture.workspace.join("link")).unwrap();
            }
            if case != "removed" {
                symlink(".gitignore", fixture.workspace.join("link")).unwrap();
            }
            let tree = fixture.snapshot();
            let mut attempt = fixture.attempt();
            attempt.tree = &tree;
            assert!(!run(&attempt, &fixture.ops()).unwrap(), "{case}");
        }
    }

    #[test]
    fn different_base_device_is_ineligible() {
        let fixture = Fixture::new();
        let device = fs::metadata(&fixture.workspace).unwrap().dev();
        assert!(!record_matches(
            &fixture.record,
            &fixture.attempt(),
            device.wrapping_add(1),
            device
        )
        .unwrap());
    }

    #[test]
    fn link_planted_after_verification_fails_time_step_without_touching_outside() {
        let fixture = Fixture::new();
        let outside = fixture.scratch.join("outside");
        fs::write(&outside, "outside").unwrap();
        let before = fs::metadata(&outside).unwrap();
        let hook = |stage: &str| {
            if stage == "swapped" {
                fs::remove_file(fixture.workspace.join("source")).unwrap();
                symlink(&outside, fixture.workspace.join("source")).unwrap();
            }
        };
        let times = |record: &Record| {
            crate::seed_times::apply(
                &fixture.workspace,
                &fixture.workspace,
                &fixture.record.tree,
                record,
            )
            .map(|_| ())
        };
        let mut ops = fixture.ops();
        ops.hook = &hook;
        ops.times = &times;
        assert!(!execute(&fixture.attempt(), &ops, motor_protocol::Uuid::now_v7()).unwrap());
        fixture.assert_settled();
        let after = fs::metadata(&outside).unwrap();
        assert_eq!(
            (after.mtime(), after.mtime_nsec()),
            (before.mtime(), before.mtime_nsec())
        );
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
    }

    #[test]
    fn current_exchange_before_and_during_clone_stays_on_validated_generation() {
        for boundary in ["validated", "mid-clone"] {
            let fixture = Fixture::new();
            let replace = || {
                let generation = fixture.store.join("staging-next");
                fs::create_dir_all(generation.join("target")).unwrap();
                fs::write(generation.join("target/artifact"), "new generation").unwrap();
                build_base::publish_record(&generation, &fixture.record).unwrap();
                let retired: Vec<_> = fs::read_dir(&fixture.store)
                    .unwrap()
                    .filter_map(|e| {
                        let e = e.unwrap();
                        e.file_name()
                            .to_string_lossy()
                            .starts_with("retired-")
                            .then(|| e.path())
                    })
                    .collect();
                assert_eq!(retired.len(), 1, "the old generation must remain leased");
                assert!(retired[0].join("target/artifact").exists());
            };
            let hook = |stage: &str| {
                if boundary == "validated" && stage == "validated" {
                    replace();
                }
            };
            let clone = |source: &Path, target: &Path| {
                let first = fs::read(source.join("artifact"))?;
                if boundary == "mid-clone" {
                    replace();
                }
                assert_eq!(fs::read(source.join("artifact"))?, first);
                clone_file(source, target)
            };
            let mut ops = fixture.ops();
            ops.hook = &hook;
            ops.clone = &clone;
            assert!(run(&fixture.attempt(), &ops).unwrap());
            assert_eq!(
                fs::read_to_string(fixture.workspace.join("target/artifact")).unwrap(),
                "old generation"
            );
            assert_eq!(
                fs::read_dir(&fixture.store)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<std::collections::BTreeSet<_>>(),
                ["current", "usage.json"]
                    .into_iter()
                    .map(std::ffi::OsString::from)
                    .collect(),
                "retired base deleted at lease end"
            );
        }
    }

    #[derive(Clone)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct SocketState {
        generation: Option<crate::remote_cache::provisioning::ActiveGeneration>,
        forwarders: Vec<crate::remote_cache::transport::Forwarder>,
        launched: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, String>>>,
    }

    impl crate::backend::IsolationLauncher for SocketState {
        fn cache_generation(
            &self,
        ) -> Option<(
            &crate::remote_cache::provisioning::ActiveGeneration,
            &[crate::remote_cache::transport::Forwarder],
        )> {
            self.generation
                .as_ref()
                .map(|generation| (generation, self.forwarders.as_slice()))
        }

        fn launch(
            &self,
            command: &str,
            cwd: &Path,
            env: &[(String, String)],
            _cpus: &[u32],
            _resources: crate::backend::JobResources,
            _sources: crate::source_seal::Sources<'_>,
        ) -> io::Result<crate::platform::Launch> {
            *self.launched.lock().unwrap() = env.iter().cloned().collect();
            crate::platform::launch(command, cwd, env)
        }
    }

    #[test]
    fn remote_socket_loss_after_seed_launches_cold_and_matching_environment_keeps_seed() {
        use crate::backend::{self, Backend, EnvironmentInputs, HostBackend, Prepared, Setup};
        use crate::remote_cache::{gateway::Tool, transport::Forwarder};
        use std::collections::BTreeMap;
        for sockets_survive in [false, true] {
            let mut fixture = Fixture::new();
            fs::write(fixture.workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
            fixture.record.tree = fixture.snapshot();
            let mut config = crate::Config::new(fixture.scratch.join("state"), "seed-test");
            config.isolate_jobs = true;
            config.tenant_vm = true;
            let trees = workspace::Trees {
                mirrors: config.state_dir().join("mirrors"),
                state: config.state_dir(),
                compacting: Default::default(),
                mirror_locks: Default::default(),
            };
            let id = motor_protocol::Uuid::now_v7();
            let cwd = fixture.workspace.to_string_lossy().into_owned();
            let setup = Setup {
                id,
                cpus: vec![0],
                memory_mib: None,
                base: false,
                recorded: BTreeMap::new(),
                snapshots: None,
                request: motor_protocol::Run {
                    job_id: id,
                    workspace_key: cwd.clone(),
                    repository_root: cwd.clone(),
                    cwd,
                    command: if sockets_survive {
                        "test -f target/artifact && printf kept > target/job-output".into()
                    } else {
                        "test -z \"$(ls -A target)\" && printf cold > target/job-output".into()
                    },
                    env: BTreeMap::new(),
                    weight_hint: None,
                    timeout: None,
                    queue_wait_limit_s: 300,
                    snapshot_commit: fixture.record.tree.clone(),
                    tree_hash: fixture.record.tree.clone(),
                    integration_tip: fixture.record.tree.clone(),
                    base_commit: Some("source".into()),
                    siblings: vec![],
                    bundles: vec![],
                    git_dependencies: vec![],
                    return_paths: vec![],
                    return_max_bytes: motor_protocol::RETURN_MAX_BYTES,
                    return_chunked: false,
                    gate_job: false,
                    network: None,
                    platform: None,
                    workspace_kind: None,
                },
            };
            let generation = crate::remote_cache::gateway::tests::generation("test", "tenant");
            let forwarders = vec![Forwarder::new(Tool::Sccache, 4181)];
            let target = fixture.workspace.join("target");
            let planned = backend::job_environment(
                &config,
                &trees,
                &setup,
                &EnvironmentInputs::before_setup(
                    &config,
                    &setup,
                    &fixture.workspace,
                    &target,
                    Some((&generation, &forwarders)),
                )
                .unwrap(),
                1,
            )
            .unwrap();
            fixture.record.inputs.environment =
                crate::compile_cache::environment_fingerprint(&planned);
            fixture.compatibility = Compatibility::Available(fixture.record.inputs.clone());
            fixture.save_record();
            assert!(run(&fixture.attempt(), &fixture.ops()).unwrap());
            let pin = pin_target(&fixture.workspace).unwrap();
            let launched = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()));
            // A prepared sandbox reports no generation if socket setup failed.
            // The portable launcher exercises dispatch without starting a VM.
            let prepared = Prepared {
                host_cwd: fixture.workspace.clone(),
                target,
                wrapper: None,
                vm: None,
                sandbox: Some(Box::new(SocketState {
                    generation: sockets_survive.then_some(generation),
                    forwarders,
                    launched: launched.clone(),
                })),
            };
            let actual = backend::job_environment(
                &config,
                &trees,
                &setup,
                &prepared.environment_inputs(),
                1,
            )
            .unwrap();
            let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = Log(bytes.clone());
            let kept = tracing::subscriber::with_default(
                motor_protocol::logging::LineSubscriber::new("[runner]", log),
                || confirm_environment(&pin, &planned, &actual, id).unwrap(),
            );
            assert_eq!(kept, sockets_survive);
            let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert_eq!(
                logs.matches("build base seed environment changed").count(),
                usize::from(!sockets_survive)
            );
            if !sockets_survive {
                assert!(logs.contains("RUSTC_WRAPPER"));
                assert!(
                    !logs.contains("/motor-tools/sccache"),
                    "only input names may be logged"
                );
            }
            let mut job = HostBackend
                .launch(&config, &trees, &setup, &prepared, 1)
                .unwrap();
            assert_eq!(*launched.lock().unwrap(), actual);
            job.process.take();
            job.execution.release().unwrap();
            let status = job.execution.wait().unwrap();
            assert!(status.success());
            assert_eq!(
                fs::read_to_string(fixture.workspace.join("target/job-output")).unwrap(),
                if sockets_survive { "kept" } else { "cold" }
            );
        }
    }

    #[test]
    fn post_prepare_discard_uses_held_directory_not_replacement_path() {
        let fixture = Fixture::new();
        assert!(run(&fixture.attempt(), &fixture.ops()).unwrap());
        let pin = pin_target(&fixture.workspace).unwrap();
        let detached = fixture.workspace.join("detached");
        fs::rename(fixture.workspace.join("target"), &detached).unwrap();
        fs::create_dir(fixture.workspace.join("target")).unwrap();
        fs::write(fixture.workspace.join("target/replacement"), "untouched").unwrap();
        let planned = [("RUSTC_WRAPPER".into(), "old".into())].into();
        assert!(!confirm_environment(
            &pin,
            &planned,
            &Default::default(),
            motor_protocol::Uuid::now_v7()
        )
        .unwrap());
        assert!(fs::read_dir(detached).unwrap().next().is_none());
        assert_eq!(
            fs::read_to_string(fixture.workspace.join("target/replacement")).unwrap(),
            "untouched"
        );
    }

    #[test]
    fn failures_settle_once_warn_once_and_job_builds_without_seed() {
        for case in [
            "exdev",
            "unsupported",
            "partial",
            "removed",
            "ownership",
            "times",
            "future",
            "identity",
        ] {
            let mut fixture = Fixture::new();
            if case == "identity" {
                fixture.compatibility = Compatibility::Unavailable("unreadable compiler".into());
            }
            if case == "future" {
                fixture.record.newest_mtime.seconds = i64::MAX;
                fixture.save_record();
            }
            let calls = Cell::new(0);
            let clone = |source: &Path, target: &Path| {
                calls.set(calls.get() + 1);
                match case {
                    "exdev" => Err(io::Error::from_raw_os_error(libc::EXDEV)),
                    "unsupported" => Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
                    "partial" => {
                        fs::create_dir_all(target)?;
                        fs::write(target.join("partial"), "partial")?;
                        Err(io::Error::other("partial clone"))
                    }
                    "removed" => {
                        fs::remove_dir_all(fixture.store.join("current"))?;
                        clone_file(source, target)
                    }
                    _ => clone_file(source, target),
                }
            };
            let repair = |_: &Path| {
                if case == "ownership" {
                    Err(io::Error::other("ownership repair failed"))
                } else {
                    Ok(())
                }
            };
            let times = |record: &Record| {
                if case == "times" {
                    Err(io::Error::other("time step failed"))
                } else {
                    crate::seed_times::apply(
                        &fixture.workspace,
                        &fixture.workspace,
                        &fixture.record.tree,
                        record,
                    )
                    .map(|_| ())
                }
            };
            let mut ops = fixture.ops();
            ops.clone = &clone;
            ops.repair = &repair;
            ops.times = &times;
            let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = Log(bytes.clone());
            let subscriber = motor_protocol::logging::LineSubscriber::new("[runner]", log);
            tracing::subscriber::with_default(subscriber, || {
                assert!(
                    !execute(&fixture.attempt(), &ops, motor_protocol::Uuid::now_v7()).unwrap(),
                    "{case}"
                );
            });
            fixture.assert_settled();
            let logs = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
            assert_eq!(
                logs.matches("build base seed failed").count(),
                1,
                "{case}: {logs}"
            );
            assert_eq!(
                calls.get(),
                usize::from(case != "identity"),
                "no clone retry or copy fallback"
            );
            assert!(std::process::Command::new("sh")
                .args(["-c", "echo built > target/job-output"])
                .current_dir(&fixture.workspace)
                .status()
                .unwrap()
                .success());
            assert_eq!(
                fs::read_to_string(fixture.workspace.join("target/job-output")).unwrap(),
                "built\n"
            );
        }
    }

    #[test]
    fn tracked_collision_and_unrelated_ignored_seed_names_survive_settlement() {
        let fixture = Fixture::new();
        let mut ops = fixture.ops();
        ops.staging = ".motor-seed-a".into();
        assert!(!execute(&fixture.attempt(), &ops, motor_protocol::Uuid::now_v7()).unwrap());
        assert!(!intent_path(&fixture.scratch, &fixture.workspace).exists());
        settle(&fixture.scratch, &fixture.workspace).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(".motor-seed-a/tracked")).unwrap(),
            "keep"
        );
        fs::create_dir(fixture.workspace.join(".motor-seed-b")).unwrap();
        fs::write(
            fixture.workspace.join(".motor-seed-b/ignored"),
            "keep ignored",
        )
        .unwrap();
        assert!(!run(&fixture.attempt(), &fixture.ops()).unwrap());
        settle(&fixture.scratch, &fixture.workspace).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(".motor-seed-b/ignored")).unwrap(),
            "keep ignored"
        );
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(".motor-seed-a/tracked")).unwrap(),
            "keep"
        );
    }

    #[test]
    fn tracked_and_retained_ignored_seed_name_files_are_not_swept() {
        let mut fixture = Fixture::new();
        fs::remove_dir_all(fixture.workspace.join(".motor-seed-a")).unwrap();
        fs::write(fixture.workspace.join(".motor-seed-a"), "tracked file").unwrap();
        fixture.record.tree = fixture.snapshot();
        fixture.save_record();
        fs::write(fixture.workspace.join(".motor-seed-b"), "ignored file").unwrap();
        assert!(!run(&fixture.attempt(), &fixture.ops()).unwrap());
        settle(&fixture.scratch, &fixture.workspace).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(".motor-seed-a")).unwrap(),
            "tracked file"
        );
        assert_eq!(
            fs::read_to_string(fixture.workspace.join(".motor-seed-b")).unwrap(),
            "ignored file"
        );
    }

    #[test]
    fn recorded_intent_never_cleans_replacement_directories_or_links() {
        let fixture = Fixture::new();
        let hook = |stage: &str| {
            if stage == "swapped" {
                panic!("stop after exchange");
            }
        };
        let mut ops = fixture.ops();
        ops.hook = &hook;
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(
                &fixture.attempt(),
                &ops
            )))
            .is_err()
        );
        fs::rename(
            fixture.workspace.join("target"),
            fixture.scratch.join("detached-clone"),
        )
        .unwrap();
        fs::rename(
            fixture.workspace.join(".motor-seed-test"),
            fixture.scratch.join("detached-empty"),
        )
        .unwrap();
        fs::create_dir(fixture.workspace.join("target")).unwrap();
        fs::write(fixture.workspace.join("target/job-owned"), "leave alone").unwrap();
        symlink(
            fixture.scratch.join("detached-clone"),
            fixture.workspace.join(".motor-seed-test"),
        )
        .unwrap();
        settle(&fixture.scratch, &fixture.workspace).unwrap();
        assert_eq!(
            fs::read_to_string(fixture.workspace.join("target/job-owned")).unwrap(),
            "leave alone"
        );
        assert!(fixture
            .workspace
            .join(".motor-seed-test")
            .symlink_metadata()
            .unwrap()
            .is_symlink());
        assert!(fixture.scratch.join("detached-clone/artifact").exists());
        assert!(!intent_path(&fixture.scratch, &fixture.workspace).exists());
    }

    fn kill() -> ! {
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
        }
        unreachable!()
    }

    #[test]
    fn crash_child() {
        let Some(workspace) = std::env::var_os("MOTOR_SEED_CRASH_WORKSPACE") else {
            return;
        };
        let workspace = PathBuf::from(workspace);
        let state = workspace.parent().unwrap();
        let boundary = std::env::var("MOTOR_SEED_CRASH_BOUNDARY").unwrap();
        let store = state.join("store");
        let record: Record =
            serde_json::from_slice(&fs::read(store.join("current/base.json")).unwrap()).unwrap();
        let compatibility = Compatibility::Available(record.inputs.clone());
        let clone = |source: &Path, target: &Path| {
            clone_file(source, target)?;
            if boundary == "mid-clone" {
                kill();
            }
            Ok(())
        };
        let times = |_: &Record| {
            // Simulate an interrupted time traversal after one source was set,
            // before the remaining files and directories can be time-set.
            let file = File::open(workspace.join("source"))?;
            file.set_modified(std::time::UNIX_EPOCH)?;
            if boundary == "mid-times" {
                kill();
            }
            Ok(())
        };
        run(
            &Attempt {
                state,
                workspace: &workspace,
                store: &store,
                mirror: &workspace,
                tree: &record.tree,
                fixed: &record.fixed_path,
                compatibility: &compatibility,
            },
            &Operations {
                clone: &clone,
                repair: &|_| Ok(()),
                times: &times,
                hook: &|stage| {
                    if stage == boundary {
                        kill();
                    }
                },
                staging: ".motor-seed-test".into(),
            },
        )
        .unwrap();
        panic!("crash boundary was not reached");
    }

    #[test]
    fn killed_setups_recover_and_committed_seed_remains_kept() {
        use std::os::unix::process::ExitStatusExt;
        for boundary in [
            "intent",
            "mkdir",
            "mid-clone",
            "swapped",
            "mid-times",
            "committed",
        ] {
            let fixture = Fixture::new();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "seed::tests::crash_child", "--nocapture"])
                .env("MOTOR_SEED_CRASH_WORKSPACE", &fixture.workspace)
                .env("MOTOR_SEED_CRASH_BOUNDARY", boundary)
                .status()
                .unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL), "{boundary}");
            settle(&fixture.scratch, &fixture.workspace).unwrap();
            if boundary == "committed" {
                let mut ops = fixture.ops();
                ops.times = &|_| panic!("kept folder must not be time-set");
                assert!(!run(&fixture.attempt(), &ops).unwrap());
                assert!(fixture.workspace.join("target/artifact").exists());
            } else if boundary == "mkdir" {
                assert!(children(&root(&fixture.workspace.join("target")).unwrap())
                    .unwrap()
                    .is_empty());
                assert!(
                    fixture.workspace.join(".motor-seed-test").is_dir(),
                    "unrecorded directory has no cleanup authority"
                );
                assert!(!intent_path(&fixture.scratch, &fixture.workspace).exists());
            } else {
                fixture.assert_settled();
            }
            assert_eq!(
                fs::read_to_string(fixture.workspace.join(".motor-seed-a/tracked")).unwrap(),
                "keep"
            );
        }
    }
}
