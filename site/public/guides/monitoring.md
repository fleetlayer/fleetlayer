# Monitoring a fleet, with an agent that fixes it

A fleet of agents comes with a lot of things running in the background: the Fleetlayer node on each machine, a sync
service, a backup every hour, a webhook receiver, a scheduled agent that reads the inbox. Each of them can stop without
saying so. A backup job can keep running and fail every time. A sync daemon can stay up after it lost its connection.
You find out days later, when you need the thing it was meant to do.

This guide sets up monitoring so that a failure is noticed within minutes and the alert reaches an agent that looks
into it before it reaches you. You decide what gets watched and which agent hears about it; your agents add the checks
and handle the alerts. The steps are written so an agent can follow them, and the few that need you are marked.

The shape:

- **Every background thing gets a check in the same change that adds it.** Something always on gets a health check;
  something that runs on a schedule pushes a heartbeat when it finishes.
- **The check looks at the work, not only the process.** A node that's running but has lost its link to the control
  plane reports itself unhealthy. A backup reports failure when restic fails, not only when the script didn't start.
- **On a laptop, Fleetlayer's gateway is the monitor.** On a server, [Gatus](https://gatus.io) is, with its config
  written from the same list of services.
- **Alerts go to a session.** One message per change, in plain words, to the agent that owns the service.

This uses `laptop` and `server` for machines and `planning` and `infra@server` for sessions. Change them to yours.

## 1. Pick the check for each thing

| What it is | Check | Fails when |
|---|---|---|
| Always on (a web page, a hub, the control plane) | a health path such as `/healthz`, polled | no answer, or not 2xx |
| A scheduled job (a backup, a staging copy) | a heartbeat it pushes when it finishes | `success=false`, or no push |
| A daemon whose work you can't see (a sync) | a heartbeat after each real piece of work | the work stops |
| The Fleetlayer node | its own heartbeat, or the gateway asking it | a plugin, the link or the gateway is down |

The third row is the one people skip. A sync daemon that's up but disconnected passes every "is it running" check. Push
the heartbeat from the place where a sync succeeds, so a stalled sync stops the heartbeat.

The agent that adds a service adds its check in the same change: the same pull request, the same commit, the same task.
A check left for later tends not to be added at all. Put that rule in your agents' shared instructions (`AGENTS.md`), so
every agent follows it without being reminded.

## 2. The node's own heartbeat

The node judges its own health: every enabled plugin running, the link to the control plane up when one is configured,
and the gateway's `/healthz` answering when node.yaml names the gateway. In `~/.config/fleetlayer/node.yaml`:

```yaml
heartbeat:
  url: https://gatus.example.com/api/v1/endpoints/fleetlayer_laptop/external
  token_file: ~/.config/fleetlayer/heartbeat-token
  every: 60
```

Every `every` seconds (60 by default) the node posts `?success=true` or `?success=false&error=<why>` to that URL, with
the token as a bearer token. It waits a minute after starting before the first report, so plugins and the link have time
to come up. When the node dies, the reports stop and the monitor's own timeout raises the alert. Either way you hear
about it.

To see the same judgement by hand, `fleetlayer status` prints `healthy` or `unhealthy: <problem>` under the node line
(from v0.3.8), and `fleetlayer status --json` includes it as `health` (absent when healthy, otherwise the reasons
joined with `;`).

