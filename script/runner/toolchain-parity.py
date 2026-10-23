#!/usr/bin/env python3
"""Read-only inventory of a runner's job-visible toolchain."""

import argparse
import fnmatch
import hashlib
import importlib.util
import json
import re
import sys
import os
from pathlib import Path
import shutil
import subprocess

HERE = Path(__file__).resolve().parent
MANIFEST = HERE / "toolchain.json"
LOCAL_BIN = Path("/usr/local/bin")
# A tenant VM binds the tenant's own directories over these image paths before
# its runner starts, so no tool the tenant runner executes may live beneath them.
TENANT_WORKSPACE_ROOTS = ("/home", "/Users")
HOST_ONLY_LOCAL_BIN_PATTERNS = ("motor-host*", "firecracker-*", "jailer-*")
if 'seed' not in globals():
    seed_spec = importlib.util.spec_from_file_location('cargo_seed', HERE / 'cargo-seed.py')
    if seed_spec is None or seed_spec.loader is None:
        raise ImportError('cannot load Cargo seed inventory')
    seed = importlib.util.module_from_spec(seed_spec)
    seed_spec.loader.exec_module(seed)
if 'syscall_surface' not in globals():
    syscall_spec = importlib.util.spec_from_file_location('syscall_surface', HERE / 'syscall-surface.py')
    if syscall_spec is None or syscall_spec.loader is None:
        raise ImportError('cannot load syscall surface probe')
    syscall_surface = importlib.util.module_from_spec(syscall_spec)
    syscall_spec.loader.exec_module(syscall_surface)


def output(*args, env=None):
    result = subprocess.run(args, env=env, text=True, capture_output=True, check=True, timeout=30)
    return result.stdout.strip()


def optional_text(path):
    try:
        return Path(path).read_text().strip()
    except (FileNotFoundError, PermissionError):
        return None


def local_bin_entries(directory=LOCAL_BIN):
    try:
        return sorted(path.name for path in directory.iterdir()
                      if path.is_file() and os.access(path, os.X_OK))
    except FileNotFoundError:
        return []


def host_only_local_bin(name):
    return any(fnmatch.fnmatchcase(name, pattern) for pattern in HOST_ONLY_LOCAL_BIN_PATTERNS)


def binary_version(path, env):
    # sccache may contact a long-lived daemon even for informational commands.
    # Inventory must never start or depend on a cache server.
    if Path(path).name.startswith("sccache"):
        return {"version": None, "error": "not-probed: sccache"}
    if Path(path).name == "gnutest":
        return {"version": None, "error": "not-probed: test has no version flag"}
    try:
        name = Path(path).name
        # true and false recognize --version only under their own names.
        argv0 = name[3:] if name.startswith("gnu") else str(path)
        result = subprocess.run([argv0, "--version"], executable=str(path), env=env,
                                text=True, capture_output=True, timeout=30)
        return {"version": (result.stdout + result.stderr).strip(), "exit_code": result.returncode}
    except subprocess.TimeoutExpired:
        return {"version": None, "error": "timeout"}
    except OSError as error:
        return {"version": None, "error": str(error)}


def tool_version(path, env, observed, version_from=None):
    if version_from:
        source = observed.get(version_from) or {}
        return {"version": source.get("version"), "exit_code": source.get("exit_code", 1)}
    return binary_version(path, env)


def role_manifest(manifest, role, job=False):
    """Select the role's toolchain roots; jobs keep registry caches in /motor-home/cargo."""
    return {**manifest, **manifest.get('layout_by_role', {}).get(role, {}).get('job' if job else 'host', {})}


def runtime_capture():
    import tempfile

    runtime = {'proc': {}, 'inotify': {}, 'tmpdir': tempfile.gettempdir()}
    for name in ('self/mountinfo', 'cpuinfo', 'meminfo'):
        try:
            runtime['proc'][name] = bool(Path('/proc', name).read_text())
        except OSError:
            runtime['proc'][name] = False
    for name in ('max_user_instances', 'max_user_watches', 'max_queued_events'):
        value = optional_text('/proc/sys/fs/inotify/' + name)
        runtime['inotify'][name] = int(value) if value else None
    try:
        with tempfile.TemporaryFile(dir='/dev/shm') as shared:
            shared.write(b'shared memory probe')
        runtime['dev_shm_writable'] = True
    except OSError:
        runtime['dev_shm_writable'] = False
    try:
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / 'exec-probe'
            executable.write_text('#!/bin/sh\nexit 0\n')
            executable.chmod(0o700)
            runtime['tmpdir_exec'] = subprocess.run([str(executable)], timeout=10).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        runtime['tmpdir_exec'] = False
    runtime['sshd_executable'] = os.access('/usr/sbin/sshd', os.X_OK)
    runtime['api_mounts'] = [line for line in (optional_text('/proc/self/mountinfo') or '').splitlines()
                             if line.split()[4] in ('/proc', '/dev/shm')]
    try:
        runtime['visible_pids'] = len([path for path in Path('/proc').iterdir() if path.name.isdigit()])
    except OSError:
        runtime['visible_pids'] = None
    return runtime


