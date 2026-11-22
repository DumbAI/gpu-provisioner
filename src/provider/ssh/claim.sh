# Adopt this machine for one gpuprov VM (`provider::ssh` create). Run as
# the configured user: bash -s -- <state dir> <vm name> <gpuprov public key>.
# Idempotent: claiming again for the same VM only re-checks the key.
set -eu
dir=$1 vm=$2 key=$3
claim=$dir/ssh-claim
keys=$HOME/.ssh/authorized_keys

sudo -n true 2>/dev/null || {
  echo "user $(id -un) needs passwordless sudo (k3s installs as a service)" >&2
  exit 10
}
command -v systemctl >/dev/null 2>&1 || {
  echo "systemd is required: k3s runs as a service" >&2
  exit 11
}

mkdir -p "$HOME/.ssh"
chmod 700 "$HOME/.ssh"
touch "$keys"
chmod 600 "$keys"

if [ ! -f "$claim" ]; then
  # k3s that gpuprov did not install would be reconfigured and restarted
  # by the installer: never take over someone else's cluster.
  if [ -e /usr/local/bin/k3s ] && [ ! -f "$dir/k3s-install.status" ]; then
    echo "k3s is already installed here, not by gpuprov: remove it or use another host" >&2
    exit 13
  fi
  # A key that was already authorized (e.g. the user logs in with it) is
  # never removed on release.
  keyed=0
  if grep -qxF -- "$key" "$keys"; then keyed=1; fi
  sudo mkdir -p "$dir"
  # Exclusive create (noclobber): of two concurrent claims, one wins.
  printf 'vm=%s\nkey_preexisting=%s\nkey=%s\n' "$vm" "$keyed" "$key" |
    sudo sh -c 'set -C; umask 022; cat > "$1"' sh "$claim" 2>/dev/null || true
fi

owner=$(sed -n 's/^vm=//p' "$claim")
if [ "$owner" != "$vm" ]; then
  echo "the host serves VM '$owner'" >&2
  exit 12
fi
grep -qxF -- "$key" "$keys" || printf '%s\n' "$key" >> "$keys"
