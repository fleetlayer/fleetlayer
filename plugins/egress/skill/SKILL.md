---
name: fleetlayer-egress
description: Work under Fleetlayer's network access rules, or set them up. Use when a connection fails with "Could not resolve host", "tlsv1 alert access denied", "CONNECT tunnel failed, response 403" or a refusal from Fleetlayer egress; when you need a host your role can't reach yet (ask for it with a reason); or when the person asks to restrict which hosts a role's agents may reach, review what they connect to, or switch a role from log mode to enforce.
---

# Network access per role

On a machine with Fleetlayer egress, a restricted role's Unix users reach the internet only through a local proxy
that reads the host name of each connection (it never decrypts) and allows the hosts on the role's list. Everything
else is refused, and every connection is logged. Users outside every role aren't filtered.

## Is it on for me?

```sh
fleetlayer-egress status
```

It names your role, its mode (`log`: nothing refused yet, only recorded; `enforce`: refused), and the presets, hosts
and approved hosts you may reach. "isn't answering" means egress isn't set up on this machine.

## A connection was refused

The signs: `Could not resolve host` (DNS refused), `tlsv1 alert access denied` (HTTPS refused), `CONNECT tunnel
failed, response 403` or a 403 with `X-Fleetlayer-Egress: refused` (through the proxy). See why:

```sh
fleetlayer-egress why api.vendor.io
```

If your work needs that host, ask for it, saying what it's for in one line (the person reads it):

```sh
fleetlayer-egress ask api.vendor.io --why "the vendor's API for the invoice import"
# a port other than 443 and 80
fleetlayer-egress ask db.example.net:5432 --why "the reporting database for the monthly export"
# wait up to ten minutes for the answer
fleetlayer-egress ask api.vendor.io --why "the vendor's API for the invoice import" --wait 10
```

Run it in a session of your role's own node: that node files the request in its own name (no other node can), and the
proxy then looks the answer up. Exit 0: allowed (or already was). Exit 3: waiting for the person; tell them what you
asked for and why, then carry on with other work and try again once they've approved it. Exit 1: refused, or the role
can't ask: report it to your person, don't work around it.

Never try to get around a refusal: no other ports, no other DNS servers, no tunnels or other machines, no encoding
data into allowed requests. Ask once, explain, and wait. Ask only for hosts the task needs, never "just in case".

## Ports other than 443 and 80

Those go through the proxy only: point the tool at it (`eval "$(fleetlayer-egress env)"`), and for SSH use
`ProxyCommand nc -X connect -x 127.0.0.1:3128 %h %p`, or use HTTPS. The host needs its port on the list
(`ask host:port`).

## Setting it up (when the person asks)

Follow `docs/guides/egress.md` in the Fleetlayer repository (fleetlayer.ai/docs/guides/egress). In short, on the proxy's
machine:

1. A user for the proxy, outside every role (`egress`), with a node whose `node.yaml` has
   `{name: egress, command: fleetlayer-egress, grants: [access.check]}`.
2. As that user, `~/.config/fleetlayer/egress.yaml` with each role's `users`, `node`, `presets` and `hosts`, in
   `mode: log`; `fleetlayer-egress check` reads it back. `fleetlayer-egress presets` lists the presets.
3. The firewall, as root: `fleetlayer-egress firewall --settings <file>` prints the rules, `--apply` applies them,
   `--unit` prints a systemd unit to apply them at boot. You'll need the person to run these (root).
4. On each restricted role's node, the probe: `{name: egress-probe, command: fleetlayer-egress, config: {probe: {}}}`.
5. After a week: `fleetlayer-egress log --refused --days 7` and `fleetlayer-egress suggest --days 7`, reviewed with the
   person; then `mode: enforce` for the role and `--apply` again.

Host lists are read again within five seconds of a save; new roles, users or ports need the firewall applied again.
Never put the proxy's user in a role, and never give a restricted user sudo or the `docker` group.
