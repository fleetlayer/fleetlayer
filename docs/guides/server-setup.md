# A whole server, set up by an agent

A laptop is a fine place to start, and a single VM is enough for a few agents. Once you run many kinds of agents side
by side (personal errands, email triage, product work, infrastructure, a worker that builds things), you want a server
that holds all of them, each in its own Unix user, with backups, monitoring and secrets done properly. Setting that up
by hand takes weeks. An agent can do it in an afternoon if it has a working example to start from.

[server-iac-example](https://github.com/fleetlayer/server-iac-example) is that example: a complete
infrastructure-as-code setup for one self-hosted server, written for an AI agent to read and follow. Point a Claude
Code or Codex session on your laptop at it, give it SSH to a fresh
[Proxmox](https://www.proxmox.com/en/proxmox-virtual-environment/overview) host, and it builds the rest, asking you
only for what needs a person: creating accounts, approving a device, pasting a token.

It's cut from a real home server that has run a fleet of agents every day for months, with everything private replaced
by placeholders. Apache 2.0: copy it, change it, keep what you like.

## What you get

- **Proxmox** with its guests described in Terraform, and their software configured by Ansible.
- **k3s** in one container, with [Argo CD](https://argo-cd.readthedocs.io/) deploying everything in it from git.
- **Tailscale** as the private network: every machine and agent user reachable by name, no ports open to the
  internet, the access policy in git.
- **Secrets from 1Password**, through [External Secrets](https://external-secrets.io/) and 1Password Connect. Nothing
  secret is committed, and one file lists every secret and who supplies it, so the agent creates the items and you
  only paste what only you have.
- **Monitoring with alerts**: Prometheus, Grafana and Loki, [Gatus](https://gatus.io) for health checks and
  heartbeats, alerts to a chat room ([monitoring](monitoring.md)).
- **Backups** with restic to Backblaze B2, each job reporting a heartbeat ([backups](backups.md)).
- **An agents VM with a Unix user per role**: your own, one for an agent that reads untrusted text, one for a worker.
  Each user has herdr, Claude Code and its own Fleetlayer node, so the access rules hold
  ([a server VM for your agents](agents-vm.md), [an agent that reads strangers' text](untrusted-input.md)).
- **Fleetlayer's control plane** in k3s, from the release binary, with its webhooks reachable through one Cloudflare
  tunnel that passes nothing but `/hooks/`.

## How to use it

**You:** install Proxmox on the machine from its ISO and make sure your laptop can SSH in as root. Then, in a session
on your laptop:

```text
Read https://github.com/fleetlayer/server-iac-example (start with its llms.txt and AGENTS.md) and set up my
Proxmox host at root@<address> the way its guides describe. Ask me for anything only I can do, all at once.
```

The repository's guides are the steps the agent follows, in order: make a private copy of the repository, fill in
the settings file (domain, network, vault), create every secret, set up the tailnet, prepare the host, create the
guests, bring up k3s and Argo CD, connect the alert room and backups, then the agents VM and its roles, and finally
the server's Fleetlayer nodes. Each guide ends with a check the agent runs before going on, so a mistake shows up where
it was made.

What only you do, collected so you can do it in one sitting: create the Tailscale, 1Password, Backblaze and
Cloudflare accounts, make their API tokens and paste them into the 1Password items the agent created, create the alert
rooms in a Matrix client, and sign in Claude Code once for each agent user.

## After it's built

The repository stays the source of truth. When you want a new agent role, a new service or a new guest, ask your
infrastructure agent: it changes the repository, applies it, and adds the health check or heartbeat in the same
change. A role with its own permissions is one more Unix user and one more node, a few minutes' work once the server
exists. Any piece can still move later ([move a piece to another machine](moving.md)).

Not ready yet: an automated end-to-end test that builds the whole setup on a throwaway rented host. Until it runs,
expect a step or two that needs fixing on your hardware, and send what you find to the repository as an issue or a
pull request.

## Other session managers and dashboards

The example runs agents in [herdr](https://herdr.dev) tabs. Anything that keeps sessions running works the same way,
because Fleetlayer attaches to the session manager you use rather than replacing it. The example's author also runs
Quietdeck, a personal dashboard that reads the alert room and the inbox and wakes the right agent, on the same server;
the guides say where something like it fits, but it isn't part of the example.