def job_identity(job):
    if job:
        return {'uid': os.getuid(), 'gid': os.getgid()}
    import pwd
    try:
        account = pwd.getpwnam('motor-job')
        return {'uid': account.pw_uid, 'gid': account.pw_gid}
    except KeyError:
        return None


def macos_parity():
    spec = importlib.util.spec_from_file_location('macos_parity', HERE.parent / 'macos-runner/parity.py')
    if spec is None or spec.loader is None:
        raise ImportError('cannot load macOS toolchain inventory')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def capture(manifest=None, job=False, role="build-server"):
    if role == "macos":
        return macos_parity().capture(manifest, job)
    paths = manifest or {}
    cargo_home = Path(paths.get("cargo_home", os.environ.get("CARGO_HOME", "/srv/build/cargo-home")))
    rustup_home = Path(paths.get("rustup_home", os.environ.get("RUSTUP_HOME", "/home/ubuntu/.rustup")))
    env = dict(os.environ, CARGO_HOME=str(cargo_home), RUSTUP_HOME=str(rustup_home),
               RUSTUP_AUTO_INSTALL="0", RUSTUP_NO_UPDATE_CHECK="1")
    if not job:
        bun_dir = Path(tool_path(paths.get("tools", {}).get("bun", {}), role, job)
                       or "/home/ubuntu/.bun/bin/bun").parent
        env["PATH"] = str(cargo_home / "bin") + f":{bun_dir}:" + os.environ.get("PATH", os.defpath)
    rustup = str(cargo_home / "bin/rustup")
    toolchains = {}
    target_payloads = {}
    for line in output(rustup, "toolchain", "list", env=env).splitlines():
        name = line.split()[0]
        toolchains[name] = {
            "rustc": output(str(rustup_home / "toolchains" / name / "bin/rustc"), "--version", env=env),
            "targets": sorted(output(rustup, "target", "list", "--installed", "--toolchain", name, env=env).splitlines()),
            "components": sorted(output(rustup, "component", "list", "--installed", "--toolchain", name, env=env).splitlines()),
        }
        target_payloads[name] = {
            target: any(path.is_file() and path.stat().st_size > 0 for path in
                        (rustup_home / "toolchains" / name / "lib/rustlib" / target / "lib").glob("libstd-*.rlib"))
            for target in toolchains[name]["targets"]}
    binaries = {}
    for path in sorted((cargo_home / "bin").iterdir()):
        if path.is_file() and os.access(path, os.X_OK) and not path.name.startswith("."):
            binaries[path.name] = {"symlink": os.readlink(path) if path.is_symlink() else None}
            if not os.path.samefile(path, cargo_home / "bin/rustup"):
                binaries[path.name].update(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                                           **binary_version(path, env))
    metadata = {}
    for root in ([] if job else [cargo_home, *sorted(Path("/srv/build/tools").glob("*"))]):
        path = root / ".crates2.json"
        if path.is_file():
            metadata[str(root)] = json.loads(path.read_text())
    tools = {}
    names = {"node", "npm", "bun", "pnpm", "nats-server", "wasm-opt", "esbuild", "git",
             "x86_64-w64-mingw32-gcc", "x86_64-w64-mingw32-g++"}
    names.update(paths.get("tools", {}))
    names.update(paths.get("gnu_coreutils", {}))
    for directory in env["PATH"].split(":"):
        if directory:
            names.update(path.name for path in Path(directory).glob("gnu*") if path.is_file())
    for name in sorted(names):
        path = shutil.which(name, path=env["PATH"])
        if path is None and name in paths.get("gnu_coreutils", {}):
            candidate = paths["gnu_coreutils"][name]
            path = candidate if os.access(candidate, os.X_OK) else None
        if path:
            owner = "" if job else subprocess.run(
                ["dpkg-query", "-S", str(Path(path).resolve())], text=True,
                capture_output=True, timeout=30).stdout.strip()
            version = tool_version(path, env, tools,
                                   paths.get("tools", {}).get(name, {}).get("version_from"))
            tools[name] = {"path": path, "realpath": str(Path(path).resolve()),
                           **version, "package": owner}
            if name == 'gnutest':
                tools[name]['sha256'] = hashlib.sha256(Path(path).read_bytes()).hexdigest()
        else:
            tools[name] = None
    packages = {}
    for line in ([] if job else output("dpkg-query", "-W", "-f=${Package}\t${Version}\t${db:Status-Status}\\n").splitlines()):
        name, version, status = line.split("\t")
        if status == "installed":
            packages[name] = version
    job_names = set(paths.get('job_environment', {}))
    for role_environment in paths.get('job_environment_by_role', {}).values():
        job_names.update(role_environment)
    mountinfo = Path('/proc/self/mountinfo').read_text() if job else ''
    wrapper = {}
    if job:
        contract = paths['compile_wrapper']
        resolved = shutil.which(os.environ.get('RUSTC_WRAPPER', ''), path=env['PATH'])
        cache_path = shutil.which('sccache', path=env['PATH'])
        wrapper = {
            'resolved': resolved,
            'supervisor_cache': optional_text(contract['supervisor_cache_file']),
            'sccache_path': cache_path,
            'sccache_sha256': hashlib.sha256(Path(cache_path).read_bytes()).hexdigest() if cache_path else None,
            'backend': optional_text(contract['sccache_path'] + '.backend'),
            'wrapper_readonly': readonly_mount(contract['sccache_path'], mountinfo),
            'backend_readonly': readonly_mount(contract['backend'], mountinfo),
        }
    return {
        "schema": 1,
        "os_release": "" if job else Path("/etc/os-release").read_text(),
        "rustup_version": output(rustup, "--version", env=env),
        "rust_default": output(rustup, "default", env=env).split()[0],
        "rust_profile": output(rustup, "show", "profile", env=env),
        "rust_toolchains": toolchains,
        "rust_target_payloads": target_payloads,
        "cargo_home": str(cargo_home), "rustup_home": str(rustup_home),
        "cargo_binaries": binaries, "cargo_metadata": metadata,
        "tools": tools, "apt_packages": packages,
        "local_bin_entries": [] if job else local_bin_entries(),
        "environment": {name: env.get(name) for name in (
            "PATH", "CC", "CXX", "WASM_BINDGEN_BIN", "WASM_OPT_BIN", "ESBUILD_BIN",
            "RUSTFLAGS", "CARGO_BUILD_TARGET", "PKG_CONFIG_PATH")},
        "job_environment": {name: os.environ.get(name) for name in sorted(job_names)},
        "compile_wrapper": wrapper,
        "mountinfo": mountinfo,
        "runtime": runtime_capture() if job else {},
        "syscalls": syscall_surface.capture(),
        "job_identity": job_identity(job),
        "cargo_registry": seed.capture(os.environ.get('CARGO_HOME', cargo_home) if job else cargo_home, strict=False),
    }


