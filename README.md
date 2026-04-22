# gasleak

A Rust CLI that surfaces stale AWS EC2 instances — who launched them, how long they've been running, whether they comply with an organisation-wide tagging contract, and whether recent CPU suggests they're doing real work.

`gasleak` is designed to be consumed both interactively and by an external cron/Slack reporter — it exits with differentiated status codes so automation can distinguish "page someone" from "nudge them" from "fine".

---

## How gasleak decides what's stale

There is no single "is this stale" test. Different instances are stale for different reasons, and we want the *reason* to drive routing (dev-box someone forgot about ≠ prod service that was supposed to be shut down last week). gasleak checks each instance against a short list of rules and emits a typed verdict for every rule that fires. Multiple verdicts can apply to the same instance.

### The three layers

Each instance is evaluated through three layers. Higher layers take precedence.

**Layer 1 — Exemption.** Is the instance owned by a controller (EKS worker node, Auto Scaling Group member, Spot fleet member)? If yes, gasleak gets out of the way — the controller owns lifecycle, and flagging individual members is noise. Verdict: `managed` (Info severity; hidden from the `stale` report by default).

**Layer 2 — Declarative tags.** A tagged instance tells us directly what it is. If the owner stamped `ExpiresAt=2026-05-01T00:00:00Z` at launch, no amount of CPU analysis is going to be more authoritative than that tag. gasleak parses the tagging contract (next section) and applies a handful of rules:

- `ExpiresAt` is in the past → `expired` (High).
- `ExpiresAt` is within 72 hours → `expiring_soon` (Medium) — the owner-confirmation nudge. The Slack reporter pings `OwnerSlack` with recent CPU context + last-active timestamp; owner extends (writes a new `ExpiresAt`) or lets it die.
- Any required contract tag is missing → `non_compliant`. High severity if `ManagedBy=gasleak/*` is present but other required tags are gone (tampered). Low severity for legacy instances with no `ManagedBy` tag at all; upgrades to High past `--migration-deadline`.

**Layer 3 — CPU evidence.** When the contract doesn't decide the question (either the instance is legacy, or it's compliant without a future deadline), we look at CloudWatch:

- At least 168 hourly CPU samples (7 days of data) AND p95 < 10 % → `idle` (Low).
- **Vetoed when `ExpiresAt` is set and still in the future.** The owner has already committed to a deadline — `expiring_soon` will handle the confirmation nudge when it gets close. Firing `idle` on top would just nag owners who've already confirmed.

### Legacy (untagged) instances

Most fleets start without the tagging contract in place. Any instance missing the `ManagedBy=gasleak/...` marker fires `non_compliant`. By default this stays at Low severity — the cron won't page on it. Once you're ready to enforce, pass `--migration-deadline <RFC3339>`; after that date, every still-untagged instance flips to High and the cron starts escalating.

### Why verdicts are non-exclusive

A 400-day-old legacy box with 0.1 % CPU fires **both** `non_compliant` and `idle`. That's deliberate — they're different problems with different fixes (migrate the tagging, or kill the box). Collapsing them to one verdict loses signal.

---

## The metrics, explained

### Age: `total_age` vs. `last_uptime`

gasleak shows two time-since measurements per instance, because they answer different questions:

- **`total_age`** — time since the instance was **originally created**, taken from the root EBS volume's `AttachTime`. Survives stop/start cycles. This is the honest answer to "how long has this thing been around?"
- **`last_uptime`** — time since the **most recent start**, from `DescribeInstances.LaunchTime`. Resets on stop/start.

When the two columns match, the instance has been running continuously since creation. When they diverge, you're looking at a box that was stopped and restarted at some point — the gap (`total_age - last_uptime`) is roughly how long ago the last restart was, plus any stopped time.

A concrete example from a real fleet:

```
created     total_age      last_uptime
2023-04-05  1112d 18h 23m  403d 9h 24m      # 3 years old, restarted ~403 days ago
2025-04-15  371d 11h 27m   11h 38m          # year-old box, restarted 12 hours ago
```

`total_age` is the field used for sort order in the `stale` report. `last_uptime` is shown for transparency but isn't used by any rule.

Instance-store (non-EBS) instances lack a root volume, so gasleak falls back to `LaunchTime` for `created_at`. Rare in modern fleets.

### CPU: p95 over 14 days

The `idle` rule uses the 95th percentile of hourly `CPUUtilization` over the last 14 days, not the average. Two reasons:

- **Average is noisy.** One unusually busy hour (a syscall storm, a misbehaving agent) can pull the mean up enough to hide an otherwise-idle box.
- **p95 answers the right question.** "For 95 % of the observed hours, CPU was below X %" — which is exactly how you'd describe a box that isn't really working.

We also record `max_pct` for display but don't use it in the rule — `max` is too sensitive to single outliers.

### The 168-sample gate

