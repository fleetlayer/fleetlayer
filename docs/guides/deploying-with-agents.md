# Deploying with agents

Once agents write most of the code, deploys stop being something a person does twice a week. Several agents finish
branches the same afternoon, each wants staging, each wants to ship, and nobody is watching every step. This guide
shows two ways teams run that, from the same pieces, and how to pick the parts you need.

- **Project A merges to main and deploys itself.** Agents open pull requests, CI and a reviewer agent check them, and
  every merge to main goes to production through Argo CD. Nobody approves a deploy: the pipeline is trusted to only
  move forward ([Continuous deployment](continuous-deployment.md)).
- **Project B deploys by hand, carefully.** Each branch is reviewed, QA'd on a shared staging environment, and goes to
  production only after a person approves it with a hardware key in the team's deploy tool. Branches take turns on
  staging, because two branches on one staging environment test neither.

The same pieces run both; the difference is which ones you switch on.

## The pieces

| Piece | What it does | A | B |
|---|---|---|---|
| [CI results to the agent that pushed](ci-results.md) | Each workflow's result arrives in the session that pushed the commit, from a webhook: no polling | yes | yes |
| [Leases](../plugins/leases.md) | Build slots on shared machines; a lock per named thing (a Terraform component) that ends when its holder dies | build slots | build slots, Terraform claims |
| [Deploy Queue](../built-on-fleetlayer.md#deploy-queue) | Branches take turns on staging, with a live page of who holds it and where each deploy is | no | yes |
| Your deploy tool's approvals | A person approves production, with their key | no | yes |
| [Shared commands](shared-commands.md) | An agent on a server asks for the approval on the person's laptop, where the key is | no | yes |
| [Continuous deployment](continuous-deployment.md) | Images by digest, production only moves forward, migrations that only add | yes | the same rules help |

## Project A: merge and it ships

1. An agent finishes a branch, pushes, and subscribes to its commit's CI: `git push && fleetlayer subscribe ci
   --commit`. The result arrives as a message.
2. A reviewer agent (or a person) reviews the pull request. When CI is green and the review passes, the agent merges.
3. CI builds the image, pins its digest in the infrastructure repo, and Argo CD rolls it out. A deploy that would move
   production backwards is skipped ([Continuous deployment](continuous-deployment.md) has the whole pipeline).
4. Argo CD's notifications can post to a Fleetlayer hooks endpoint, so a failed sync reaches an agent that can fix it.

There's no queue: main is the queue, and every merge is small. Agents that touch shared infrastructure still take a
lease per thing they change (`fleetlayer lease acquire terraform --payload '{"component":"dns"}' --wait`), so two of
them never apply over each other.

## Project B: take turns, QA, approve

1. **Ready means the person says so.** The agent finishes, CI is green, and its person decides the branch is ready for
   staging. Then the agent joins the queue with its PR and a summary of what staging QA covers.
2. **Its turn.** When staging is free and it's first in line, the agent rebases, waits for CI, takes staging, and
   approves the staging deploy itself: joining was the person's approval.
3. **QA.** The queue's page follows the deploy through CI's jobs. The agent runs its staging QA and reports passed or
   failed. A failed QA keeps staging while the fix goes in (a migration may have run there already).
4. **Production needs the person.** The agent asks its person, who approves the production deploy in the deploy tool,
   with their PIN and key. The queue shows "waiting for approval in <your deploy tool>" and approves nothing. If the
   agent runs on a server and the key is on the laptop, a [shared command](shared-commands.md) carries the request
   to the laptop, where the person sees exactly which commit and touches the key.
5. **Shipped.** CI merges the branch once production is live; the queue sees the merge, releases staging, and the
   next branch's turn starts. The announcement goes to the team's channel.

Agents don't keep polling for any of this: the queue's long-poll wakes them when it's their turn, and a session that
crashes or a laptop that sleeps frees staging by itself after a few minutes.

## Choosing

- **One environment per change is cheap?** (Preview environments per pull request.) Then you don't need turns: use
  Project A's shape and give each branch its own environment.
- **Staging is shared and expensive** (a real database, real integrations, data migrations), or production needs a
  person's approval by policy? Take turns, as in Project B.
- **Both in one company** is normal: an internal tool deploys on merge while the customer-facing app goes through the
  queue. The agents, the CI webhooks and the leases are the same; only the project's settings differ.

## What to tell your agents

Put the project's way of deploying in its `AGENTS.md`: when to join (only on the person's yes), what staging QA
covers, who approves production, and never to approve production themselves. Deploy Queue's README has the API its
CLI uses; a project usually wraps it in a small script (`bin/deploy-queue join 1093 --summary "..."`) so agents don't
handle tokens or long-polls themselves.