def tool_path(spec, role, job=False):
    """Where `role` puts a manifest tool: jobs see `job_path` when the tool has
    one (Bun, which the runner mounts there), otherwise both hosts and jobs use
    the role's `path_by_role` entry or the measured build-server `path`."""
    if job and "job_path" in spec:
        return spec["job_path"]
    return spec.get("path_by_role", {}).get(role, spec.get("path"))


def first_line(text):
    return text.splitlines()[0] if text else ""


def validate(manifest, role="build-server"):
    if role == "macos":
        macos_parity().validate(manifest, json.loads(MANIFEST.read_text()))
        return
    if manifest["schema"] != 1 or manifest["rust_default"] not in manifest["rust_toolchains"]:
        raise ValueError("invalid parity schema or default toolchain")
    if (set(manifest.get('job_identity', {})) != {'uid', 'gid'}
            or any(type(value) is not int or value <= 0 for value in manifest['job_identity'].values())):
        raise ValueError('invalid numeric job identity')
    for key in ("rustup",):
        if not re.fullmatch(r"\d+\.\d+\.\d+", manifest[key]):
            raise ValueError("unpinned version: " + key)
    for name, spec in manifest["rust_toolchains"].items():
        if not re.fullmatch(r"(?:\d+\.\d+\.\d+|stable)-x86_64-unknown-linux-gnu", name):
            raise ValueError("unpinned Rust toolchain: " + name)
        if not re.fullmatch(r"rustc \d+\.\d+\.\d+ \([a-f0-9]+ \d{4}-\d{2}-\d{2}\)", spec["rustc"]):
            raise ValueError("unpinned rustc: " + name)
        if not spec["targets"] or spec["targets"] != sorted(set(spec["targets"])):
            raise ValueError("invalid Rust targets: " + name)
        std = sorted(component.removeprefix("rust-std-") for component in spec["components"] if component.startswith("rust-std-"))
        if std != spec["targets"]:
            raise ValueError("Rust targets and components disagree: " + name)
    for name, pattern in {"node": r"v\d+\.\d+\.\d+", "node22": r"v\d+\.\d+\.\d+",
                          "npm": r"\d+\.\d+\.\d+", "npx": r"\d+\.\d+\.\d+",
                          "corepack": r"\d+\.\d+\.\d+", "bun": r"\d+\.\d+\.\d+",
                          "bun-1.3.14": r"\d+\.\d+\.\d+", "pnpm": r"\d+\.\d+\.\d+",
                          "pnpx": r"\d+\.\d+\.\d+", "pnx": r"\d+\.\d+\.\d+",
                          "nats-server": r"nats-server: v\d+\.\d+\.\d+"}.items():
        if name in manifest["tools"] and not re.fullmatch(pattern, manifest["tools"][name]["version"]):
            raise ValueError("unpinned version: " + name)
    archive_urls = {
        "bun-1.3.14": "https://github.com/oven-sh/bun/releases/download/bun-v1.3.14/bun-linux-x64.zip",
        "node22": "https://nodejs.org/dist/v22.22.1/node-v22.22.1-linux-x64.tar.xz",
    }
    archive_layouts = {
        "bun-1.3.14": ("1.3.14", "zip", "/usr/local/lib/bun/1.3.14", "bin/bun",
                        "/usr/local/bin/bun-1.3.14"),
        "node22": ("v22.22.1", "tar.xz", "/usr/local/lib/nodejs/22.22.1", "bin/node",
                   "/usr/local/bin/node22"),
    }
    for name, url in archive_urls.items():
        if name not in manifest["tools"]:
            continue
        spec = manifest["tools"][name]
        archive = spec.get("archive", {})
        if archive.get("url") != url or not re.fullmatch(r"[a-f0-9]{64}", archive.get("sha256", "")):
            raise ValueError("invalid pinned archive: " + name)
        version, archive_format, install_dir, install_path, link = archive_layouts[name]
        if (spec["version"] != version or archive.get("format") != archive_format
                or archive.get("install_dir") != install_dir or archive.get("install_path") != install_path
                or spec["path"] != link):
            raise ValueError("invalid archive install layout: " + name)
        if name == "bun-1.3.14" and archive.get("archive_path") != "bun-linux-x64/bun":
            raise ValueError("invalid Bun archive binary: " + name)
        if name == "node22" and archive.get("strip_components") != 1:
            raise ValueError("invalid Node archive root: " + name)
    wasm_opt = manifest["tools"].get("wasm-opt")
    if wasm_opt is not None:
        match = re.fullmatch(r"wasm-opt version (\d+)", wasm_opt.get("version", ""))
        if not match:
            raise ValueError("unpinned version: wasm-opt")
        release = match.group(1)
        source = wasm_opt.get("source", {})
        url = (f"https://github.com/WebAssembly/binaryen/releases/download/version_{release}/"
               f"binaryen-version_{release}-x86_64-linux.tar.gz")
        expected_paths = {"boat": "/usr/local/bin/wasm-opt", "tenant": "/usr/local/bin/wasm-opt"}
        expected_realpaths = {role: f"/usr/local/lib/binaryen/{release}/bin/wasm-opt"
                              for role in ("boat", "tenant")}
        if (source.get("url") != url or not re.fullmatch(r"[a-f0-9]{64}", source.get("sha256", ""))
                or source.get("archive_root") != f"binaryen-version_{release}"
                or source.get("install_dir") != f"/usr/local/lib/binaryen/{release}"
                or wasm_opt.get("path") != "/usr/bin/wasm-opt"
                or wasm_opt.get("path_by_role") != expected_paths
                or wasm_opt.get("realpath_suffix_by_role") != expected_realpaths):
            raise ValueError("invalid pinned Binaryen archive or install layout")
    git = manifest["tools"].get("git")
    if git is not None:
        validate_git(git)
    bun = manifest["tools"].get("bun")
    if bun is not None:
        validate_bun(bun, manifest)
    for name, spec in manifest["tools"].items():
        source = spec.get("version_from")
        if source and (source not in manifest["tools"]
                       or manifest["tools"][source].get("version") != spec.get("version")):
            raise ValueError("invalid version source: " + name)
    defaults = set()
    for tool in manifest["cargo_installs"]:
        if "git" in tool:
            if not re.fullmatch(r"[0-9a-f]{40}", tool["rev"]):
                raise ValueError("git tools need a full revision")
            if tool.get("default"):
                if tool["crate"] in defaults:
                    raise ValueError("duplicate default Cargo tool")
                defaults.add(tool["crate"])
        elif not re.fullmatch(r"\d+\.\d+\.\d+", tool["version"]):
            raise ValueError("unpinned Cargo tool")
        for name in tool["bins"]:
            binary = name + "-" + tool["rev"][:8] if "git" in tool else name
            expected = manifest["cargo_binaries"][binary]
            if "git" not in tool and tool["version"] not in expected.get("version", "").split():
                raise ValueError("Cargo install and binary versions disagree: " + name)
    if not re.fullmatch(r'[a-f0-9]{64}', manifest['compile_wrapper']['sccache_sha256']):
        raise ValueError('invalid trusted compiler cache wrapper hash')