The `idle` rule refuses to fire unless CloudWatch returned at least **168 hourly data points** (7 days). One setting, two concerns:

- **Instance maturity.** A box that's been up for 6 hours hasn't given us enough signal to declare it idle.
- **Data quality.** An instance where the CloudWatch agent is flaky or missing returns sparse data — we'd rather say "can't tell" than read sparseness as idleness.

Missing metrics never get interpreted as low CPU. If the agent isn't reporting, the instance never lands in `idle`.

### `last_active`

Alongside aggregate CPU, the `last_active` column answers "when did this box last do real work?" — the most recent hour within the 14-day lookback where the hourly `Maximum` CPU exceeded 5 %. This gives the Slack reporter natural phrasing when nudging an owner: "your instance has been idle for 11 days, last active on 2026-04-10."

- `>14d ago` — we have CPU data for this instance, but no hour in the 14-day lookback window crossed 5 % Max CPU. Not "never ever" — bounded by the lookback, not the instance's full lifetime.
- `no data` — CloudWatch returned zero samples. Usually means the agent isn't reporting, the instance was just launched, or the caller role lacks CloudWatch permissions. We don't know whether it's idle.
- `-` — CPU wasn't fetched at all (e.g. `--no-cpu`).
- The threshold is "something real happened" rather than the stricter p95-based idle threshold — `max > 5 %` catches a single busy sample within an otherwise quiet hour.

### Severity, not a score

Verdicts carry a severity (`Info`, `Low`, `Medium`, `High`). gasleak doesn't assign a numeric "staleness score" — that would conflate categories that mean different things. The cron reporter routes based on severity (see exit codes below).

---

## The tagging contract

The contract is deliberately minimal. We care about three things: who owns the instance, how to reach them, and when they've committed it should die. That's four tags, and `ExpiresAt` is the only lever that actually drives policy.

A contract-compliant instance carries:

| Tag | Example | Purpose |
|---|---|---|
| `ManagedBy` | `gasleak/0.1.0` | Marks the instance as owned by the contract. Starts-with match on `gasleak/`. |
| `Owner` | `arn:aws:iam::123:user/tsvetan` | Attribution. Auto-stamped by `gasleak launch` from the real caller identity. |
| `OwnerSlack` | `@tsvetan` or `#team-payments` | Routing. Where the reporter sends confirmation nudges. |
| `ExpiresAt` | `2026-05-01T00:00:00Z` | The owner's declared deadline. RFC 3339, must be in the future. |

### The confirmation loop

1. Launch with `ExpiresAt=now+7d` → clean.
2. At day 5, `expiring_soon` fires → the Slack reporter pings `OwnerSlack` with recent CPU stats as context.
3. Owner decides:
   - **Still needs it:** runs a future `gasleak extend <id> --for 14d` subcommand that rewrites the `ExpiresAt` tag.
   - **Done with it:** ignores the nudge; `expired` fires on day 7, cron pages to confirm termination.
4. Long-lived instances without a valid future `ExpiresAt` keep firing `idle` / `non_compliant` until someone commits to a deadline. That's the only forcing function — there's no DND / opt-out tag.

A `gasleak launch` subcommand that stamps these tags atomically at `RunInstances` time is not yet implemented. Until then, the contract is something you apply manually at launch; `gasleak stale` will tell you every instance that doesn't meet it.

---

## Build

```
cargo build            # debug binary at target/debug/gasleak
cargo build --release  # target/release/gasleak
```

MSRV and lint rules are set in `clippy.toml`. `cargo clippy --all-targets -- -D warnings` stays green; `cargo test` passes.

---

## AWS credentials and region

Configure AWS via the standard environment variables before running `gasleak`:

```
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...
export AWS_SESSION_TOKEN=...   # only if you have temporary creds
export AWS_REGION=us-east-1
gasleak list
```

Multi-region scanning is not yet supported; set `AWS_REGION` per invocation.

---

## Subcommands

### `gasleak list`

Inventory of running EC2 instances with owner attribution, creation date, and age.

```
gasleak list
gasleak list --with-cpu
```

| Flag | Effect |
|---|---|
| `--with-cpu` | Also fetch 14-day CloudWatch CPU per instance. Incurs `GetMetricData` cost. |

The `src` column in the output shows how `launched_by` was resolved:

- `tag` — one of the preferred owner-ish tags was set (`Owner`, `LaunchedBy`, etc.).
- `iam-role` — fell back to the IAM instance profile's role name.
- `key-name` — fell back to the SSH key-pair name.
- `unknown` — nothing matched.

### `gasleak stale`

Applies the rules, prints verdicts, exits with a severity-reflecting code. CPU is fetched automatically; `Managed` rows are hidden.

```
gasleak stale
gasleak stale --no-cpu
gasleak stale --migration-deadline 2026-06-01T00:00:00Z
```

