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
3. **CPU evidence.** When the contract does not decide the question (legacy instance, or compliant but without a future deadline), gasleak falls back to CloudWatch. Rule: `inactive`, whose severity scales with how long ago the last CPU activity was. Vetoed when `ExpiresAt` is still in the future, because the owner has already committed.

Two Low-severity warnings run in parallel with the severity-varying rules:
- `long_lived` fires when an instance has been around for a long time, regardless of current activity.
- `underutilized` fires when sustained p95 CPU stays below a threshold. Independent of recency. Answers "is this box oversized?" (right-size it) as opposed to "has this box gone quiet?" (kill it).

Verdicts are non-exclusive. A legacy box that's been around for years, went quiet last week, and has always had low p95 load can fire `non_compliant`, `long_lived`, `inactive`, and `underutilized` all at once. Each verdict points at a different action.

### The rules

| Rule | Fires when | Severity |
|---|---|---|
| `managed` | Instance carries `eks:cluster-name`, `aws:autoscaling:groupName`, or `aws:ec2spot:fleet-request-id`. **Pre-empts all other rules.** Hidden in `stale` output. | Info |
| `expired` | `ExpiresAt` is in the past. | High |
| `expiring_soon` | `ExpiresAt` is within 72 hours. The confirmation nudge. | Medium |
| `inactive` | Time since last CPU activity crosses configured thresholds. Below 7d = not flagged. 7–14d = Low. 14–30d = Medium. 30d+ or no active hour in window = High. Requires 168+ samples. Vetoed by a valid future `ExpiresAt`. | Low / Medium / High |
| `underutilized` | p95 CPU over the lookback window is below 2 % (configurable). Requires 168+ samples. Vetoed by a valid future `ExpiresAt`. Right-sizing warning. | Low |
| `long_lived` | `total_age` is at or above 90 days (configurable). Informational warning even when the instance is currently active. | Low |
| `non_compliant` | Any required contract tag missing or malformed. High when `ManagedBy=gasleak/*` is present (tampered). Low for legacy untagged. | Low / High |

### Severity and exit codes

Severity is a four-level enum (`Info`, `Low`, `Medium`, `High`) derived from the verdict type. The process exit code is the worst severity across the scan.

| Code | Meaning | Triggered by |
|---|---|---|
| `0` | Nothing actionable. | Only Info or Low verdicts. |
| `1` | Needs attention. | Any Medium verdict (`expiring_soon`, `inactive` at Medium severity). |
| `2` | Page someone. | Any High verdict (`expired`, tampered `non_compliant`, or `inactive` at High severity). |

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

### CPU signals: recency vs. sustained load

Two CPU-driven rules, answering two different questions:

- **`inactive` (severity: Low/Medium/High)** is driven by `last_active_at`: the most recent hour within the lookback whose `Maximum` CPU crossed 5 %. Severity scales with the gap to *now*. Answers "should we consider killing this box?"
- **`underutilized` (severity: Low)** is driven by p95 CPU over the lookback window. Fires when sustained load sits below 2 %. Answers "is this box oversized?"

They can both fire on the same instance and point at different actions. A box that was recently busy but whose p95 is 1 % suggests right-sizing to a smaller instance type, not termination. A box whose last activity was 40 days ago is a termination candidate regardless of p95.

`avg_cpu` and `max_cpu` are displayed for context but do not drive either rule. `p95_cpu` drives `underutilized` only.

### The lookback window

CloudWatch is queried for `inactive_high_days` of hourly data (default 30). The window has to be at least as long as the High threshold, otherwise gasleak cannot distinguish "inactive for 20 days" from "inactive for 60 days". Using the High threshold directly removes a knob.

### The samples gate

`inactive` refuses to fire unless CloudWatch returned at least **168 hourly data points** (7 days by default). One setting covers two concerns.

- **Instance maturity.** A box that has been up for 6 hours has not given us enough signal to declare it inactive.
- **Data quality.** A flaky or missing CloudWatch agent returns sparse data. Sparseness should not read as inactivity.

Missing metrics never get interpreted as low CPU. An instance whose agent is not reporting cannot land in `inactive`.

### `last_active` column

The `last_active` column answers "when did this box last do real work?" It is the most recent hour within the lookback whose hourly `Maximum` CPU crossed 5 %. The Slack reporter uses it to phrase the confirmation nudge naturally: "your instance has been idle for 27 days, last active on 2026-03-25".