def validate_git(spec):
    """Boat and tenant images build the server's git release from source. Tie
    the source URL, install directory and image paths to the measured version,
    so refreshing the server's version without new source pins fails here."""
    match = re.fullmatch(r"git version (\d+\.\d+\.\d+)", spec.get("version", ""))
    if not match:
        raise ValueError("unpinned version: git")
    release = match.group(1)
    source = spec.get("source", {})
    install_dir = "/usr/local/lib/git/" + release
    if (source.get("url") != f"https://www.kernel.org/pub/software/scm/git/git-{release}.tar.xz"
            or not re.fullmatch(r"[a-f0-9]{64}", source.get("sha256", ""))):
        raise ValueError("invalid pinned git source: " + release)
    if (source.get("install_dir") != install_dir
            or spec.get("path_by_role") != {role: "/usr/local/bin/git" for role in ("boat", "tenant")}
            or any(spec.get("realpath_suffix_by_role", {}).get(role) != install_dir + "/bin/git"
                   for role in ("boat", "tenant"))):
        raise ValueError("invalid git install layout: " + release)
    packages = source.get("build_packages")
    if not packages or not all(re.fullmatch(r"[a-z0-9][a-z0-9+.-]+", name) for name in packages):
        raise ValueError("invalid git build packages")


def validate_bun(spec, manifest):
    """The runner runs Bun from its role's manifest path and mounts that
    directory into jobs at job_path, which every role's job PATH searches.
    The tenant's path must avoid the directories the tenant agent binds over."""
    job_dir = str(Path(spec.get("job_path", "")).parent)
    for role, environment in manifest["job_environment_by_role"].items():
        if job_dir not in environment["PATH"].split(":"):
            raise ValueError(f"Bun's job directory {job_dir} is not on the {role} job PATH")
    if spec["path"] != spec.get("job_path") or set(spec.get("path_by_role", {})) - {"boat", "tenant"}:
        raise ValueError("invalid Bun install layout")
    tenant = tool_path(spec, "tenant")
    if any(tenant == root or tenant.startswith(root + "/") for root in TENANT_WORKSPACE_ROOTS):
        raise ValueError(f"tenant Bun at {tenant} would be hidden by the tenant's bound workspace roots")


