# Health checks

`gpuprov health` answers one question: **is this VM still there and usable, or
has the vendor stopped, preempted, or reclaimed it?** It is read-only, so it's
safe to run from cron, CI, or a monitor.

```console
$ gpuprov health dev
dev  HEALTHY  aws  us-west-2  i-0abc…  ubuntu@54.190.1.2:22  (judged by vendor)
  ✓ record   UP at us-west-2
  ✓ vendor   running (i-0abc…)
  ! vendor   spot instance (AWS may reclaim it with a 2-minute notice)
  ✓ ping     54.190.1.2 answers ICMP echo
  ✓ network  54.190.1.2:22 accepts connections
  ✓ ssh      login ok as ubuntu
  - gpu      pass --deep
  - k3s      pass --deep

$ gpuprov health dev
dev  GONE  aws  us-west-2  i-0abc…  (judged by vendor)
  ✓ record   UP at us-west-2
  ✗ vendor   aws no longer has i-0abc… — terminated or reclaimed
  ✗ vendor   terminated by AWS: spot capacity reclaimed
```

## Usage

```bash
gpuprov health                  # every live VM
gpuprov health dev              # by VM name
gpuprov health i-0abc123        # by instance id
gpuprov health 54.190.1.2       # by IP (a recorded VM's, or any address)
gpuprov health dev --deep       # also nvidia-smi and k3s health
gpuprov health --json           # machine-readable reports
```

| Exit code | Worst verdict |
|---|---|
| 0 | all healthy |
| 1 | degraded |
| 2 | unhealthy or gone |
| 3 | unknown |

## Verdicts

| Verdict | Meaning |
|---|---|
| **HEALTHY** | running at the vendor and usable |
| **DEGRADED** | running, but unreachable, SSH or a deep check failing, replaced by another instance, moved to a new address, or flagged by the vendor (scheduled retirement, maintenance) |
| **UNHEALTHY** | the vendor reports it stopped, failed, or being deleted: typically a preemption, eviction, crash, or failed status check |
| **GONE** | the vendor no longer has it (terminated or reclaimed), or `gpuprov down` removed it |
| **UNKNOWN** | neither the vendor nor the network could tell |

## How it decides

The check climbs a ladder. A later rung is skipped once an earlier one fails.

1. **record**: gpuprov's local record (location, instance id, address).
2. **vendor**: the cloud adapter's `diagnose`. Every cloud reports whether the
   instance still exists and its normalized status. Clouds with richer health
   APIs add vendor-native findings, as the table below shows.
3. **ping**: ICMP echo. It's informational when the vendor answered, because
   clouds commonly block ICMP.
4. **network**: a TCP connect to the SSH port.
5. **ssh**: gpuprov's key logs in.
6. **gpu** and **k3s** (`--deep`): `nvidia-smi -L` sees the GPUs, and the
   single-node cluster is healthy (node Ready, CoreDNS, GPU allocatable).

**Fallback: generic IP checks.** When the vendor can't be asked (an API or
credential error, or an IP address gpuprov has no record of), the verdict rests
on ping, TCP, and SSH alone. The report then says `judged by network`. Such a
check can confirm a VM is reachable and that gpuprov's key works. It cannot tell
a host that is merely down from one that was reclaimed, so a silent host is
UNKNOWN, not GONE.

## Vendor-native signals

| Cloud | Signals beyond "exists + status" |
|---|---|
| AWS | why it stopped or was terminated (`StateReason`, e.g. "terminated by AWS: spot capacity reclaimed"), counting recently terminated instances; system and instance status checks (`DescribeInstanceStatus`); scheduled retirement, reboot, and maintenance events; spot lifecycle |
| GCP | preemption of spot/preemptible VMs (with `lastStopTimestamp`), upcoming maintenance windows, shutdowns in progress, status messages |
| Lambda | `unhealthy` (and since when it was last healthy) and `preempted` |
| DigitalOcean | powered-off Droplets (which keep billing) |
| Verda | spot evictions (`discontinued`) and `notfound`, failed installs and `no_capacity`, spot instances |
| others | existence and normalized status (the default) |

Adding signals for another cloud means overriding one method,
`CloudProvider::diagnose`. See [Adding a cloud](../architecture/adding-a-cloud.md#health-diagnosis-optional).

## Library use

```rust
use gpu_provisioner::vm::health::{self, HealthOptions, SystemProbes, Target};

match health::resolve(&db, "dev")? {
    Target::Vm(record) => {
        let report = health::check(Some(&*provider), &record, &SystemProbes, HealthOptions::default()).await;
        println!("{} is {}", report.name, report.verdict);
    }
    Target::Address(ip) => {
        let report = health::check_address(&ip, 22, &SystemProbes).await;
        println!("{ip} is {}", report.verdict);
    }
}
```

`HealthReport` is serializable (it's what `--json` prints). The probes are a
trait (`HealthProbes`), so the check can run against simulated hosts in tests.
