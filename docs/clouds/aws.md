# AWS

```bash
gpuprov provision --name dev --gpus A10G:1 --region us-east-2 --region us-west-2
```

## Planning

A GPU ask maps to a ladder of EC2 types, cheapest first: same GPU, more
CPU and RAM as you go up. The ladder is crossed **type-major** with your
regions, so the cheapest type is tried in every region before a bigger one.

| GPU | count → types |
|---|---|
| T4 | 1 → g4dn.xlarge … g4dn.16xlarge · 4 → g4dn.12xlarge · 8 → g4dn.metal |
| A10G | 1 → g5.xlarge … g5.16xlarge · 4 → g5.12xlarge, g5.24xlarge · 8 → g5.48xlarge |
| L4 | 1 → g6.xlarge … g6.16xlarge · 4 → g6.12xlarge, g6.24xlarge · 8 → g6.48xlarge |
| L40S | 1 → g6e.xlarge … g6e.16xlarge · 4 → g6e.12xlarge, g6e.24xlarge · 8 → g6e.48xlarge |
| A100 · A100-80GB · H100 · H200 | 8 → p4d.24xlarge · p4de.24xlarge · p5.48xlarge · p5e.48xlarge |

## Prepare (per offer)

Checks run cheapest first, so a region that isn't viable fails before anything is
created:

1. Instance-type metadata (GPU, architecture) and the zones that actually offer the type.
2. AMI: the Ubuntu / Deep Learning AMI matching GPU and architecture, resolved through
   SSM public parameters (or `--image`). Its root device and minimum disk size are validated.
3. Public subnets in the default VPC, restricted to the offering zones.
4. A per-VM security group (`gpuprov-<name>`) opening 22 plus requested ports.

Default AMIs, keyed on the instance type's live metadata:

| | x86_64 | arm64 |
|---|---|---|
| CPU | Canonical Ubuntu 22.04 | Canonical Ubuntu 22.04 (arm64) |
| GPU | Deep Learning Base AMI (OSS NVIDIA driver) | Deep Learning Base AMI arm64 (OSS NVIDIA driver) |

Pre-Turing GPUs (K80/V100/M60) are not supported by the OSS driver; pass
`--image` for those. A `--disk` smaller than the AMI's snapshot is rejected with
the required size.

## Create / observe / delete

- `RunInstances` uses a no-retry client and walks the zones' subnets on capacity
  errors. Instances use IMDSv2 and gp3 root disks, and are tagged
  `gpuprov-vm-name=<name>` and `gpuprov-managed=true`. The SSH key travels in
  cloud-init; no EC2 key pairs are created.
- `observe` uses `DescribeInstances` filtered by the identity tag. `delete` is
  `TerminateInstances` (a not-found instance counts as success). `cleanup` deletes
  the security group, retrying while ENIs detach.

## Error mapping

| AWS code | Kind | Effect |
|---|---|---|
| `InsufficientInstanceCapacity`, `InsufficientCapacity`, … | Capacity | compensate, next offer |
| `VcpuLimitExceeded`, `InstanceLimitExceeded`, `MaxSpotInstanceCountExceeded` | Quota | skip the **whole region** |
| `Unsupported` | Unavailable | skip this type in the region |
| `RequestLimitExceeded`, `InternalError`, … | Transient | retry with backoff |
| `AuthFailure`, `ExpiredToken`, … | Credentials | abort (after compensating) |

## IAM permissions

The caller needs `ec2:Describe*`, `ec2:RunInstances`, `ec2:TerminateInstances`,
`ec2:CreateTags`, `ec2:CreateSecurityGroup`, `ec2:DeleteSecurityGroup`,
`ec2:AuthorizeSecurityGroupIngress`, and `ssm:GetParameter` (public AMI
parameters). The VM itself gets **no** IAM role.