def readonly_mount(path, mountinfo):
    matches = [line.split() for line in mountinfo.splitlines()
               if path == line.split()[4] or path.startswith(line.split()[4].rstrip("/") + "/")]
    return bool(matches) and "ro" in max(matches, key=lambda row: len(row[4]))[5].split(",")


def differences(manifest, actual, role="build-server", job=False, gnu_test_sha256=None):
    if role == "macos":
        return macos_parity().differences(manifest, actual, job)
    manifest = role_manifest(manifest, role, job)
    errors = []

    def equal(name, expected, observed):
        if expected != observed:
            errors.append(f"{name}: expected {expected!r}, observed {observed!r}")

    equal('motor-job identity', manifest['job_identity'], actual.get('job_identity'))
    errors.extend(syscall_surface.differences(actual.get('syscalls', {}), job=job))

    for key in ("rust_default", "rust_profile", "cargo_home", "rustup_home"):
        equal(key, manifest[key], actual.get(key))
    chains = actual.get("rust_toolchains", {})
    equal("Rust toolchain names", sorted(manifest["rust_toolchains"]), sorted(chains))
    for name, spec in manifest["rust_toolchains"].items():
        for key in ("rustc", "targets", "components"):
            equal(name + " " + key, spec[key], chains.get(name, {}).get(key))
        # rustup's receipts can survive a deleted or incomplete target directory.
        for target in spec['targets']:
            if actual.get('rust_target_payloads', {}).get(name, {}).get(target) is not True:
                errors.append(name + ': missing libstd payload for ' + target)
    version = first_line(actual.get("rustup_version", "")).split()
    equal("rustup", manifest["rustup"], version[1] if len(version) > 1 else None)
    expected_bins, bins = manifest["cargo_binaries"], actual.get("cargo_binaries", {})
    equal("cargo binary names", sorted(expected_bins), sorted(bins))
    for name, expected in expected_bins.items():
        observed = bins.get(name, {})
        if expected.get("proxy"):
            equal(name + " rustup proxy", "rustup", observed.get("symlink"))
        elif "version" in expected:
            equal(name + " version", expected["version"], first_line(observed.get("version")))
            equal(name + " --version exit", 0, observed.get("exit_code"))
        if "matches" in expected:
            other = bins.get(expected["matches"], {})
            if not observed.get("sha256") or observed.get("sha256") != other.get("sha256"):
                errors.append(name + ": does not match " + expected["matches"])
    if not bins.get("sccache", {}).get("sha256") or bins.get("sccache", {}).get("sha256") == bins.get("sccache-0.18.0-stock", {}).get("sha256"):
        errors.append("sccache: missing patched binary or identical to stock")
    if role == "build-server":
        for name, expected in manifest["sccache"]["build_server_sha256"].items():
            equal(name + " SHA-256", expected, bins.get(name, {}).get("sha256"))
    tools = actual.get("tools", {})
    for name, expected in manifest["tools"].items():
        observed = tools.get(name) or {}
        # A tool the image installs beside a distribution copy, like git,
        # resolves from another directory there; its version is still exact.
        equal(name + " path", tool_path(expected, role, job), observed.get("path"))
        equal(name + " --version exit", 0, observed.get("exit_code"))
        version = first_line(observed.get("version"))
        if name == "wasm-opt":
            expected_major = re.fullmatch(r"wasm-opt version (\d+)", expected.get("version", ""))
            actual_major = re.match(r"wasm-opt version (\d+)(?:\s|$)", version)
            if (not expected_major or not actual_major
                    or expected_major.group(1) != actual_major.group(1)):
                expected_value = expected_major.group(1) if expected_major else None
                observed_value = actual_major.group(1) if actual_major else version
                errors.append(f"wasm-opt major version: expected {expected_value!r}, observed {observed_value!r}")
        elif "version" in expected:
            equal(name + " version", expected["version"], version)
        elif not re.fullmatch(expected["version_pattern"], version):
            errors.append(name + ": unexpected distro tool version " + repr(version))
        realpath_suffix = expected.get("realpath_suffix_by_role", {}).get(role, expected.get("realpath_suffix"))
        if realpath_suffix and not observed.get("realpath", "").endswith(realpath_suffix):
            errors.append(name + ": wrong resolved executable path")
    if role == "build-server" and not job:
        catalogued = {name for name, spec in manifest["tools"].items()
                      if spec["path"].startswith(str(LOCAL_BIN) + "/")}
        uncatalogued = sorted(name for name in actual.get("local_bin_entries", [])
                              if name not in catalogued and not host_only_local_bin(name))
        if uncatalogued:
            errors.append("uncatalogued job-visible /usr/local/bin entries: " + ", ".join(uncatalogued))
    for name in manifest["gnu_coreutils"]:
        observed = tools.get(name) or {}
        equal(name + " path", manifest["gnu_coreutils"][name], observed.get("path"))
        packages = {owner.split(':', 1)[0] for line in observed.get('package', '').splitlines()
                    for owner in line.partition(': ')[0].split(', ') if owner}
        owned = bool(packages & {'coreutils', 'gnu-coreutils'})
        family = bool(re.fullmatch(r'.*\((?:GNU )?coreutils\) [0-9]+(?:\.[0-9]+)+.*',
                                  first_line(observed.get('version'))))
        if name == 'gnutest' and job:
            # Job filesystems omit dpkg's database. Compare gnutest's hash with
            # the fresh host hash after checking that host file's package owner.
            # test has no --version option.
            identified = bool(gnu_test_sha256) and observed.get('sha256') == gnu_test_sha256
        else:
            identified = owned or (name != 'gnutest' and not packages and family)
        if not observed.get("path") or not identified:
            errors.append(name + ": missing GNU coreutils alias")
    if job:
        environment = {**manifest['job_environment'], **manifest['job_environment_by_role'][role]}
        for name, expected in environment.items():
            equal("job " + name, expected, actual.get("job_environment", {}).get(name))
        runtime = actual.get('runtime', {})
        for name in ('self/mountinfo', 'cpuinfo', 'meminfo'):
            equal('/proc/' + name + ' readable', True, runtime.get('proc', {}).get(name))
        for name in ('dev_shm_writable', 'tmpdir_exec', 'sshd_executable'):
            equal('job ' + name, True, runtime.get(name))
        for name, minimum in manifest.get('inotify_minimum', {}).items():
            value = runtime.get('inotify', {}).get(name)
            if not isinstance(value, int) or value < minimum:
                errors.append(f'inotify {name}: expected at least {minimum}, observed {value!r}')
        # Only build-server jobs read the shared compile cache through the
        # runner's wrapper. Boat jobs compile directly: no cache is filled there,
        # and an empty read-only cache breaks C builds that cc-rs routes to it.
        if role == 'build-server':
            contract, wrapper = manifest['compile_wrapper'], actual.get('compile_wrapper', {})
            equal('compiler wrapper resolution', environment['RUSTC_WRAPPER'], wrapper.get('resolved'))
            equal('compiler supervisor cache', contract['sccache_path'], wrapper.get('supervisor_cache'))
            for name in ('sccache_path', 'sccache_sha256', 'backend'):
                equal('compiler cache ' + name, contract[name], wrapper.get(name))
            for name in ('wrapper_readonly', 'backend_readonly'):
                equal('compiler cache ' + name, True, wrapper.get(name))
        if not readonly_mount(manifest["rustup_home"], actual.get("mountinfo", "")):
            errors.append("job rustup home is not mounted read-only")
    else:
        required = manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"][role]
        for name in required:
            if name not in actual.get("apt_packages", {}):
                errors.append("missing apt package: " + name)
        if role in ("boat", "tenant"):
            equal("sccache build inputs", manifest["sccache"]["inputs_sha256"], actual.get("sccache_inputs_sha256"))
            artifacts = actual.get('sccache_artifacts') or {}
            equal('sccache artifact inputs', manifest['sccache']['inputs_sha256'], artifacts.get('inputs_sha256'))
            for name in manifest['sccache']['build_server_sha256']:
                equal(name + ' built artifact hash', artifacts.get('binaries', {}).get(name), bins.get(name, {}).get('sha256'))
    return errors


