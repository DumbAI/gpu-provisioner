# Kubernetes (k3s)

Every VM becomes a single-node [k3s](https://k3s.io) cluster **by default**: real
Kubernetes, managed by systemd on the node. You deploy apps with plain
`kubectl`. There is no custom controller; k3s itself is the reconciler.

```bash
gpuprov provision --name gpu-k8s --gpus L4:1 --cloud gcp --region us-central1
export KUBECONFIG=~/.gpu-provisioner/kube/gpu-k8s.yaml
kubectl get nodes
kubectl apply -f examples/gpu-test.yaml    # runs nvidia-smi in a pod requesting nvidia.com/gpu
kubectl logs job/gpu-test
```

## Turning it off

| Scope | How |
|---|---|
| one VM | `gpuprov provision --no-k8s ...`, or `kubernetes: false` in the spec file |
| everywhere | `[defaults] kubernetes = false` in `config.toml` (then `--k8s` opts a VM back in) |

On clouds that cannot host k3s (the Docker test cloud), the default is skipped
with a note. An explicit `--k8s` there is rejected before anything is created.

## How it gets installed

The cluster is a [state machine](../architecture/state-machines.md#45-k3s-cluster-machine-clusterk3srs)
layered on the VM launch machine. The installer can reach the node by two routes:

1. **Boot.** On clouds that boot with cloud-init (AWS, GCP), the installer rides in
   the VM's user data and runs while the VM boots, so the install overlaps the
   wait for SSH.
2. **SSH.** Once the VM is up, gpuprov checks what the boot run did (one SSH round
   trip: a status marker, `cloud-init status`, the k3s unit, GPU presence):

    | Found | Action |
    |---|---|
    | installed at boot, k3s running | skip straight to the API check |
    | boot install still running | wait (up to 15 minutes) |
    | boot install failed, never ran (no cloud-init, reused VM), or still running after 15 min | push the installer over SSH |
    | a GPU the boot run didn't see | push the installer over SSH to add the GPU runtime |

The two routes never race. The installer takes a host-wide lock, so an SSH run
waits for a boot run still holding it. The installer is also idempotent: it
writes `/etc/rancher/k3s/config.yaml` itself and restarts k3s only when that file
changes.

What the installer does: if `nvidia-smi` works, it installs `nvidia-container-toolkit`
(unless the image already has it), makes `nvidia` the default containerd runtime,
and applies the NVIDIA device plugin (pinned `v0.19.3`). Then it installs k3s.

After installing, gpuprov:

3. **waits for the API** (`/readyz`, up to 10 minutes);
4. **writes the kubeconfig**: it reads `/etc/rancher/k3s/k3s.yaml`, renames the
   cluster/user/context to `gpuprov-<name>`, points `server` at the access
   endpoint, and saves it with mode 0600;
5. **validates** on the node that the node is Ready, CoreDNS is ready, and (with a
   GPU) `nvidia.com/gpu` is allocatable. In direct mode it also checks that the API
   certificate covers the public host and that the API port is reachable from your
   machine. That last check catches firewall problems that on-node checks can't see.

If a cluster step fails, **the VM is kept** so you can debug it with `gpuprov ssh`
(the boot log is `/var/log/cloud-init-output.log`). Re-run the same `provision`
command to retry; it reuses the VM and re-checks the install. Or run
`gpuprov down` to remove it.

## Access modes

| Mode | When | Kubeconfig server | You run |
|---|---|---|---|
| Direct | the cloud can open ports (AWS, GCP) | `https://<public-ip>:6443` | nothing |
| SSH tunnel | the cloud can't (Prime Intellect) | `https://127.0.0.1:16443` | `gpuprov tunnel <name>` in another terminal |

The API certificate covers the public host and always covers `127.0.0.1`. At boot
the installer learns the public IP from the cloud's metadata service. If it
can't, validation notices that the certificate doesn't cover the host and re-runs
the installer once with it.

!!! warning "Direct mode exposure"
    In direct mode, ports 6443, 80, and 443 are open to `0.0.0.0/0`, now on every
    VM by default. API access is gated by the client certificate inside the
    kubeconfig, so treat that file as a secret, or use `--no-k8s` for VMs that don't
    need a cluster.