| Flag | Effect |
|---|---|
| `--no-cpu` | Skip CloudWatch entirely. The `idle` rule is silenced; a banner warns. |
| `--migration-deadline <RFC3339>` | After this date, `non_compliant` upgrades from Low to High severity. |

Output columns: `sev`, `instance_id`, `type`, `created`, `total_age`, `last_uptime`, `last_active`, `launched_by`, `verdicts`. Rows are sorted by severity desc, then `total_age` desc. A one-line summary follows: `N flagged / M scanned; worst severity: …`.

When `--migration-deadline` is not set, gasleak emits a warning so the "non-compliant stays at Low forever" behaviour is never silent.

---

## Rules at a glance

| Rule | Fires when | Severity |
|---|---|---|
| `managed` | Instance carries an `eks:cluster-name`, `aws:autoscaling:groupName`, or `aws:ec2spot:fleet-request-id` tag. **Pre-empts all other rules.** Hidden in `stale` output. | Info |
| `expired` | `ExpiresAt` is in the past. | High |
| `expiring_soon` | `ExpiresAt` is within 72 hours. The confirmation nudge. | Medium |
| `idle` | ≥168 CPU samples AND p95 < 10 %. **Vetoed by a valid future `ExpiresAt`** — owner already committed to a deadline. | Low |
| `non_compliant` | Any required contract tag missing/malformed. High if `ManagedBy=gasleak/*` is present (tampered); Low for legacy instances with no `ManagedBy`, upgrading to High past `--migration-deadline`. | Low / High |

---

## Exit codes

The cron / Slack reporter routes on exit code:

| Code | Meaning | Triggered by |
|---|---|---|
| `0` | Nothing actionable. | Only `Info` or `Low` verdicts. |
| `1` | Needs attention. | Any `Medium` verdict — currently only `expiring_soon`. |
| `2` | Page someone. | Any `High` verdict (`expired`, tampered `non_compliant`, post-deadline `non_compliant`). |

---

## Example scenarios

### 1. Daily inventory

```
gasleak list --with-cpu
```

Every running instance in the configured region with owner attribution and 14-day average/max CPU. Answers "what's actually running and who owns it?"

### 2. What's stale right now?

```
gasleak stale
```

CPU is fetched automatically. `Managed` rows are hidden. Exit 0 as long as no instance is `expired`, over-tier, or contract-broken.

### 3. Fast path without CPU

```
gasleak stale --no-cpu
```

Skips CloudWatch — the `idle` rule is silenced but all the declarative rules (`expired`, `expiring_soon`, `non_compliant`) still fire. Useful when the caller role lacks `cloudwatch:GetMetricData`.

### 4. Roll out the tagging contract

Day 1, contract is new; nothing in the fleet complies:

```
gasleak stale
# exit 0: everything is LOW severity, cron stays quiet
```

Six weeks later, you're ready to enforce:

```
gasleak stale --migration-deadline 2026-06-01T00:00:00Z
# exit 2: every still-untagged instance flips to HIGH
```

### 5. Cron integration

```
#!/bin/sh
OUTPUT=$(gasleak stale --migration-deadline 2026-06-01T00:00:00Z)
CODE=$?

case $CODE in
  0) ;;                                      # silent
  1) post_slack "#ops-nudge"      "$OUTPUT"  ;;
  2) post_slack "#ops-incidents"  "$OUTPUT"  ;;
esac
```

---

## IAM permissions

Minimal policy:

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

`cloudwatch:GetMetricData` can be omitted if you only ever run with `--no-cpu`.

---

## What's not implemented yet

- `gasleak launch` — contract-enforcing instance creation. Until this lands, stamping the contract tags is on whoever launches instances.
- `gasleak explain <instance-id>` — per-instance rule trace.
- Multi-region parallel scan.
- JSON output (for piping to `jq` / Slack formatters).
- Per-instance cost attribution in USD and AVAX.
- Config file support — tunables (CPU threshold, sample count, warn window) are currently compile-time defaults. Override them in `staleness.rs::Config::defaults()` if you need different numbers.
- `long_stopped` verdict for stopped instances racking up EBS charges.

---

## Troubleshooting

**`RequestExpired` / `credential provider was not enabled`**: your AWS credentials are missing or have expired. Re-auth via SSO/your broker and re-export env vars.

**Nothing shows up**: gasleak only scans the region in `AWS_REGION`. Run with `-v` to see the region it's talking to in the log output.

**EKS nodes in the `stale` output**: they should fire `managed(eks)` and be hidden by default. If they appear in the flagged section, the instance is missing both `eks:cluster-name` *and* `aws:autoscaling:groupName` tags, which is unusual. Check raw tags with `gasleak list`.

**Every instance shows `non_compliant` with Low severity and exit 0**: you haven't set `--migration-deadline`. That's by design — there's no date to enforce against yet. Pass a deadline to start escalating.
