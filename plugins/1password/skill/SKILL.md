---
name: fleetlayer-1password
description: Use 1Password items granted to this machine's role (op:// references) without holding a 1Password token, through Fleetlayer's 1Password plugin, or set the plugin up for the person. Use when a command needs a token or key that lives in 1Password ("op run", "op read", an op:// reference, OP_SERVICE_ACCOUNT_TOKEN missing), when `fleetlayer secrets run` says a reference isn't delivered, or when the person asks to give their agents secrets from 1Password per role.
---

# 1Password items for this role

The person's 1Password service-account token lives with Fleetlayer's 1Password plugin, under a Unix user you can't
read. It delivers the items granted to this machine's role (your Unix user's node). You never need `op` or a
1Password token here: don't ask for one, and don't look for one.

## Use an item

```sh
fleetlayer secrets list                     # what this role has, by reference
GITHUB_TOKEN=op://github/deploy-bot/token fleetlayer secrets run -- gh api user
fleetlayer secrets run --env-file .env -- terraform plan   # .env lines like NAME=op://vault/item/field
```

Where a script ran `op run -- <cmd>`, run `fleetlayer secrets run -- <cmd>` instead: same references, same env files.
Output comes back with secrets replaced by `[REDACTED <reference>]`. Never print, echo, write down or send a value,
and never ask the person to paste one into the chat.

Some roles also get files (an env file, a token file) for tools that read one; `fleetlayer readings
onepassword.receive` shows their paths (`files`) and what's missing (`missing`) for your node.

## When an item isn't there

"not delivered to this node: NAME=op://…" means your role has no grant for it. Check `fleetlayer secrets grants`
(every role, its nodes and its items), then tell the person which reference you need, for what, and which role you're
in (`fleetlayer me` names your node). Grants are per role, never per session, and they're the person's to change.

## Setting it up for the person

`docs/plugins/1password.md` in the Fleetlayer repo (and fleetlayer.ai/docs/plugins/1password/) has every step:

1. A Unix user and node for the source on an always-on machine, `op` installed there, and a service-account token in
   a 0600 file owned by that user. Have the person create the token in 1Password and paste it into that file
   themselves; never handle it in the chat.
2. The source's `source:` config: `token_files` and `roles` (role name, its nodes, the `op://` references it may read).
3. `receive: {}` (or files to write) on each role's node, then approve the first delivery to each
   (`fleetlayer readings sync.status` on the source names the command).
4. Check: `fleetlayer secrets list` on a role's node, and `fleetlayer status` on both for problems.
