# Keep agents to the hosts they need

An agent that can read a token can send it to any host it can reach, and an agent that reads strangers' text can be
told to. This guide restricts a role's network access to the hosts its work needs, in log mode first, so nothing breaks
while you learn what those hosts are. How it works, every setting and its limits: [Network access per role](../egress.md).

You don't have to do this by hand. Ask an agent on the machine to set up egress control for a role: the
`fleetlayer-egress` skill has these steps. It needs root for the firewall, so it will ask you to run that part.

The example restricts the `triage` role ([An agent that reads strangers' text](untrusted-input.md)) on a Linux server.
Linux only for now.

## 1. Pick the roles

A role is a Unix user (or a few) with its own node. Restrict the roles you trust least first: the one reading email or
the web, then everyday work. Leave your own user, root and any trusted runner unrestricted.

A restricted user mustn't have sudo, and mustn't be in the `docker` group (containers started through Docker's daemon
aren't restricted).

## 2. A user for the proxy

The proxy enforces the rules, so it runs as a user outside every restricted role, which those roles can't stop or
replace. A user of its own is best:

```sh
sudo useradd --create-home --shell /bin/bash egress
sudo loginctl enable-linger egress
```

Then give it its own node ([Add a machine](node-services.md)), with the plugin in its `node.yaml`:

```yaml
plugins:
  - name: egress
    command: fleetlayer-egress
    grants: [access.check]
```

(For a first try, your own user's node works too, as long as your user isn't in a role.)

## 3. The settings, in log mode

As the proxy's user, write `~/.config/fleetlayer/egress.yaml`:

```yaml
mode: log
roles:
  triage:
    users: [triage]
    node: server-triage          # the role's node, so its agents can ask for hosts
    presets: [anthropic]
    hosts:
      - fleet.example.net        # your control plane, which the role's node links to
```

Check it, then restart the egress node so the plugin starts:

```sh
fleetlayer-egress check
```

## 4. The firewall

As root, on the same machine:

```sh
sudo fleetlayer-egress firewall --settings ~egress/.config/fleetlayer/egress.yaml          # read the rules first
sudo fleetlayer-egress firewall --settings ~egress/.config/fleetlayer/egress.yaml --apply
```

They're lost at a reboot unless something applies them at boot. Install the unit it prints:

```sh
sudo fleetlayer-egress firewall --settings ~egress/.config/fleetlayer/egress.yaml --unit \
  | sudo tee /etc/systemd/system/fleetlayer-egress-firewall.service
sudo systemctl enable --now fleetlayer-egress-firewall
```

Regenerate them (`--apply` again) after adding a role, a user or a port. Host lists don't need it.

## 5. The probe, on the role's node

The probe checks, as the role's own user, that the rules hold, and reports a problem through the node's health if
they don't. In the restricted role's `node.yaml`:

```yaml
plugins:
  - name: egress-probe
    command: fleetlayer-egress
    config:
      probe: { allowed: api.anthropic.com }
```

Run the same checks by hand as that user any time: `fleetlayer-egress probe`.

## 6. Tools that read `HTTPS_PROXY`

Ports 443 and 80 are redirected to the proxy without any setup. Pointing tools at the proxy as well gives better
refusals (a `403` that says how to ask, instead of a TLS alert) and lets them reach other ports. For the role's shell:

```sh
fleetlayer-egress env >> ~/.profile
```

For Claude Code, the same variables under `env` in the role's `~/.claude/settings.json`.

## 7. A week later: the list, then enforce

As the proxy's user:

```sh
fleetlayer-egress log --refused --days 7     # what would have been refused
fleetlayer-egress suggest --days 7           # the presets and hosts that work needed
```

Read the suggestion before merging it. Every allowed host is somewhere data can go, so leave out anything you can't
explain. Then set `mode: enforce` on the role, and apply the firewall again (enforcing changes the rules).

## 8. Who approves new hosts

When an agent needs a host that isn't on the list, it runs `fleetlayer-egress ask <host> --why "…"`. The request
reaches you through the control plane's approvals ([access control](../access-control.md#asking-the-person-approval-and-justification)):
an `access.ask` event, approved with `fleetlayer-control approvals approve <id>`. Once approved, it stays allowed
while it's used.

If your policy turns approvals off (`ask: []`), any host an agent asks for is allowed, with its reason in the audit
log. To keep asking for hosts only:

```yaml
ask:
  - { who: "*", can: connect, to: "host:*", for: 30d }
deny:                            # never, whoever asks
  - { who: "*", can: connect, to: ["host:pastebin.com", "host:*.pastebin.com"] }
```

## When something's refused

An agent sees `Could not resolve host`, `tlsv1 alert access denied`, or a `403` from the proxy. The skill tells it to
run `fleetlayer-egress why <host>` and ask if the work needs the host. You can see the same on the proxy's machine
with `fleetlayer-egress log --refused`.
