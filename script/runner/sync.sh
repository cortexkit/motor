#!/usr/bin/env bash
# Operator tool: populate a source copy by hand from repository roots.
# Normal remote jobs synchronize through the ck-motor execution service instead.
# Send Git objects without local config, then Git's working-tree file set.
set -euo pipefail
host=${RUNNER_HOST:-alfonso-runner}
script_dir=$(cd "$(dirname "$0")" && pwd)
# A real sync takes minutes. A lock older than an hour belongs to a sync that
# was killed or lost its connection, so a later sync may reclaim it.
lock_stale_after=${RUNNER_LOCK_STALE_AFTER_S:-3600}
[[ "$lock_stale_after" =~ ^[1-9][0-9]*$ ]] || {
  echo 'RUNNER_LOCK_STALE_AFTER_S must be a positive number of seconds' >&2
  exit 2
}
lock_owner="$host pid=$$ token=$RANDOM$RANDOM"
printf -v lock_owner_arg '%q' "$lock_owner"
if [ "$#" -eq 0 ]; then
  set -- "$(cd "$script_dir/../.." && pwd)"
fi
command -v bun >/dev/null
command -v rsync >/dev/null
export GIT_CONFIG_NOSYSTEM=1
unset GIT_CONFIG_COUNT GIT_CONFIG_KEY_0 GIT_CONFIG_VALUE_0
scratch=$(mktemp -d)
locked=0
cleanup() {
  if [ "$locked" -eq 1 ]; then
    if ! ssh -o BatchMode=yes -o ForwardAgent=no "$host" \
      "flock -x /srv/build/runner.lock.guard bash -s -- release $lock_owner_arg" <<'REMOTE'
set -euo pipefail
lock=/srv/build/runner.lock
owner=$2
if [ -d "$lock" ] && [ -r "$lock/owner" ] && [ "$(cat -- "$lock/owner")" = "$owner" ]; then
  rm -f -- "$lock/owner" "$lock/started_at"
  rmdir -- "$lock"
fi
REMOTE
    then
      echo "Unable to release source-copy lock for $lock_owner; it can be reclaimed after ${lock_stale_after}s" >&2
    fi
  fi
  rm -rf "$scratch"
}
trap cleanup EXIT