def check(manifest, role="build-server", job=False, gnu_test_sha256=None, cargo_registry=None):
    if role == "macos":
        validate(manifest, role)
        actual = capture(manifest, job=job, role=role)
        errors = differences(manifest, actual, role, job)
        encoded = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
        return {"passed": not errors, "errors": errors, "role": role, "job": job,
                "manifest_sha256": hashlib.sha256(encoded).hexdigest(), "observed": actual}
    validate(manifest)
    actual = capture(role_manifest(manifest, role, job), job=job, role=role)
    if role in ("boat", "tenant") and not job:
        state = Path('/usr/local/lib/motor' if role == 'tenant' else '/srv/build')
        marker = state / "sccache-inputs.sha256"
        actual["sccache_inputs_sha256"] = marker.read_text() if marker.is_file() else None
        artifacts = state / 'sccache-artifacts.json'
        actual['sccache_artifacts'] = json.loads(artifacts.read_text()) if artifacts.is_file() else None
    errors = differences(manifest, actual, role, job, gnu_test_sha256)
    registry = None
    if cargo_registry is not None:
        seed.validate(cargo_registry)
        registry = seed.differences(cargo_registry, actual['cargo_registry'])
        if not registry['passed']:
            errors.append('Cargo registry parity: ' + json.dumps(registry, sort_keys=True))
    elif role == 'boat':
        errors.append('Cargo registry parity requires the operator-supplied server seed manifest')
    # Keep qualification output bounded without discarding the measured versions.
    actual.pop("cargo_metadata", None)
    actual.pop('cargo_registry', None)
    for row in [*actual["cargo_binaries"].values(), *actual["tools"].values()]:
        if row and row.get("version"):
            row["version"] = first_line(row["version"])
    required = manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"][role]
    actual["apt_packages"] = {name: version for name, version in actual["apt_packages"].items() if name in required}
    if job:
        actual["mountinfo"] = "\n".join(line for line in actual["mountinfo"].splitlines()
                                          if manifest["rustup_home"] in line.split()[:6])
    encoded = json.dumps(manifest, sort_keys=True, separators=(",", ":")).encode()
    return {"passed": not errors, "errors": errors, "role": role, "job": job,
            "manifest_sha256": hashlib.sha256(encoded).hexdigest(), "observed": actual,
            "cargo_registry": registry}


