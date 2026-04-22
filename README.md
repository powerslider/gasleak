# gasleak

A Rust CLI that surfaces stale AWS EC2 instances. For each running instance it attributes an owner, computes its creation date and current age, reads recent CPU from CloudWatch, and applies a small set of rules to decide whether it deserves attention.

`gasleak` is built for two audiences. A human typing `gasleak stale` sees an ordered table of what needs cleaning up. A cron job running the same command reads the exit code (0, 1, 2) and routes accordingly. The tool never terminates instances on its own.

---

## Quickstart

```
cargo build --release

export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...            # only if using temporary creds
export AWS_REGION=us-east-1

./target/release/gasleak stale
```

That prints one row per running instance that matched a rule, with severity, age, owner, and the verdicts that fired. Exit code reflects the worst severity.

---

## How gasleak decides

There is no single "is this stale" test. Different instances are stale for different reasons, and the reason needs to drive routing. A forgotten dev box is a quiet nudge. A prod service past its declared deadline is a page.

### The three-layer model

Each instance is evaluated against three layers. Higher layers take precedence.

1. **Exemption.** An EKS worker node, Auto Scaling Group member, or Spot fleet member is owned by a controller. gasleak gets out of the way. Verdict: `managed` (Info severity, hidden from `stale` output by default).
2. **Declarative tags.** A correctly tagged instance tells us directly what it is. If the owner stamped `ExpiresAt=2026-05-01T00:00:00Z` at launch, no CPU analysis is going to be more authoritative than that tag. Rules: `expired`, `expiring_soon`, `non_compliant`.
3. **CPU evidence.** When the contract does not decide the question (legacy instance, or compliant but without a future deadline), gasleak falls back to CloudWatch. Rule: `idle`. Vetoed when `ExpiresAt` is still in the future, because the owner has already committed.

Verdicts are non-exclusive. A long-lived legacy box with low CPU fires both `non_compliant` and `idle`, because those are different problems with different fixes.

### The rules

| Rule | Fires when | Severity |
|---|---|---|
| `managed` | Instance carries `eks:cluster-name`, `aws:autoscaling:groupName`, or `aws:ec2spot:fleet-request-id`. **Pre-empts all other rules.** Hidden in `stale` output. | Info |
| `expired` | `ExpiresAt` is in the past. | High |
| `expiring_soon` | `ExpiresAt` is within 72 hours. The confirmation nudge. | Medium |
| `idle` | 168 or more hourly CPU samples AND p95 below 10 %. Vetoed by a valid future `ExpiresAt`. | Low |
| `non_compliant` | Any required contract tag missing or malformed. High when `ManagedBy=gasleak/*` is present (tampered). Low for legacy untagged, upgrading to High past `--migration-deadline`. | Low / High |

### Severity and exit codes

Severity is a four-level enum (`Info`, `Low`, `Medium`, `High`) derived from the verdict type. The process exit code is the worst severity across the scan.

| Code | Meaning | Triggered by |
|---|---|---|
| `0` | Nothing actionable. | Only Info or Low verdicts. |
| `1` | Needs attention. | Any Medium verdict (currently only `expiring_soon`). |
| `2` | Page someone. | Any High verdict (`expired`, tampered `non_compliant`, post-deadline `non_compliant`). |

---

## Metrics

### Age: `total_age` vs `last_uptime`

gasleak shows two time-since values per instance because they answer different questions.

- **`total_age`** comes from the root EBS volume's `AttachTime`. It is the time since the instance was originally created and survives stop/start cycles. The honest answer to "how long has this thing been around?"
- **`last_uptime`** comes from `DescribeInstances.LaunchTime`. It is the time since the most recent start and resets on stop/start.

When the two match, the instance has been running continuously since creation. When they diverge, you are looking at a box that was stopped and restarted.

```
created     total_age      last_uptime
2023-04-05  1112d 18h 23m  403d 9h 24m     # 3 years old, restarted ~403 days ago
2025-04-15  371d 11h 27m   11h 38m         # year-old box, restarted 12 hours ago
```

`total_age` is the sort key for `stale`. Neither column drives a rule on its own.

