# CI results to the agent that pushed

An agent pushes a branch and needs to know whether CI passed, so it can fix what failed or move on. The obvious way is
to have it watch: `gh run watch` until the run finishes. That works for one agent. With five or ten agents each
watching their own runs, it fails in a way nobody sees coming: they all poll GitHub as you, they use up your account's
API allowance together, and every agent gets "API rate limit exceeded" until the hour turns over.

There are three ways for the result to reach the agent, from simplest to best:

- **Polling**, done carefully: fine for a few agents.
- **A GitHub webhook routed by Fleetlayer** to the session that pushed the commit (or subscribed to the branch): no
  polling at all.
- **The notification email**, read by an email triage agent and forwarded to the right session, if you already run
  one.

## Polling, and what it costs

GitHub allows 5,000 API requests an hour per user, and every agent running `gh` as you shares them. `gh run watch`
checks every 3 seconds by default, about 1,200 requests an hour for each run being watched, so four or five agents
watching at once use up the hour. If agents poll, give them this in their shared instructions:

```sh
timeout 1020 gh run watch <run-id> --interval 60 --exit-status     # once per run; never a loop over gh run list
```

`gh api rate_limit` shows what's left and when it resets. A watcher still costs a request a minute and holds a
session while it waits; the next way costs nothing.

## A webhook to the session that pushed

GitHub can call a URL whenever a workflow run finishes. Fleetlayer's control plane receives it, reads which repository,
branch and commit it's about, and delivers it to the sessions subscribed to that commit or that branch. The agent that
pushed hears about its own build, success or failure, as a message in its own session, a few seconds after the run
ends.

**1. An endpoint on the control plane.** Make up a signing secret for GitHub, then your agent runs, on the control
plane's machine:

```sh
openssl rand -hex 32 > github-secret
fleetlayer-control endpoint add --label github --channel ci --github-secret-file github-secret
# prints /hooks/<id>/<secret>, once
```

The path includes a secret, so treat the whole URL like a password; keep it and the signing secret in your password
manager. With `--github-secret-file`, a request must also carry GitHub's signature of its body
(`X-Hub-Signature-256`) made with that secret, or it's refused. `fleetlayer-control endpoint secret <id>
--github-secret-file …` adds one to an endpoint you already have.

**If the URL leaks** (it ended up in a log, a screenshot or a chat), give the endpoint a new one. These commands come
in v0.3.12. `fleetlayer-control endpoint list` shows each endpoint's ID, and `endpoint rotate <id>` prints a new
`/hooks/<id>/<secret>` once. The old URL answers 404 at once, and the label, channel and GitHub signing secret stay.
Deliveries fail until GitHub has the new URL; its Recent Deliveries tab can send them again. For no gap at all, add a
second endpoint with the same `--github-secret-file`, repoint GitHub to it, then `endpoint remove <old id>`.

To repoint a webhook with `gh`, send the whole config, because a `PATCH` of `config` replaces all of it, then ask
GitHub to ping it:

```sh
gh api -X PATCH repos/your-org/your-repo/hooks/<hook-id> -f config[url]="$NEW_HOOK_URL" \
  -f config[content_type]=json -f config[secret]="$(cat github-secret)"
gh api -X POST repos/your-org/your-repo/hooks/<hook-id>/pings
```

The ping's result shows in Recent Deliveries: it should be 200.

**2. Expose only the hooks listener.** The control plane listens for webhooks on a port of their own (`--hooks-listen`,
7601 by default), apart from its API. Put just that one behind a public HTTPS address: a Cloudflare Tunnel, or a
route on your reverse proxy that sends only `/hooks/` there (and `/healthz`, if you want an uptime check of it).
Nothing else of the control plane should be public.

**3. Add the webhook in GitHub.** In the repository (or the organisation) settings, under Webhooks: the public URL
with the path from step 1, content type `application/json`, the signing secret as the webhook's **Secret**, and the
event **Workflow runs**. An agent with admin rights on the repository can add it with `gh api` instead:

```sh
gh api repos/your-org/your-repo/hooks -f name=web -F active=true -f 'events[]=workflow_run' \
  -f config[url]="$HOOK_URL" -f config[content_type]=json -f config[secret]="$(cat github-secret)"