# Bun supplies a TOML parser on the operator machine, including machines whose
# Python predates tomllib. Inspect every Cargo.toml in each discovered repository, not just the
# current workspace members: target, patch, dev and workspace deps all matter.
bun - "$scratch" "$@" <<'JS'
const fs = require("node:fs");
const path = require("node:path");
const scratch = process.argv[2];
const roots = new Map();
function git(root, args) {
  const p = Bun.spawnSync(["git", "-C", root, ...args]);
  if (p.exitCode) throw new Error(p.stderr.toString());
  return p.stdout.toString();
}
function add(root, name) {
  root = fs.realpathSync(root);
  if (fs.realpathSync(git(root, ["rev-parse", "--show-toplevel"]).trim()) !== root)
    throw new Error(`Not a repository root: ${root}`);
  name ??= path.basename(root);
  if (!/^[A-Za-z0-9_-]+$/.test(name)) throw new Error(`Unsafe repository name: ${name}`);
  if (roots.has(name) && roots.get(name) !== root) throw new Error(`Duplicate repository name: ${name}`);
  roots.set(name, root);
}
for (const root of process.argv.slice(3)) add(root);
function files(root) {
  return [...new Set(git(root, ["ls-files", "-z", "--cached", "--others", "--exclude-standard"]).split("\0"))]
    .filter(f => {
      if (!f) return false;
      try { fs.lstatSync(path.join(root, f)); return true; }
      catch (error) { if (error.code === "ENOENT") return false; throw error; }
    });
}
function depPaths(obj, inDeps = false) {
  let result = [];
  for (const [key, value] of Object.entries(obj)) {
    if (inDeps && key === "path" && typeof value === "string") result.push(value);
    else if (value && typeof value === "object") result.push(...depPaths(value,
      inDeps || ["dependencies", "dev-dependencies", "build-dependencies", "patch", "replace"].includes(key)));
  }
  return result;
}
const done = new Set();
while (done.size < roots.size) {
  for (const [name, root] of roots) {
    if (done.has(name)) continue;
    done.add(name);
    // Worktrees are not siblings of the other checkouts. Resolve external paths
    // against the original repo's sibling directory, or explicit roots supplied
    // by the caller, while syncing the caller's own working tree under its basename.
    const common = path.resolve(root, git(root, ["rev-parse", "--git-common-dir"]).trim());
    const originalParent = path.dirname(path.dirname(common));
    for (const file of files(root).filter(f => path.basename(f) === "Cargo.toml")) {
      const manifest = path.join(root, file);
      for (const dep of depPaths(Bun.TOML.parse(fs.readFileSync(manifest, "utf8")))) {
        if (path.isAbsolute(dep)) throw new Error(`Absolute path dependency cannot be relocated: ${manifest}: ${dep}`);
        const resolved = path.resolve(path.dirname(manifest), dep);
        if (resolved === root || resolved.startsWith(root + path.sep)) {
          if (!fs.existsSync(path.join(resolved, "Cargo.toml"))) throw new Error(`Missing dependency: ${resolved}`);
          continue;
        }
        const relative = path.relative(path.dirname(root), resolved).split(path.sep);
        const depName = relative.shift();
        if (!depName || depName === "..") throw new Error(`Dependency is not a sibling repo: ${resolved}`);
        const depRoot = roots.get(depName) ?? path.join(originalParent, depName);
        if (!fs.existsSync(path.join(depRoot, ...relative, "Cargo.toml")))
          throw new Error(`Missing path dependency ${dep} from ${manifest}; pass its repository root to sync.sh`);
        add(depRoot, depName);
      }
    }
  }
}
let rows = "";
for (const [name, root] of roots) {
  if (/[\n\t]/.test(root)) throw new Error("Repository root contains a tab or newline");
  const list = files(root);
  for (const f of list) {
    const full = path.join(root, f);
    if (fs.lstatSync(full).isDirectory()) throw new Error(`Submodule needs an explicit checkout: ${full}`);
    if (fs.lstatSync(full).isSymbolicLink()) {
      const dest = fs.realpathSync(full);
      if (!dest.startsWith(root + path.sep))
        throw new Error(`Unsafe source symlink: ${full}`);
    }
  }
  fs.writeFileSync(path.join(scratch, name + ".files"), list.join("\0") + "\0");
  rows += name + "\t" + root + "\n";
}
fs.writeFileSync(path.join(scratch, "repos"), rows);
JS

# The remote update uses several SSH sessions, so one owner record protects the whole sync.
if ! ssh -o BatchMode=yes -o ForwardAgent=no "$host" \
  "flock -x /srv/build/runner.lock.guard bash -s -- acquire $lock_owner_arg $lock_stale_after" <<'REMOTE'
set -euo pipefail
lock=/srv/build/runner.lock
owner=$2
stale_after=$3
now=$(date +%s)
if [ -e "$lock" ]; then
  old_owner='unrecorded legacy lock'
  if [ -r "$lock/owner" ]; then old_owner=$(cat -- "$lock/owner"); fi
  if [ -r "$lock/started_at" ]; then
    old_started=$(cat -- "$lock/started_at")
  else
    old_started=$(stat -c %Y -- "$lock")
  fi
  if [[ ! "$old_started" =~ ^[0-9]+$ ]]; then
    old_started=$(stat -c %Y -- "$lock")
  fi
  age=$((now - old_started))
  if (( age < 0 )); then age=0; fi
  if (( age < stale_after )); then
    printf 'Runner busy: owner=%s started_at=%s age_s=%s\n' "$old_owner" "$old_started" "$age" >&2
    exit 1
  fi
  printf 'Reclaiming stale source-copy lock: owner=%s started_at=%s age_s=%s\n' \
    "$old_owner" "$old_started" "$age" >&2
  stale=$(mktemp -d /srv/build/.runner.lock.stale.XXXXXX)
  rmdir -- "$stale"
  mv -T -- "$lock" "$stale"
  rm -rf -- "$stale"