| Display | Meaning |
|---|---|
| `Xd Yh ago` | We have data and found at least one active hour in the window. |
| `>30d ago` | We have data but no hour in the lookback window crossed 5 % Max CPU. The number matches `inactive_high_days`. |
| `no data` | CloudWatch returned zero samples. Agent missing, instance very new, or permissions gap. We do not know whether it is inactive. |
| `-` | CPU was not fetched, typically because the CloudWatch call failed. A warning is logged when this happens. |

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

Output columns: `instance_id`, `state`, `type`, `created`, `total_age`, `last_uptime`, `launched_by`, `src`, `region`, `p95_cpu`, `max_cpu`, `last_active`.

The `src` column shows how `launched_by` was resolved.

- `tag` means one of the owner-ish tags was set (`Owner`, `LaunchedBy`, and similar).
- `iam-role` means the fallback used the IAM instance profile's role name.
- `key-name` means the fallback used the SSH key-pair name.
- `unknown` means nothing matched.

### `gasleak stale`

Applies the rules, prints verdicts, exits with a severity-reflecting code. `managed` rows are hidden. No flags.

```
gasleak stale
```

Output columns: `sev`, `instance_id`, `type`, `created`, `total_age`, `last_uptime`, `last_active`, `p95_cpu`, `launched_by`, `verdicts`. Sorted by severity desc, then `total_age` desc. A one-line summary follows (`N flagged / M scanned, worst severity: …`).

`p95_cpu` is visible on every row regardless of whether `underutilized` fires, so you can spot borderline cases (e.g. p95 = 2.5 %, just above the 2 % default threshold) and decide whether to tighten the config.

Severity is driven by the data — specifically `last_active` time and `total_age` — so you don't need to pick a date to start escalating. A long-quiet box earns High severity on its own.

### `gasleak explain <instance-id>`

Debug view for a single instance. Prints the tag dump, the parsed `ContractView`, the CPU summary, and a full rule trace showing every rule with either the fired verdict or the reason it was skipped.

```
gasleak explain i-0abc123
```

Unlike `stale`, `explain` does not filter by state, so it works on stopped instances too. Exit code is always 0.

Example output:

```
Instance i-078f873c9dc0331e6 (us-east-1)
  state        : running
  type         : c5.4xlarge
  created      : 2025-02-12
  total_age    : 433d 13h 56m
  ...

Rule evaluation:
  managed        skipped no controller tags present
  expired        skipped ExpiresAt tag not set
  expiring_soon  skipped ExpiresAt tag not set
  idle           fired   idle(p95=0.8%, n=336)
  non_compliant  fired   non_compliant(missing=ManagedBy,Owner,OwnerSlack,ExpiresAt)

Summary: 2 verdict(s) fired, worst severity: LOW, flagged: yes
```

### Global

| Flag | Effect |
|---|---|
| `--config <PATH>` | Load this config file. Errors if the path does not exist. Overrides `$GASLEAK_CONFIG` and the default. |
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

### Config file

Optional. Three ways to point at a file, highest precedence first:

1. `--config <PATH>` CLI flag. Explicit. Errors if the file is missing.
2. `$GASLEAK_CONFIG` env var. Explicit. Errors if the file is missing.
3. `$HOME/.config/gasleak/gasleak.toml`. Default. Silently falls back to built-in defaults if the file is missing.

A file that exists but fails to parse is always a hard error. Unknown keys are ignored so future gasleak releases stay backward-compatible.

All keys are optional:

```toml
[inactive]
low_days    = 7            # below this, `inactive` does not fire
medium_days = 14           # at/above this, severity = Medium
high_days   = 30           # at/above this, severity = High. Also the CloudWatch lookback.
min_samples = 168          # data-quality floor (7 days of hourly data)

[underutilized]
p95_threshold_pct = 2.0    # below this, fires a Low warning. Vetoed by future ExpiresAt.

[long_lived]
age_days = 90              # at/above this, fires a Low warning regardless of activity

[warn]
window_hours = 72          # lead-time before ExpiresAt for `expiring_soon`
```

The CLI never gains flags for these tunables. If you need to tune a knob per-invocation, write a one-off TOML file and point `GASLEAK_CONFIG` at it.

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

Implemented today: `list`, `stale`, `explain`, contract parsing, all rules, severity-based exit codes, `total_age` / `last_uptime` / `last_active`, config file loading, single-region scanning.

Not implemented yet:

- `gasleak launch`. Contract-enforcing instance creation. Until this lands, stamping the contract tags is on whoever launches instances.
- `gasleak extend <instance-id> --for <duration>`. The confirmation mechanism that rewrites `ExpiresAt`.
- Multi-region parallel scan.
- JSON output for piping to `jq` or Slack formatters.
- Per-instance cost attribution in USD and AVAX.
- `long_stopped` verdict for stopped instances racking up EBS charges.
