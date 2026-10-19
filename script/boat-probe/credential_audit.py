"""Guest-side account credential audit; returns names and paths, never values."""

SOURCE = r'''
import os, pathlib, re

def credential_audit(roots=("/root", "/home/user", "/etc"), environment=None):
    sensitive = re.compile(r"(^BOAT_|TOKEN|SECRET|PASSWORD|API_KEY|ACCESS_KEY|PRIVATE_KEY|CREDENTIAL)", re.I)
    environment = os.environ if environment is None else environment
    result = {"environment_names": sorted(k for k in environment if sensitive.search(k)),
              "credential_paths": [], "scanned_roots": [], "errors": []}
    names = {".netrc", ".git-credentials", ".npmrc", "credentials", "credentials.json",
             "credentials.toml", "hosts.yml", "auth.json", "auth.conf", "token", "tokens.json",
             "id_rsa", "id_ed25519", "id_ecdsa", "config.json"}
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
                # Docker's generic config name matters only inside its own directory.
                match = lower in names and (lower != "config.json" or ".docker" in path.parts)
                match |= "credential" in lower or lower.endswith((".token", ".tokens"))
                if match and (path.is_symlink() or path.stat().st_size):
                    result["credential_paths"].append(str(path))
    for root in map(pathlib.Path, roots):
        for name in ("environment", "profile", ".profile"):
            path = root / name
            if path.exists():
                for key in re.findall(r"(?m)^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=", path.read_text()):
                    if sensitive.search(key):
                        result["environment_names"].append(str(path) + ":" + key)
    result["credential_paths"].sort()
    result["clean"] = not any(result[k] for k in ("environment_names", "credential_paths", "errors"))
    return result
'''