Instance-store (non-EBS) instances lack a root volume, so gasleak falls back to `LaunchTime` for `created_at`. Rare in modern fleets.

### CPU: p95 over 14 days

The `idle` rule uses the 95th percentile of hourly `CPUUtilization` over the last 14 days, not the average.

- **Average is noisy.** One unusually busy hour pulls the mean up enough to hide an otherwise idle box.
- **p95 answers the right question.** "For 95 % of the observed hours, CPU was below X %" is exactly how you would describe a box that is not really working.

`max_pct` is displayed for context but never drives the rule. `max` is too sensitive to single outliers.

### The 168-sample gate

`idle` refuses to fire unless CloudWatch returned at least **168 hourly data points** (7 days). One setting covers two concerns.

- **Instance maturity.** A box that has been up for 6 hours has not given us enough signal to declare it idle.
- **Data quality.** A flaky or missing CloudWatch agent returns sparse data. Sparseness should not read as idleness.

Missing metrics never get interpreted as low CPU. An instance whose agent is not reporting cannot land in `idle`.

### `last_active`

The `last_active` column answers "when did this box last do real work?" It is the most recent hour within the 14-day lookback whose hourly `Maximum` CPU crossed 5 %. The Slack reporter uses it to phrase the confirmation nudge naturally: "your instance has been idle for 11 days, last active on 2026-04-10".

| Display | Meaning |
|---|---|
| `Xd Yh ago` | We have data and found at least one active hour in the window. |
| `>14d ago` | We have data but no hour in the 14-day window crossed 5 % Max CPU. Bounded by the lookback, not the instance's full lifetime. |
| `no data` | CloudWatch returned zero samples. Agent missing, instance very new, or permissions gap. We do not know whether it is idle. |
| `-` | CPU was not fetched, typically because the CloudWatch call failed. A warning is logged when this happens. |

The 5 % threshold catches "something real happened", distinct from the stricter p95 threshold the rule uses.

---

## The tagging contract

The contract is deliberately minimal. Four tags, one lever (`ExpiresAt`).

| Tag | Example | Purpose |
|---|---|---|
| `ManagedBy` | `gasleak/0.1.0` | Marks the instance as owned by the contract. Starts-with match on `gasleak/`. |
| `Owner` | `arn:aws:iam::123:user/tsvetan` | Attribution. Auto-stamped by `gasleak launch` from the real caller identity. |
| `OwnerSlack` | `@tsvetan` or `#team-payments` | Routing. Where the reporter sends confirmation nudges. |
| `ExpiresAt` | `2026-05-01T00:00:00Z` | The owner's declared deadline. RFC 3339, must be in the future. |

### The confirmation loop

1. Launch with `ExpiresAt=now+7d`. The instance is clean.
2. At day 5, `expiring_soon` fires. The Slack reporter pings `OwnerSlack` with recent CPU stats as context.
3. Owner decides.
   - **Still needs it.** Runs a future `gasleak extend <id> --for 14d` subcommand that rewrites the `ExpiresAt` tag.
   - **Done with it.** Ignores the nudge. `expired` fires on day 7 and the cron pages to confirm termination.
4. An instance without a valid future `ExpiresAt` keeps firing `idle` and `non_compliant` until someone commits to a deadline. That is the only forcing function. There is no DND opt-out tag.

A `gasleak launch` subcommand that stamps these tags atomically at `RunInstances` time is not yet implemented. Until then, the contract is something you apply manually at launch. `gasleak stale` will tell you every instance that does not meet it.

---

## CLI reference

Both subcommands always fetch 14-day CloudWatch CPU per instance. If the fetch fails (missing IAM permission, network error), gasleak logs a warning and continues with CPU fields rendered as `-`. The `idle` rule will not fire in that case.

### `gasleak list`

Inventory of running EC2 instances with owner attribution, creation date, age, and 14-day CPU activity.

```
gasleak list
```

Output columns: `instance_id`, `state`, `type`, `created`, `total_age`, `last_uptime`, `launched_by`, `src`, `region`, `avg_cpu`, `max_cpu`, `last_active`.

The `src` column shows how `launched_by` was resolved.