On a laptop that runs [`fleetlayer up`](https://fleetlayer.ai/guides/laptop.md), you don't need this: the gateway asks the node directly (below).

## 3. On a laptop: the gateway

`fleetlayer up` starts a gateway with a status page at `http://fleetlayer.localhost:7680`. It is the laptop's monitor.

Every 30 seconds it polls each service's health path and asks the node how it is. A service whose plugin stopped
announcing it counts as down too, so a killed plugin shows before the node restarts it. A check is down after two
failures in a row, so a crash that the service manager or the node fixes within a few seconds isn't an alert.

To have an agent hear about it, name a session when you start:

```sh
fleetlayer up --alerts-to planning
```

`--alerts-to` is written into gateway.yaml only when that file doesn't exist yet. Otherwise set it in
`~/.config/fleetlayer/gateway.yaml`:

```yaml
alerts:
  session: planning      # tab, or tab@node
  desktop: true          # the default
```

Each change (down, or back up) is a desktop notification (`osascript` on macOS, `notify-send` on Linux when it's
installed) and a Fleetlayer message to that session, such as:

```text
Fleetlayer gateway: Heartbeat backup is down: restic failed Status: http://fleetlayer.localhost:7680
```

### Heartbeats on the laptop

The gateway takes heartbeats at the same path Gatus uses, so a scheduled job reports the same way on the laptop and on a
server. Name each one in gateway.yaml with how often it reports:

```yaml
heartbeats:
  backup: { every: 3600, token_file: ~/.config/fleetlayer/backup-heartbeat-token }
```

Then, at the end of the job:

```sh
token=$(cat ~/.config/fleetlayer/backup-heartbeat-token)
if restic backup ~/Notes; then
  curl -fsS -X POST -H "Authorization: Bearer $token" \
    "http://fleetlayer.localhost:7680/api/v1/endpoints/fleetlayer_backup/external?success=true"
else
  curl -fsS -X POST -H "Authorization: Bearer $token" \
    "http://fleetlayer.localhost:7680/api/v1/endpoints/fleetlayer_backup/external?success=false&error=restic+failed"
fi
```

- **`success=false` is down at once.** A job that reports its own failure doesn't wait for a second round.
- **A missing push is down after twice `every`.** Here, two hours without a report.
- **The key** can be the heartbeat's name (`backup`) or Gatus's `<group>_<name>` form (`fleetlayer_backup`). Use the
  Gatus form and the job's URL differs between laptop and server only in the host.
- **Without `token_file`**, any process on the machine may push. With one, a push without the right
  `Authorization: Bearer` header gets a 401.

Where `*.localhost` doesn't resolve outside the browser (some Linux systems), use
`curl --connect-to ::127.0.0.1:7680` with the same URL.

## 4. On a server: Gatus

On a server you probably run a monitor already. If it's Gatus, the gateway writes its config from the same registry of
services and heartbeats:

```sh
fleetlayer-gateway emit gatus --alert matrix > /etc/gatus/fleetlayer.yaml
```

It writes an endpoint for each service with a health path (polled every 60 s, expecting a 200) and an external endpoint
for each heartbeat in gateway.yaml, with a heartbeat interval of twice its `every`. Tokens are never written into the
file: each external endpoint gets `${FLEETLAYER_HEARTBEAT_<NAME>_TOKEN}` for Gatus to fill in from its environment.
`--alert` adds a Gatus alert type to every endpoint; it must be one already configured in Gatus. Setting up that alert
type usually needs a chat account or token, which is yours to create and paste (into the secrets manager, where the
agent reads it from). `--group` changes the
group (default `fleetlayer`), which is also the first half of each heartbeat's key.

A heartbeat named `backup` comes out as:

```yaml
external-endpoints:
- name: backup
  group: fleetlayer
  token: ${FLEETLAYER_HEARTBEAT_BACKUP_TOKEN}
  heartbeat:
    interval: 7200s
  alerts:
  - type: matrix
```

The health URLs are the backend addresses the gateway knows (`http://127.0.0.1:7670/healthz`), so run the emit on the
machine Gatus can reach them from, or list the services in gateway.yaml with addresses Gatus can reach. With
`--out <file> --watch`, it keeps the file current as plugins restart on new ports.

For a node that isn't on the same machine as a gateway, point its `heartbeat.url` (step 2) at a Gatus external endpoint
of its own.

## 5. Alerts an agent can act on

An alert is worth sending only if someone would act on it. A few rules keep them that way:

- **One alert per change.** Down once, back up once. Never one per failed probe.
- **One message per round.** Stopping the control plane also drops the node's link to it; the gateway sends both
  changes as one message, so the agent sees one cause instead of two separate incidents.
- **Short outages the service manager fixes are no alert.** That's what the two-failures rule is for.
- **Say what failed and where to look.** The gateway's messages name the check, the error and the status page.
- **Don't retry what can't succeed.** If the alert session doesn't exist, the node refuses the message, and the gateway
  shows the refusal on its status page rather than retrying it forever. If the node itself is down, alerts wait (the
  newest 50) and go out when it's back.

Then tell the receiving agent what to do with them, in its own instructions (its `AGENTS.md` or `CLAUDE.md`): look into
the alert, fix what it can, and ask you only about what it can't. In the author's setup, alerts go to one triage agent,
which hands each one to the agent that owns the service. That agent investigates (a full disk, an expired key, a stale
lock) and escalates only what it can't fix.

Don't silence a real problem by loosening its check. If an alert fires for something that isn't a problem, fix the check
so it measures the work.

## 6. Route each alert to the session that owns the service

The gateway sends every alert to one session. When different agents own different services, route alerts through a
Fleetlayer channel instead, the same way webhooks reach the session that owns the work.

Create a webhook endpoint for the monitor:

```sh
fleetlayer-control endpoint add --label monitor --channel alerts    # prints /hooks/<id>/<secret>, once
```

Point your monitor's webhook alert at the control plane's hooks listener with that path, adding `?subject=<service>`.
The body can be JSON with a `summary`, `text`, `message` or `title` field, or plain text; the first 200 characters of
plain text become the summary. To try it:

```sh
curl -fsS -X POST -H 'Content-Type: application/json' \
  -d '{"message": "backup is down: restic failed"}' \
  "https://hooks.example.com/hooks/<id>/<secret>?subject=backup"
```

In the session that owns backups:

```sh
fleetlayer subscribe alerts --subject backup
```

And name where everything else goes:

```sh
fleetlayer-control fallback alerts infra@server
```

A session subscribed to `alerts` without `--subject` gets every alert on the channel. The fallback gets an alert only
when nobody subscribes to it. Nothing is queued: if the target session is offline, the event is kept with its delivery
result, so pick a fallback that's always running.

## Gatus or Uptime Kuma

Both are good self-hosted monitors with health checks and push heartbeats. The difference that matters for a fleet is
where the checks live.

- **Gatus** keeps every check in config files. They live in your infrastructure repo, go through review and deploy with
  everything else, and an agent adds a check in the same change that adds a service. That's why Fleetlayer's gateway
  speaks Gatus's heartbeat path and can write Gatus config.
- **[Uptime Kuma](https://github.com/louislam/uptime-kuma)** is friendly and has a good UI. Its checks live in its own
  database behind that UI, so an agent automating it goes through its API or edits the database, both harder to review
  than a file in git. It may grow better agent access over time. For someone who wants a UI first, it's a fine choice.

## When it doesn't work

- **The monitor dies with the thing it monitors.** A watcher that runs inside the process it watches goes quiet exactly
  when you need it. That's why the gateway runs as its own user service beside the node rather than as a node plugin: a
  dead node still shows on the status page and still gets a desktop notification, and the session message goes out when
  the node is back. launchd or systemd restart the gateway, and the node's health includes the gateway's `/healthz`, so
  a gateway that keeps failing turns the node's health red. Apply the same rule to your own watchers.
- **A proxy closes idle connections.** Traefik 3.4's default entrypoint `idleTimeout` is 180 seconds, and a shared
  tool's stream (a browser session between tasks) can sit idle much longer. The control plane pings each node's link
  every 30 seconds and nodes ping their tool streams every 30 seconds, so any proxy in front of a control plane needs an
  idle timeout longer than 30 seconds. Otherwise the link drops and comes back, and the node's heartbeat reports "not
  linked to the control plane" in between.
- **The heartbeat says success while the work fails.** Report from the line after the work, with its exit status, as in
  the backup example. A heartbeat sent at the start of a job, or by a wrapper that ignores the result, only proves the
  scheduler ran.
- **The inputs stopped.** A backup can succeed every hour on yesterday's data if the job that stages the files stopped.
  Give staging jobs their own heartbeats. See [Backups for a fleet of agents](https://fleetlayer.ai/guides/backups.md) ("Watch it").
- **Alerts go nowhere.** If the gateway's alert session was renamed or closed, its messages are refused; the status page
  says so. Check it after renaming a session.
- **Heartbeat pushes get 404 or 401.** A 404 means gateway.yaml has no heartbeat by that key (the gateway reads its
  config at start, so restart it after adding one). A 401 means the token is missing or doesn't match the file.
