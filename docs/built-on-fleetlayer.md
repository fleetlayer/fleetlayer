# Built on Fleetlayer

Products that use Fleetlayer's pieces (sessions, messages, events, leases, the access policy, sign-in) instead of
building their own. Each is a separate project with its own repository; you can use Fleetlayer without any of them.

## Quietdeck

[Quietdeck](https://quietdeck.ai/) is a control deck for one person's agents. An email triage agent reads your inboxes
and posts cards to the deck, and you approve what it proposes: a reply, a forward to the agent that owns the work, a
filter. Its runner is a Fleetlayer plugin that carries out what you approved, and forwards mail, alerts and CI
failures through Fleetlayer's messages, under the access policy. Source-available under the Functional Source License.
[Its directory entry](directory/quietdeck.md).

## Deploy Queue

[Deploy Queue](https://github.com/fleetlayer/deploy-queue) lets people and their agents take turns on a shared
environment. One branch holds staging at a time, from its staging deploy through QA until its production deploys are
live and it's merged; the rest wait in line. A live page shows who holds staging and which step its deploy is on,
who's next, whose CI is red and what shipped today. It also keeps claims on shared infrastructure (Terraform modules),
so two sessions don't apply over each other, and an apply and a deploy on the same environment wait for each other.

Everything about a project is in one settings file: its environments, the steps a branch goes through, which CI jobs
mean what. The approvals stay in the deploy tool you trust: the queue shows "waiting for approval" and approves
nothing. It runs on its own today, with an HTTP API and a token per client; using Fleetlayer's leases for its turns
and claims, and its events for CI status, is being built. Source-available under the Functional Source License.
[Its directory entry](directory/deploy-queue.md), and how it fits a team's deploys: [Deploying with
agents](guides/deploying-with-agents.md).

## Yours

Built something on Fleetlayer? Add it to the [plugin directory](directory.md) with a pull request, and tell us: we
list products here that people can use today.