fi
pending=$(mktemp -d /srv/build/.runner.lock.new.XXXXXX)
printf '%s\n' "$owner" > "$pending/owner"
date +%s > "$pending/started_at"
if ! mv -T -- "$pending" "$lock"; then
  rm -rf -- "$pending"
  echo 'Runner busy while acquiring the source-copy lock' >&2
  exit 1
fi
REMOTE
then
  echo 'Runner busy (or not set up); inspect /srv/build/runner.lock before retrying' >&2
  exit 1
fi
locked=1
while IFS=$'\t' read -r name root; do
  echo "sync: $name <- $root"
  head=$(git -C "$root" rev-parse --verify HEAD)
  state=$(ssh -n -o BatchMode=yes -o ForwardAgent=no "$host" \
    "git --git-dir=/srv/build/mirrors/$name.git rev-parse --verify refs/runner/head 2>/dev/null || true; git --git-dir=/srv/build/mirrors/$name.git cat-file -e '$head^{commit}' 2>/dev/null && echo present || true")
  previous=${state%%$'\n'*}
  if [[ "$state" != *present ]]; then
    revisions=(HEAD)
    # A different local worktree may not know the mirror's last head. In that
    # case send a full HEAD bundle; no private-host fetch is ever needed.
    if [[ "$previous" =~ ^[0-9a-f]{40}$ ]] && git -C "$root" cat-file -e "$previous^{commit}" 2>/dev/null; then
      revisions+=("^$previous")
    fi
    git -C "$root" bundle create "$scratch/$name.bundle" "${revisions[@]}"
    rsync -az -e 'ssh -o BatchMode=yes -o ForwardAgent=no' \
      "$scratch/$name.bundle" "$host:/srv/build/bundles/$name.bundle"
  fi
  ssh -o BatchMode=yes -o ForwardAgent=no "$host" "bash -s -- $name $head" <<'REMOTE'
set -euo pipefail
name=$1
head=$2
mirror=/srv/build/mirrors/$name.git
workspace=/srv/build/src/$name
if [ ! -d "$mirror" ]; then git init --bare "$mirror"; fi
if [ -f "/srv/build/bundles/$name.bundle" ]; then
  git --git-dir="$mirror" bundle verify "/srv/build/bundles/$name.bundle"
  git --git-dir="$mirror" fetch --no-tags "/srv/build/bundles/$name.bundle" +HEAD:refs/runner/head
  rm "/srv/build/bundles/$name.bundle"
fi
git --git-dir="$mirror" cat-file -e "$head^{commit}"
git --git-dir="$mirror" update-ref refs/runner/head "$head"
mkdir -p "$workspace"
if [ ! -d "$workspace/.git" ]; then git init "$workspace"; fi
# The working repository borrows objects from our credential-free mirror. Its
# config and hooks are created on the runner, never copied from the operator machine.
printf '%s\n' "$mirror/objects" > "$workspace/.git/objects/info/alternates"
git -C "$workspace" -c core.hooksPath=/dev/null checkout --force --detach "$head"
REMOTE
  mkdir "$scratch/$name"
  # Staging first allows --delete to remove stale, newly ignored and deleted
  # source on the server; --files-from alone would leave those files behind.
  rsync -a --from0 --files-from="$scratch/$name.files" "$root/" "$scratch/$name/"
  # Protect only the metadata we just generated. This does not filter the Git
  # working-tree list: tracked fixtures under ignored directories still sync.
  rsync -az --delete --exclude=/.git/ -e 'ssh -o BatchMode=yes -o ForwardAgent=no' \
    "$scratch/$name/" "$host:/srv/build/src/$name/"
done < "$scratch/repos"
