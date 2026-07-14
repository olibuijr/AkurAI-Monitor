---
name: akurai-monitor
description: Operates AkurAI Monitor on EC2 through akurai-monitorctl. Use for health, hosts, applications, metrics, logs, alert rules/events, recipients, webhook channels, Pi-bun agent channels, and channel tests.
compatibility: Runs on Titan and forwards to the AkurAI EC2 VM through the maintained akurai-ec2 CLI.
license: MIT
---

# AkurAI Monitor operations

Use `akurai-monitorctl` for every monitor read and mutation. Do not edit `/var/lib/rust-monitor/monitor.db`, `/etc/rust-monitor.env`, systemd units, or Nginx directly. The canonical source is `~/Projects/AkurAI-Monitor`; production is `https://monitor.olibuijr.com`, service `rust-monitor`, loopback port `8800`.

```sh
akurai-monitorctl health
akurai-monitorctl status
akurai-monitorctl events
akurai-monitorctl hosts list
akurai-monitorctl applications list
akurai-monitorctl metrics list --limit 50
akurai-monitorctl logs list --limit 50
akurai-monitorctl alerts list
akurai-monitorctl recipients list
akurai-monitorctl channels list
```

## CRUD contract

Every managed resource supports `list`, `get ID`, `create FIELD=VALUE...`, `update ID FIELD=VALUE...`, and `delete ID --yes`:

```sh
akurai-monitorctl hosts create name=tv address=100.88.0.8 kind=linux
akurai-monitorctl applications create host_id=2 name=tv-frame service_name=tv-frame.service
akurai-monitorctl metrics create host_id=2 application_id=1 name=health.ready value=1
akurai-monitorctl logs create host_id=2 application_id=1 source=tv-frame line='ready'
akurai-monitorctl alerts create name='High load' host_id=2 metric_name=load.5m operator=gt threshold=4 duration_secs=300
akurai-monitorctl hosts update 2 enabled=false
akurai-monitorctl applications delete 4 --yes
```

Values are JSON scalars: numbers, `true`, `false`, `null`, or strings. Alert operators are `gt`, `lt`, and `eq`.

## Recipients and channels

A channel is a transport connection. A recipient selects the destination on that connection. Enabled recipients receive triggered and resolved alerts.

Webhook:

```sh
akurai-monitorctl channels create name=ops-webhook kind=webhook endpoint=https://example.invalid/monitor token_env=MONITOR_CHANNEL_TOKEN_WEBHOOK
akurai-monitorctl recipients create channel_id=1 name=operations target=on-call
akurai-monitorctl channels test 1
```

Pi-bun agent session:

```sh
akurai-monitorctl channels create name=pi-bun kind=pi-bun endpoint=http://100.88.0.9:4173 token_env=MONITOR_CHANNEL_TOKEN_PI_BUN
akurai-monitorctl recipients create channel_id=2 name=monitor-agent target=SESSION_ID
akurai-monitorctl channels test 2
```

For `pi-bun`, `target` is the Pi-bun session ID that receives an OMP prompt. Its endpoint is restricted to Titan's `100.88.0.9` AkurAI-VPN address; webhook endpoints require HTTPS and must resolve only to public addresses. Optional channel secret names must start with `MONITOR_CHANNEL_TOKEN_` and exist in the protected Monitor service environment. Store or synchronize their values through AkurAI-PassVault; never put a value in a CLI argument, SQLite, source, logs, chat, or this skill.

A channel test must return `sent` greater than zero before treating the connection as ready. HTTP errors, missing environment variables, missing recipients, and Pi-bun login/command failures fail closed.
