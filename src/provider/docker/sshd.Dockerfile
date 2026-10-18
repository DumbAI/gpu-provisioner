# The gpuprov Docker test cloud's "VM image": a minimal sshd box with the
# same login contract as a cloud-init'd VM — $SSH_USER with passwordless sudo
# and $SSH_PUBLIC_KEY authorized. Built from stdin (no build context), so the
# entrypoint is written inline.
FROM debian:bookworm-slim

RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      openssh-server sudo ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /run/sshd \
 && rm -f /etc/ssh/ssh_host_*

RUN printf '%s\n' \
  '#!/bin/sh' \
  'set -eu' \
  ': "${SSH_USER:?SSH_USER is required}"' \
  ': "${SSH_PUBLIC_KEY:?SSH_PUBLIC_KEY is required}"' \
  'if ! id "$SSH_USER" >/dev/null 2>&1; then useradd -m -s /bin/bash "$SSH_USER"; fi' \
  'echo "$SSH_USER ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/90-$SSH_USER"' \
  'chmod 0440 "/etc/sudoers.d/90-$SSH_USER"' \
  'HOME_DIR=$(getent passwd "$SSH_USER" | cut -d: -f6)' \
  'mkdir -p "$HOME_DIR/.ssh"' \
  'printf "%s\n" "$SSH_PUBLIC_KEY" > "$HOME_DIR/.ssh/authorized_keys"' \
  'chmod 700 "$HOME_DIR/.ssh" && chmod 600 "$HOME_DIR/.ssh/authorized_keys"' \
  'chown -R "$SSH_USER:$SSH_USER" "$HOME_DIR/.ssh"' \
  'ssh-keygen -A' \
  'exec /usr/sbin/sshd -D -e' \
  > /usr/local/bin/gpuprov-entrypoint \
 && chmod 0755 /usr/local/bin/gpuprov-entrypoint

EXPOSE 22
ENTRYPOINT ["/usr/local/bin/gpuprov-entrypoint"]