- `tag` means one of the owner-ish tags was set (`Owner`, `LaunchedBy`, and similar).
- `iam-role` means the fallback used the IAM instance profile's role name.
- `key-name` means the fallback used the SSH key-pair name.
- `unknown` means nothing matched.

### `gasleak stale`

Applies the rules, prints verdicts, exits with a severity-reflecting code. `managed` rows are hidden.

```
gasleak stale
gasleak stale --migration-deadline 2026-06-01T00:00:00Z
```

| Flag | Effect |
|---|---|
| `--migration-deadline <RFC3339>` | After this date, legacy `non_compliant` upgrades from Low to High severity. |

Output columns: `sev`, `instance_id`, `type`, `created`, `total_age`, `last_uptime`, `last_active`, `launched_by`, `verdicts`. Sorted by severity desc, then `total_age` desc. A one-line summary follows (`N flagged / M scanned, worst severity: …`).

When `--migration-deadline` is unset and at least one legacy instance is present, `stale` logs a warning so the "Low forever" behaviour is never silent.

### Global

| Flag | Effect |
|---|---|
| `-v`, `-vv` | Verbosity. `-v` enables info logs, `-vv` enables debug. |

---

## Operations

### AWS credentials and region

gasleak uses the standard AWS SDK environment variables.

```
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...   # only if using temporary creds
export AWS_REGION=us-east-1
```

Multi-region scanning is not yet implemented. Set `AWS_REGION` per invocation.

### IAM policy

Minimum for `list` and `stale`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "ec2:DescribeInstances",
        "cloudwatch:GetMetricData"
      ],
      "Resource": "*"
    }
  ]
}
```

Both actions are required. `ec2:DescribeInstances` drives the scan, and `cloudwatch:GetMetricData` powers the CPU activity columns and the `idle` rule. If the CloudWatch permission is missing, gasleak logs a warning and continues with empty CPU data.

### Cron integration

```sh
#!/bin/sh
OUTPUT=$(gasleak stale --migration-deadline 2026-06-01T00:00:00Z)
CODE=$?

case $CODE in
  0) ;;                                      # silent
  1) post_slack "#ops-nudge"      "$OUTPUT"  ;;
  2) post_slack "#ops-incidents"  "$OUTPUT"  ;;
esac
```

### Build

```
cargo build            # debug binary at target/debug/gasleak
cargo build --release  # target/release/gasleak
```

MSRV and lint rules are set in `clippy.toml`. `cargo clippy --all-targets -- -D warnings` stays green. `cargo test` passes.

### Troubleshooting

**`RequestExpired` or `credential provider was not enabled`.** AWS credentials are missing or have expired. Re-auth via SSO or your broker and re-export env vars.

**Nothing shows up.** gasleak only scans the region in `AWS_REGION`. Run with `-v` to see the region it is talking to in the log output.

**EKS nodes in the `stale` output.** They should fire `managed(eks)` and be hidden by default. If they appear in the flagged section the instance is missing both `eks:cluster-name` and `aws:autoscaling:groupName` tags, which is unusual. Inspect raw tags with `gasleak list`.

**Every instance shows `non_compliant` with Low severity and exit 0.** No `--migration-deadline` is set. That is by design. There is no date to enforce against yet. Pass a deadline to start escalating.

---

## Status

Implemented today: `list`, `stale`, contract parsing, all rules, severity-based exit codes, `total_age` / `last_uptime` / `last_active`, single-region scanning.

Not implemented yet:

- `gasleak launch`. Contract-enforcing instance creation. Until this lands, stamping the contract tags is on whoever launches instances.
- `gasleak extend <instance-id> --for <duration>`. The confirmation mechanism that rewrites `ExpiresAt`.
- `gasleak explain <instance-id>`. Per-instance rule trace.
- Multi-region parallel scan.
- JSON output for piping to `jq` or Slack formatters.
- Per-instance cost attribution in USD and AVAX.
- Config file support. Tunables (CPU threshold, sample count, warn window) are compile-time defaults today. Override them in `staleness.rs::Config::defaults()` if you need different numbers.
- `long_stopped` verdict for stopped instances racking up EBS charges.
