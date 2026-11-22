# Which VM this machine serves (`provider::ssh` observe): prints the
# claiming VM's name, or nothing. bash -s -- <state dir>. Reads only.
set -eu
claim=$1/ssh-claim
[ -f "$claim" ] || exit 0
sed -n 's/^vm=//p' "$claim"