```

**4. Agents subscribe after they push.** Put this in their shared instructions (`AGENTS.md`):

```sh
git push && fleetlayer subscribe ci --commit
```

Keep polling only as a fallback: if nothing arrives within the usual CI time plus five minutes, the agent checks once
with `gh run watch <id> --interval 60`.

`--commit` reads the repository from the `origin` remote and the commit from `HEAD` (or give one: `--commit
<rev>`), and subscribes to `<owner/repo>@<sha>`. This is what makes it work when every agent pushes to the same
branch: results for `main` go to whoever pushed that commit, not to every session on `main`. The command says what it
found:

```
Subscribed to ci for your-org/your-repo@1a2b3c4.
Already in (sent to you now): Site success on your-org/your-repo@main 1a2b3c4 "Fix the login form" https://github.com/…
Pending: CI. Each result arrives as a message when it completes.
```

A commit subscription ends by itself. The control plane tracks every run of the commit (GitHub tells it when each is
requested), and once none is pending, the subscription is removed. A workflow that finished before the agent
subscribed (a 30-second docs build) is sent straight away, and if everything has already finished, the subscription
ends at once. Runs are remembered for a day, and a commit subscription that's still there after a day is removed.

Each result arrives as one message, marked as outside data:

```
CI failure on your-org/your-repo@main 1a2b3c4 "Fix the login form" https://github.com/your-org/your-repo/actions/runs/123
```

followed by the details (workflow, conclusion, branch, commit, run URL, attempt). A `cancelled` result usually means
someone pushed on top, and a workflow that cancels older runs stopped this one: the agent fetches and subscribes to the
newer commit. Sessions never get GitHub's raw
payload, which is long and full of URLs (a ping's includes the webhook's own URL, secret path and all): other GitHub
events arrive as a short summary of the same fields, a ping as one line, and the endpoint's secret is removed from
anything delivered. Only completed runs are delivered;
`endpoint add --all-actions` delivers `requested` and `in_progress` too. GitHub's workflow run events don't list the
jobs, so the message names the workflow, not which job failed: the agent opens the run (`gh run view <id>
--log-failed`, one API call) to see why.

An agent on its own branch can still subscribe to the branch, and that subscription stays until it's removed:

```sh
fleetlayer subscribe ci --subject your-org/your-repo@your-branch
```

Subscriptions are kept by `<tab>@<node>`, so they survive restarts.

**5. A home for the rest.** Results nobody subscribed to (a commit whose agent didn't subscribe, a branch whose agent
finished) go to a fallback session:

```sh
fleetlayer-control fallback ci planning@laptop
```

Then nobody polls, and the API allowance is left for real work. `fleetlayer subscriptions` lists who's listening to
what; `fleetlayer unsubscribe ci --subject …` ends a branch subscription when the branch is merged. More on channels
and subscriptions is in [Watch, steer and message your agents](https://fleetlayer.ai/guides/day-to-day.md).

**6. Watch the pipe.** Two checks catch it breaking: an uptime check of the public `/healthz` (the listener is up and
reachable), and a scheduled job that reads the webhook's last delivery (`gh api repos/<repo>/hooks/<id>`, the
`last_response.code` field) and fails when it isn't 200 (GitHub reached it, and it took the delivery). A hook that has
never delivered has no code yet; treat that as fine only until the first push.

## The notification email, through a triage agent

GitHub also emails you when a workflow run fails. If an email triage agent already reads your mail, it can treat those
emails the same way: recognise a CI failure, work out which session owns that repository and branch, and forward it
there with `fleetlayer msg send`, instead of leaving it in your inbox. This needs no public endpoint and no webhook
settings, and it covers repositories where you can't add a webhook. It's slower than a webhook (email takes a minute
or two) and depends on the triage agent's judgement for the routing.

[Quietdeck](https://quietdeck.ai), a control deck built on Fleetlayer, does this today. Its triage agent reads the CI
failure email, judges which session owns that repository or branch (from the sessions' summaries and what it knows),
and proposes a card with a forward action; it can also resume an ended session, or start a new one. By default you
approve each forward. A rule you make, together with the project's policy allowing forwards, lets routine ones go out
automatically, and those still show in the deck's timeline. Nothing in it is CI-specific: you set up a deck, its
runner and a triage agent ([getting started](https://quietdeck.ai/getting-started/)). While an agent is fixing CI, a
Quietdeck lease can keep the failure emails out of your inbox until it's done
([leases](https://quietdeck.ai/leases/)).

A triage agent reads strangers' email, so give it its own Unix user and limit whom it may message:
[Run an agent that reads strangers' text](https://fleetlayer.ai/guides/untrusted-input.md).

## Which to use

| | Polling | Webhook | Email |
|---|---|---|---|
| Setup | none | an endpoint, a public hooks address, a webhook per repo or org | a triage agent |
| Speed | up to the interval | seconds | a minute or two |
| Cost | API requests from every watcher | none | the triage agent's tokens |
| Works for repos you don't admin | yes | no | yes |

Most fleets end up with webhooks for their own repositories and polling (at 60 seconds) as the fallback where they
can't add one.

## When it doesn't work

- **"API rate limit exceeded" in every session:** too many watchers. `gh api rate_limit` says when it resets; then
  move agents to `--interval 60` or to the webhook.
- **The webhook fires but no session hears it:** check the subject. `fleetlayer subscriptions` shows what each session
  subscribed to; it must be exactly `<owner/repo>@<branch>`, or `<owner/repo>@<full 40-character sha>` (a short sha
  matches nothing: use `--commit`). Events nobody subscribed to go to the fallback, if there is one.
- **GitHub shows 401:** the signature didn't verify: the webhook's Secret in GitHub and the endpoint's
  `--github-secret-file` differ. Set them to the same value.
- **A commit subscription never ends:** a workflow started by another workflow (`on: workflow_run`) that begins after
  the others finished starts a new wait, and one whose `requested` event never arrived isn't known to be pending, so it
  ends early. Either way the subscription is gone after a day.
- **GitHub shows the webhook failing:** its Recent Deliveries tab shows the response. A 404 means the path is wrong;
  no response means the hooks listener isn't reachable from the internet.
- **The same result arrives twice:** it shouldn't: deliveries are de-duplicated by GitHub's delivery ID. If a
  session subscribed to both the branch and the commit, it gets the result once; two sessions subscribed to the same
  branch both get it.
