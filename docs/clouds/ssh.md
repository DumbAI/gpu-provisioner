# SSH hosts (your own machines)

The `ssh` cloud uses machines you already have: a workstation, a lab server, a
DGX Spark. Each configured host is a region of its own. Provisioning **adopts**
the host for the VM, and `gpuprov down` **gives it back**. Nothing is bought,
and the machine is never powered off. Everything else (SSH waits, the k3s
cluster with its GPU runtime, health, kubeconfig, tunnels) works as on any
cloud.

```bash
gpuprov provision --cloud ssh --name dev --gpus GB10:1     # any host offering GB10
gpuprov provision --cloud ssh --name dev --region spark    # that host
gpuprov ssh dev -- nvidia-smi
gpuprov down dev                                           # uninstalls k3s, gives the host back
```

## Configure a host

```toml
[ssh.hosts.spark]                    # the id: what --region names
host = "spark.local"                 # address or name answering SSH
user = "ryan"                        # needs passwordless sudo
port = 22                            # default 22
identity_file = "~/.ssh/id_ed25519"  # a key that logs `user` in
gpus = "GB10:1"                      # what --gpus matches; omit for a CPU host
# keep_k3s = true                    # leave k3s installed on down (faster next start)
```

The host needs:

- **Linux with systemd**: k3s runs as a service;
- **passwordless sudo for `user`**: for example a file in
  `/etc/sudoers.d/` with `ryan ALL=(ALL) NOPASSWD:ALL`;
- **no k3s that gpuprov did not install**: the installer would reconfigure
  and restart it, so the host is refused instead;
- for a GPU, the NVIDIA driver (`nvidia-smi` works). The installer adds
  `nvidia-container-toolkit` with apt if it is missing; DGX OS ships it.

## How it maps

| Primitive | SSH host (with the host's own key) |
|---|---|
| plan | one offer per configured host that fits the ask: `--gpus` against `gpus` (same name, at least that many), `--instance-type` against the host id or its type (`GB10:1`, `cpu`), `--region` restricting to named hosts. Offers are in host id order |
| prepare | nothing |
| create | check passwordless sudo, systemd and foreign k3s; **claim** the host for the VM in `/var/lib/gpuprov/ssh-claim` (created exclusively: of two concurrent claims, one wins); add gpuprov's public key to `user`'s `authorized_keys` |
| observe | read the claim. If it names this VM, the instance (id = the host id) is running at the configured host, port and user |
| delete | if the claim is this VM's: run `k3s-uninstall.sh` (unless `keep_k3s`), remove the key the claim added (a key already authorized before is kept), drop the claim |
| cleanup | nothing |

After `create`, the host is reached with gpuprov's own key, like every
VM: the launch, the k3s installer, `gpuprov ssh` and embedders use it
unchanged. The host's own key is used only to adopt and release.

**One VM per host.** A claim by another VM makes `create` fail as
`Capacity`, so failover tries the next matching host; with none left, the
launch is exhausted. `gpuprov down` of the claiming VM frees the host.

**Errors:** an unreachable host at `create` is `Unavailable` (nothing was
claimed; the next host is tried), and transient afterwards; `Permission
denied` is `Credentials`; a missing sudo, systemd or a foreign k3s is
`InvalidRequest`.

## Capabilities

| k3s | k8s API access | `--port` | `--spot` | `--image` | Login | Per-VM side resources |
|---|---|---|---|---|---|---|
| ✓ over SSH | SSH tunnel | ✗ (gpuprov does not manage your firewall) | ✗ | ✗ (the machine keeps its OS) | the configured user and port | the claim file and gpuprov's authorized key |

## Notes

- **Host keys are not pinned**, as for every cloud (`StrictHostKeyChecking=no`):
  use it on networks you trust, or reach the host through a VPN.
- **What the VM's workload leaves on the disk stays.** Releasing removes k3s
  (and with it the cluster's containers and images) but not files a
  workload wrote to host paths. Clean those up as the workload requires.
- **The machine must stay reachable.** `observe` and `delete` need SSH; a host
  that disappears for good leaves its VM `LEAKED` until it answers again or
  its row is removed.
