# Docker (local test cloud)

The `docker` cloud runs each "VM" as a long-running sshd container on your
local Docker daemon. It exists so the **real** state machines (journaling,
failover bookkeeping, SSH probing, reuse, crash adoption, teardown) can run end
to end without a cloud account. CI uses it the same way.

```bash
gpuprov provision --cloud docker --name demo --instance-type container
gpuprov ssh demo -- 'whoami && sudo -n true && echo ok'
gpuprov down demo
```

| Primitive | Docker |
|---|---|
| plan | one offer in region `local` (GPU asks are rejected) |
| prepare | check the daemon; build the embedded sshd image on first use (its tag includes the Dockerfile hash) |
| create | `docker run -d`, labeled `gpuprov-vm-name=<name>`, SSH published on `127.0.0.1:<random port>` |
| observe | `docker ps -a` by label, plus `docker inspect` for status and mapped port |
| delete | `docker rm -f` |

The container's entrypoint gives the same login contract that cloud-init gives a
real VM: `$SSH_USER` with passwordless sudo and the public key authorized.

Containers don't run systemd, so `--k8s` cannot install k3s here. Use this cloud
for VM-lifecycle testing.
