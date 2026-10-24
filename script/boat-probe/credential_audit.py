"""Guest account credential audit: report names and paths, never secret values."""

SOURCE = r'''
import os, pathlib, pwd, re

def credential_audit(roots=None, environment=None):
    homes = sorted({p.pw_dir for p in pwd.getpwall() if p.pw_dir == "/root" or p.pw_dir.startswith("/home/")})
    api_homes = sorted({p.pw_dir for p in pwd.getpwall() if p.pw_dir.startswith("/")}) if roots is None else list(roots)
    roots = [*homes, "/etc"] if roots is None else list(roots)
    environment = os.environ if environment is None else environment
    sensitive = re.compile(r"(^BOAT_|TOKEN|SECRET|PASSWORD|API_KEY|ACCESS_KEY|PRIVATE_KEY|CREDENTIAL)", re.I)
    neutral = {"ASCII_TOKEN", "BOAT_ID"}
    result = {"environment_names": sorted(k for k in environment if sensitive.search(k) and k not in neutral),
              "provider_environment_names": sorted(k for k in environment if k in neutral),
              "credential_paths": [], "source_lookalikes": [], "npmrc_keys": {},
              "boat_api_key_absent": {}, "scanned_roots": [], "errors": []}
    for home in api_homes:
        path = pathlib.Path(home) / ".config/ascii/boat/config.json"
        result["boat_api_key_absent"][str(path)] = not (path.exists() or path.is_symlink())
    names = {".netrc", ".git-credentials", "credentials", "credentials.json", "credentials.toml",
             "hosts.yml", "auth.json", "auth.conf", "token", "tokens.json", "id_rsa", "id_ed25519", "id_ecdsa"}
    for root in map(pathlib.Path, roots):
        result["scanned_roots"].append(str(root))
        count = 0
        def error(e):
            result["errors"].append(str(e.filename))
        for directory, dirs, files in os.walk(root, onerror=error, followlinks=False):
            count += len(dirs) + len(files)
            if count > 200000:
                result["errors"].append(str(root) + ": enumeration cap reached")
                break
            for name in files:
                path = pathlib.Path(directory) / name
                lower = name.lower()
                try:
                    if lower == ".npmrc":
                        keys = []
                        for line in path.read_text().splitlines():
                            line = line.strip()
                            if line and not line.startswith(("#", ";")):
                                key, sep, _ = line.partition("=")
                                if not sep or not re.fullmatch(r"[A-Za-z0-9_./:@-]+", key.strip()):
                                    result["errors"].append(str(path) + ": unsupported npmrc syntax")
                                else:
                                    keys.append(key.strip())
                        result["npmrc_keys"][str(path)] = sorted(keys)
                        if any(re.search(r"auth|password|token|username|certfile|keyfile", k, re.I) for k in keys):
                            result["credential_paths"].append(str(path))
                    elif lower in names or lower.endswith((".token", ".tokens")) or (lower == "config.json" and ".docker" in path.parts):
                        if path.is_symlink() or path.stat().st_size:
                            result["credential_paths"].append(str(path))
                    elif "credential" in lower:
                        # Installed SDK source filenames are not account credential stores.
                        # Boat's base image also caches toolchain sources (Node's under
                        # ~/.nvm/.cache), whose C++ and ES-module files carry such names.
                        if lower.endswith((".py", ".pyc", ".js", ".mjs", ".cjs", ".ts", ".mts", ".cts",
                                           ".map", ".c", ".cc", ".cpp", ".h", ".hpp")):
                            result["source_lookalikes"].append(str(path))
                        else:
                            result["credential_paths"].append(str(path))
                except OSError:
                    result["errors"].append(str(path))
        for name in ("environment", "profile", ".profile", ".bashrc"):
            path = root / name
            if path.exists():
                for key in re.findall(r"(?m)^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=", path.read_text()):
                    if sensitive.search(key) and key not in neutral:
                        result["environment_names"].append(str(path) + ":" + key)
    result["credential_paths"].sort()
    result["source_lookalikes"].sort()
    result["clean"] = not any(result[k] for k in ("environment_names", "credential_paths", "errors")) and all(result["boat_api_key_absent"].values())
    return result
'''
