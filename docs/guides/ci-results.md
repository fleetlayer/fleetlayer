# CI results to the agent that wrote the branch

An agent pushes a branch and needs to know whether CI passed, so it can fix what failed or move on. The obvious way is
to have it watch: `gh run watch` until the run finishes. That works for one agent. With five or ten agents each
watching their own runs, it fails in a way nobody sees coming: they all poll GitHub as you, they use up your account's
API allowance together, and every agent gets "API rate limit exceeded" until the hour turns over.

There are three ways for the result to reach the agent, from simplest to best:

- **Polling**, done carefully: fine for a few agents.
- **A GitHub webhook routed by Fleetlayer** to the session that subscribed to that branch: no polling at all.
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

## A webhook to the session that owns the branch

GitHub can call a URL whenever a workflow run finishes. Fleetlayer's control plane receives it, reads which repository
and branch it's about, and delivers it to the session subscribed to that branch. The agent that pushed the branch
hears about its own build, as a message in its own session, a few seconds after the run ends.

**1. An endpoint on the control plane.** Your agent runs, on the control plane's machine:

```sh
fleetlayer-control endpoint add --label github --channel ci     # prints /hooks/<id>/<secret>, once
```

The path includes a secret, so treat the whole URL like a password.

**2. Expose only the hooks listener.** The control plane listens for webhooks on a port of their own (`--hooks-listen`,
7601 by default), apart from its API. Put just that one behind a public HTTPS address: a Cloudflare Tunnel, or a
route on your reverse proxy that sends only `/hooks/` there. Nothing else of the control plane should be public.

**3. Add the webhook in GitHub (you).** In the repository (or the organisation) settings, under Webhooks: the public URL
with the path from step 1, content type `application/json`, and the events **Workflow runs** (and **Check suites** if
you use checks from other apps). GitHub's settings page is yours to click; an agent with an admin token can add it
with `gh api` instead.

**4. Agents subscribe when they start on a branch.** Put this in their shared instructions (`AGENTS.md`):

```sh
fleetlayer subscribe ci --subject your-org/your-repo@your-branch
```

Fleetlayer reads the subject from GitHub's payload as `<owner/repo>@<branch>`, so a result for that branch reaches
that session and nowhere else. The message reads like
`workflow_run CI completed failure on your-org/your-repo@fix-login`, and arrives marked as outside data.
Subscriptions are kept by `<tab>@<node>`, so they survive restarts.

**5. A home for the rest.** Results for branches nobody subscribed to (main, a branch whose agent finished) go to a
fallback session:

```sh
fleetlayer-control fallback ci planning@laptop
```

Then nobody polls, and the API allowance is left for real work. `fleetlayer subscriptions` lists who's listening to
what; `fleetlayer unsubscribe ci --subject …` ends one when the branch is merged. More on channels and subscriptions is
in [Watch, steer and message your agents](day-to-day.md).

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
[Run an agent that reads strangers' text](untrusted-input.md).

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
  subscribed to; it must be exactly `<owner/repo>@<branch>`. Events nobody subscribed to go to the fallback, if there
  is one.
- **GitHub shows the webhook failing:** its Recent Deliveries tab shows the response. A 404 means the path is wrong;
  no response means the hooks listener isn't reachable from the internet.
- **The same result arrives twice:** it shouldn't: deliveries are de-duplicated by GitHub's delivery ID. If two
  sessions both subscribed to the branch, both get it.
