#!/usr/bin/env bash
# Operator tool: bootstrap a builder and populate a source copy by hand.
# Normal jobs use the ck-motor execution service; this script does not deploy it.
# Bootstrap the dedicated Ubuntu builder from a Mac; no private Git fetches.
set -euo pipefail
host=${RUNNER_HOST:-alfonso-runner}
script_dir=$(cd "$(dirname "$0")" && pwd)
root=${RUNNER_REPO_ROOT:-$(cd "$script_dir/../.." && pwd)}
root=$(cd "$root" && pwd -P)
name=${root##*/}
[[ "$name" =~ ^[A-Za-z0-9_-]+$ ]] || { echo "Unsafe repository name: $name" >&2; exit 2; }
if [ -f "$root/rust-toolchain.toml" ]; then
  toolchain=$(bun -e 'console.log(Bun.TOML.parse(await Bun.file(process.argv[1]).text()).toolchain.channel)' "$root/rust-toolchain.toml")
elif [ -f "$root/rust-toolchain" ]; then
  toolchain=$(<"$root/rust-toolchain")
else
  toolchain=$(rustc --version | cut -d ' ' -f 2)
fi
nextest=$(cargo nextest --version | head -n 1 | cut -d ' ' -f 2)
bun_version=$(bun --version)
for version in "$toolchain" "$nextest" "$bun_version"; do
  [[ "$version" =~ ^[a-zA-Z0-9._-]+$ ]] || { echo "Invalid version: $version" >&2; exit 1; }
done
echo "setup: $host rust=$toolchain nextest=$nextest bun=$bun_version"
ssh -o BatchMode=yes -o ForwardAgent=no "$host" \
  "bash -s -- $toolchain $nextest $bun_version" <<'REMOTE'
set -euo pipefail
test "$(id -un)" = ubuntu
sudo -n true
# Refuse to put the build cache on the small root disk if the array is missing.
mountpoint -q /srv/build
test "$(findmnt -n -o FSTYPE /srv/build)" = xfs
export DEBIAN_FRONTEND=noninteractive
sudo -n apt-get update
# Workspace build.rs files invoke rustc and hash Cargo.lock, with no generators.
# Bundled SQLite/crypto dependencies need the C toolchain. unzip is required by
# Bun's installer; xz and CA certificates are required by toolchain downloads.
sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y \
  build-essential clang lld pkg-config libssl-dev sqlite3 git rsync curl jq \
  ca-certificates unzip xz-utils unattended-upgrades ufw nodejs npm
sudo -n install -d -o ubuntu -g ubuntu /srv/build /srv/build/src /srv/build/target /srv/build/cargo-home /srv/build/tools-target /srv/build/mirrors /srv/build/bundles

# Mac paths must share the generations' XFS device: FICLONE cannot cross from
# /srv/build to the root filesystem. Refuse to hide pre-existing /Users data;
# moving an existing installation onto the array requires an operator first.
sudo -n install -d -o ubuntu -g ubuntu -m 0755 /srv/build/Users /Users
if ! mountpoint -q /Users && [ -n "$(sudo -n find /Users -mindepth 1 -maxdepth 1 -print -quit)" ]; then
  echo 'setup: /Users is nonempty and not mounted; migrate it to /srv/build/Users before setup' >&2
  exit 1
fi
users_bind='/srv/build/Users /Users none bind 0 0'
awk '$1 !~ /^#/ && $2 == "/Users" && ($1 != "/srv/build/Users" || $3 != "none" || $4 != "bind") { bad=1 } END { exit bad }' /etc/fstab || {
  echo 'setup: conflicting /Users entry in /etc/fstab' >&2
  exit 1
}
if ! grep -Fxq "$users_bind" /etc/fstab; then
  printf '%s\n' "$users_bind" | sudo -n tee -a /etc/fstab >/dev/null
fi
mountpoint -q /Users || sudo -n mount /Users
test "$(findmnt -n -o FSTYPE /Users)" = xfs
test "$(stat -c %d /Users)" = "$(stat -c %d /srv/build)"
reflink_source=$(mktemp /srv/build/.motor-reflink-source.XXXXXX)
reflink_target=$(mktemp /Users/.motor-reflink-target.XXXXXX)
trap 'rm -f "$reflink_source" "$reflink_target"' EXIT
printf 'motor runner same-filesystem reflink probe\n' > "$reflink_source"
cp --reflink=always "$reflink_source" "$reflink_target"
cmp "$reflink_source" "$reflink_target"
rm -f "$reflink_source" "$reflink_target"
trap - EXIT
echo 'setup: /Users XFS bind mount and nonempty reflink probe passed'

# Open the SSH port before enabling the firewall. Do not reset unrelated rules:
# refuse a non-dedicated host instead of silently deleting its service policy.
sudo -n ufw default deny incoming
sudo -n ufw default allow outgoing
sudo -n ufw allow 22/tcp
sudo -n ufw --force enable
sudo -n tee /etc/ssh/sshd_config.d/00-runner-keys-only.conf >/dev/null <<'SSH'
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
PermitRootLogin no
AllowAgentForwarding no
AllowTcpForwarding no
X11Forwarding no
SSH
sudo -n /usr/sbin/sshd -t
sudo -n systemctl reload ssh
effective=$(sudo -n /usr/sbin/sshd -T)
for setting in 'passwordauthentication no' 'kbdinteractiveauthentication no' 'permitrootlogin no'; do
  grep -Fx "$setting" <<< "$effective"
done
sudo -n tee /etc/apt/apt.conf.d/99-runner-security-only >/dev/null <<'APT'
#clear Unattended-Upgrade::Allowed-Origins;
#clear Unattended-Upgrade::Origins-Pattern;
Unattended-Upgrade::Allowed-Origins { "${distro_id}:${distro_codename}-security"; };
APT
sudo -n tee /etc/apt/apt.conf.d/20auto-upgrades >/dev/null <<'APT'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
APT
sudo -n systemctl enable --now apt-daily.timer apt-daily-upgrade.timer
# Reject additional firewall exceptions and non-loopback service listeners.
# Local DNS/NTP and the DHCP client are OS plumbing, not inbound services.
sudo -n ufw status numbered
sudo -n ufw status | awk '/ALLOW IN/ && $1 != "22\/tcp" { bad=1 } END { exit bad }'
sudo -n ss -H -lntup | awk '
  { addr=$5; if (addr ~ /^127\./ || addr ~ /^\[::1\]:/) next;
    if ($1 == "tcp" && addr ~ /:22$/) next;
    if ($1 == "udp" && addr ~ /:68$/) next;
    print "Unexpected listener: " $0; bad=1 }
  END { exit bad }'

export CARGO_HOME=/srv/build/cargo-home
export RUSTUP_HOME="$HOME/.rustup"
export CARGO_TARGET_DIR=/srv/build/tools-target
export PATH="$CARGO_HOME/bin:$HOME/.bun/bin:$PATH"
if ! command -v rustup >/dev/null; then
  curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o /tmp/runner-rustup.sh
  bash /tmp/runner-rustup.sh -y --no-modify-path --profile minimal --default-toolchain "$1"
  rm /tmp/runner-rustup.sh
fi
rustup toolchain install "$1" --profile minimal
rustup default "$1"
if [ "$(cargo nextest --version 2>/dev/null | head -n 1 | cut -d ' ' -f 2 || true)" != "$2" ]; then
  cargo install cargo-nextest --locked --version "$2"
fi
if [ "$(bun --version 2>/dev/null || true)" != "$3" ]; then
  curl --proto '=https' --tlsv1.2 -fsSL https://bun.com/install -o /tmp/runner-bun.sh
  bash /tmp/runner-bun.sh "bun-v$3"
  rm /tmp/runner-bun.sh
fi
# Install alongside the system Node/npm executables, not in a shell-local
# Corepack cache: trusted job preparation resolves pnpm through the service PATH.
if [ "$(pnpm --version 2>/dev/null || true)" != "11.1.2" ]; then
  sudo -n npm install --global --prefix /usr/local pnpm@11.1.2 --ignore-scripts
fi
test "$(pnpm --version)" = '11.1.2'
cat > /srv/build/runner.env <<ENV
export CARGO_HOME=/srv/build/cargo-home
export CARGO_TARGET_DIR=/srv/build/target
export RUSTUP_HOME=$HOME/.rustup
export RUSTUP_TOOLCHAIN=$1
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=clang
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS='-C link-arg=-fuse-ld=lld'
export PATH=/srv/build/cargo-home/bin:$HOME/.bun/bin:\$PATH
ENV
source /srv/build/runner.env
rustc --version
cargo nextest --version
bun --version
node --version
npm --version
pnpm --version
REMOTE
# A second connection proves the keys-only policy did not lock out this client.
"$script_dir/sync.sh" "$root"
# Some source copies provide a checksum-verified NATS fixture installer.
# The motor repository has no NATS tests or installer; provision it only when
# the source repository requests it, not as a dependency on another checkout.
if [ -f "$root/scripts/ci/install-nats-server.sh" ]; then
  ssh -o BatchMode=yes -o ForwardAgent=no "$host" "bash -s -- $name" <<'REMOTE'
set -euo pipefail
cd "/srv/build/src/$1"
nats_env=$(mktemp)
trap 'rm -f "$nats_env"' EXIT
RUNNER_TEMP=/srv/build/tools-target GITHUB_ENV="$nats_env" bash scripts/ci/install-nats-server.sh
# Remote jobs start with a cleared environment, but /usr/local/bin is on their
# PATH. Install the verified bytes there, not just a shell-local CK override.
nats_binary=$(<"$nats_env")
nats_binary=${nats_binary#CK_NATS_SERVER_BIN=}
sudo install -m 0755 "$nats_binary" /usr/local/bin/nats-server
cmp "$nats_binary" /usr/local/bin/nats-server
sha256sum "$nats_binary" /usr/local/bin/nats-server
test "$(/usr/local/bin/nats-server --version)" = 'nats-server: v2.15.0'
/usr/local/bin/nats-server --version
REMOTE
fi
echo 'setup: complete'