def refresh(manifest, actual):
    # Preserve install sources and accepted OS differences; refresh host measurements.
    manifest = json.loads(json.dumps(manifest))
    for key in ("rust_default", "rust_profile", "rust_toolchains", "cargo_home", "rustup_home"):
        manifest[key] = actual[key]
    manifest["rustup"] = actual["rustup_version"].split()[1]
    manifest["cargo_binaries"] = {}
    for name, row in actual["cargo_binaries"].items():
        expected: dict[str, object] = {"proxy": True} if row["symlink"] == "rustup" else {}
        if row.get("version"):
            if row.get("exit_code") != 0:
                raise ValueError("failed version capture: " + name)
            expected["version"] = first_line(row["version"])
        elif name not in ("rustup", "sccache", "sccache-0.18.0-stock") and not expected:
            raise ValueError("unmeasured binary: " + name)
        manifest["cargo_binaries"][name] = expected
    for install in manifest["cargo_installs"]:
        if install.get("default"):
            for name in install["bins"]:
                variant = name + "-" + install["rev"][:8]
                if actual["cargo_binaries"][name]["sha256"] != actual["cargo_binaries"][variant]["sha256"]:
                    raise ValueError("default binary does not match pinned revision: " + name)
                manifest["cargo_binaries"][name]["matches"] = variant
    for name, expected in manifest["tools"].items():
        row = actual["tools"][name]
        if not row or row.get("exit_code") != 0:
            raise ValueError("unmeasured tool: " + name)
        expected["path"] = row["path"]
        if "version" in expected:
            expected["version"] = first_line(row["version"])
        else:
            expected["observed_build_server_version"] = first_line(row["version"])
    manifest["gnu_coreutils"] = {name: row["path"] for name, row in actual["tools"].items()
                                 if row and row.get("package", "").startswith("gnu-coreutils:")}
    required = manifest["apt_packages"]["required"] + manifest["apt_packages"]["by_role"]["build-server"]
    manifest["apt_packages"]["observed_build_server_versions"] = {name: actual["apt_packages"][name] for name in required}
    manifest["sccache"]["build_server_sha256"] = {
        name: actual["cargo_binaries"][name]["sha256"] for name in ("sccache", "sccache-0.18.0-stock")}
    manifest["sccache"]["inputs_sha256"] = "".join(hashlib.sha256((HERE / name).read_bytes()).hexdigest() for name in (
        "build-sccache.sh", "patches/sccache-0.18.0-read-only-hits.patch"))
    manifest["capture_sha256"] = hashlib.sha256(json.dumps(actual, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    validate(manifest)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group(required=True)
    modes.add_argument("--capture", action="store_true",
                        help="print observed state as JSON; does not install or modify tools")
    modes.add_argument("--check", action="store_true", help="compare live state with the manifest; exit nonzero on drift")
    modes.add_argument("--from-capture", type=Path, help="refresh measured manifest fields from captured JSON to stdout")
    parser.add_argument("--manifest", type=Path, default=MANIFEST)
    parser.add_argument("--role", choices=("build-server", "boat", "tenant", "macos"), default="build-server")
    parser.add_argument("--job", action="store_true", help="check inside a real Rust job, including PATH and read-only rustup")
    parser.add_argument('--gnu-test-sha256', help='for --job: hash of gnutest from the fresh package-qualified host check')
    parser.add_argument('--cargo-seed-manifest', type=Path, help='trusted server seed inventory; required for Boat parity')
    parser.add_argument('--cargo-seed-sha256', help='independently recorded server seed manifest digest')
    args = parser.parse_args()
    if args.role == "macos" and args.from_capture:
        parser.error('macOS pins must be reviewed against the build-server manifest, not refreshed from capture')
    if bool(args.cargo_seed_manifest) != bool(args.cargo_seed_sha256):
        parser.error('--cargo-seed-manifest and --cargo-seed-sha256 must be supplied together')
    if args.capture:
        # The manifest names tools beyond the built-in list (pinned variants such
        # as bun-1.3.14) and the GNU coreutils locations; without it the capture
        # omits them and --from-capture refuses it as unmeasured.
        manifest = json.loads(args.manifest.read_text())
        if args.role == "macos":
            result = capture(manifest, job=args.job, role=args.role)
        else:
            result = capture(role_manifest(manifest, args.role, args.job), job=args.job, role=args.role)
    else:
        manifest = json.loads(args.manifest.read_text())
        result = refresh(manifest, json.loads(args.from_capture.read_text())) if args.from_capture else check(
            manifest, args.role, args.job, args.gnu_test_sha256,
            seed.verified_manifest(args.cargo_seed_manifest, args.cargo_seed_sha256) if args.cargo_seed_manifest else None)
    print(json.dumps(result, indent=2, sort_keys=True))
    return 1 if args.check and not result["passed"] else 0


if __name__ == "__main__":
    sys.exit(main())
