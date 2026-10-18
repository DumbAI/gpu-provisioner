# Getting started

## Install

=== "From crates.io"

    ```bash
    cargo install gpu-provisioner      # installs the `gpuprov` binary
    ```

=== "From source"

    ```bash
    git clone https://github.com/DumbAI/gpu-provisioner
    cd gpu-provisioner
    cargo install --path .
    ```

Requirements: Rust 1.91.1 or newer, and the system OpenSSH client (`ssh`) on your `PATH`.
On macOS and Linux it is already there.

## Try it without a cloud account

The `docker` cloud runs each "VM" as a local sshd container. It's the quickest way
to see the whole lifecycle. Containers can't run k3s, so these come up as plain
VMs:

```bash
gpuprov provision --cloud docker --name demo --instance-type container
gpuprov ssh demo -- uname -a
gpuprov status demo        # the journaled state transitions
gpuprov down demo
```

## Your first cloud VM

1. **Credentials.** Use whatever each cloud's own tooling already uses. Details are
   in [Configuration](guides/configuration.md).
    - AWS: `aws configure` (or a profile in `~/.gpu-provisioner/config.toml`).
    - GCP: `gcloud auth login` and `gcloud config set project <id>`.
    - Prime Intellect: `prime login`, or `PRIME_API_KEY`.

2. **Provision.**

    ```bash
    # AWS: the cheapest T4 type in us-west-2, falling back to us-east-1
    gpuprov provision --name dev --gpus T4:1 --region us-west-2 --region us-east-1

    # GCP: an L4 in any zone of us-central1
    gpuprov provision --cloud gcp --name dev-gcp --gpus L4:1 --region us-central1

    # Prime Intellect: the cheapest H100 offer anywhere in the marketplace
    gpuprov provision --cloud primeintellect --name dev-pi --gpus H100:1
    ```

    Each VM is also a single-node k3s cluster by default. The command blocks until
    SSH works and the cluster is validated, then prints the kubeconfig path. Add
    `--no-k8s` for a plain VM. Progress goes to stderr and the summary to stdout.

3. **Use it.**

    ```bash
    gpuprov ssh dev                    # interactive shell
    gpuprov ssh dev -- nvidia-smi      # one command; the exit code propagates
    ssh dev                            # plain OpenSSH works too
    gpuprov list
    ```

4. **Tear it down.** VMs bill until you do.

    ```bash
    gpuprov down dev
    ```

## When things go wrong

- **Ctrl-C** during `provision` finishes the in-flight cloud call, then deletes what
  this run created. Press it twice to exit immediately. Re-running the same
  command then resumes from the journal.
- `gpuprov status <name>` shows the last operation and its recent transitions,
  including why each failed offer was skipped.
- A VM shown as `LEAKED` means teardown could not be confirmed. Run
  `gpuprov down <name>` again, or `gpuprov provision` with the same name, which
  cleans up first.
