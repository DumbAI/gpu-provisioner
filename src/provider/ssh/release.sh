# Give this machine back (`provider::ssh` delete): what the claim added
# goes, nothing else. bash -s -- <state dir> <vm name> <keep k3s: 0|1>.
# Idempotent: a host that is not this VM's is left alone.
set -eu
dir=$1 vm=$2 keep=$3
claim=$dir/ssh-claim
[ -f "$claim" ] || exit 0
[ "$(sed -n 's/^vm=//p' "$claim")" = "$vm" ] || exit 0

sudo -n true 2>/dev/null || {
  echo "user $(id -un) needs passwordless sudo" >&2
  exit 10
}
# The only k3s here is gpuprov's (a claim refuses any other).
if [ "$keep" != 1 ] && [ -x /usr/local/bin/k3s-uninstall.sh ]; then
  sudo /usr/local/bin/k3s-uninstall.sh >/dev/null 2>&1
  sudo rm -f "$dir/k3s-install.status"
fi
if [ "$(sed -n 's/^key_preexisting=//p' "$claim")" = 0 ]; then
  key=$(sed -n 's/^key=//p' "$claim")
  keys=$HOME/.ssh/authorized_keys
  if [ -f "$keys" ]; then
    # Rewrite in place: the file keeps its owner and mode.
    grep -vxF -- "$key" "$keys" > "$keys.gpuprov" || true
    cat "$keys.gpuprov" > "$keys"
    rm -f "$keys.gpuprov"
  fi
fi
sudo rm -f "$claim"
