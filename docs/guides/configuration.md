# Configuration

## Local state

Everything lives under `~/.gpu-provisioner`, or under `$GPUPROV_HOME` when set:

```
~/.gpu-provisioner/
├── config.toml        # optional CLI config (below)
├── state.db           # SQLite: VM rows + state-machine journal
├── keys/              # the shared ed25519 keypair (generated on first use)
├── ssh/<name>         # per-VM ssh config, Include'd from ~/.ssh/config
├── sockets/           # ssh ControlMaster sockets
└── kube/<name>.yaml   # kubeconfigs of --k8s clusters (0600)
```

## `config.toml`

All sections are optional. Unknown keys are rejected.

```toml
[aws]
profile = "default"            # or explicit static credentials:
# access_key_id = "..."
# secret_access_key = "..."
# session_token = "..."        # optional

[gcp]
project = "my-project"         # else $GOOGLE_CLOUD_PROJECT / gcloud config
# access_token = "..."         # else $GOOGLE_OAUTH_ACCESS_TOKEN / gcloud auth print-access-token
regions = ["us-central1"]      # default failover order for --cloud gcp

[primeintellect]
api_key = "..."                # else $PRIME_API_KEY, else ~/.prime/config.json
# team_id = "..."              # bill pods to a team
# regions = ["united_states"]  # default region filter (empty = anywhere)

[docker]
# image = "my/sshd:tag"        # default: built locally from an embedded Dockerfile

[defaults]
regions = ["us-west-2", "us-east-1"]   # AWS failover order when the spec has none
disk_gb = 100
ssh_user = "ubuntu"
kubernetes = true                      # every VM becomes a k3s node; false = plain VMs
```

## Precedence

For a launch, values come from CLI flags first, then the spec YAML, then the
config file. That includes the k3s choice: `--k8s`/`--no-k8s`, then `kubernetes:`
in the spec, then `[defaults] kubernetes`. Region names are cloud-specific, so each cloud has its own default
region list. AWS uses `[defaults] regions`; the other clouds use their own section.

Whether an empty region list is allowed is the cloud's call. AWS and GCP require
at least one region. Prime Intellect treats an empty list as "anywhere in the
marketplace".

## Credentials by cloud

| Cloud | Resolution order |
|---|---|
| AWS | static keys in `[aws]`, then `[aws] profile`, then the default AWS chain (env vars, `~/.aws`, IMDS) |
| GCP | `[gcp] access_token`, then `$GOOGLE_OAUTH_ACCESS_TOKEN`, then `gcloud auth print-access-token` (cached, refreshed before expiry). Project: `[gcp] project`, then `$GOOGLE_CLOUD_PROJECT`/`$CLOUDSDK_CORE_PROJECT`, then `gcloud config get-value project` |
| Prime Intellect | `[primeintellect] api_key`, then `$PRIME_API_KEY`, then the prime CLI's `~/.prime/config.json` |
| Docker | the local Docker daemon (`docker info` must work) |
