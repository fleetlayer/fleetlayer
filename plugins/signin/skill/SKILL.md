---
name: fleetlayer-signin
description: Set up, run and look after Fleetlayer's sign-in service (fleetlayer-signin), which gives people one passkey sign-in for every web service the fleet runs, with who may use what decided by the access policy. Use when the person asks to put a service behind Fleetlayer sign-in, invite someone or take their access away, let someone into a service, sign someone out everywhere, add Google Workspace or another OpenID Connect provider, map its groups to teams, put a service that can't do OpenID Connect behind forward-auth, rotate the signing keys, or move the sign-in service; or when a page says "You're not allowed to use" a service.
---

# Fleetlayer sign-in

`fleetlayer-signin` is a node plugin and an OpenID Connect provider. People sign in to it once, with a passkey or
their company's provider, and from there to every service that's one of its clients. Before it sends anyone back to a
service, it asks the control plane whether the policy gives them `sign_in` on `service:<name>`.

The reference for every setting is `docs/plugins/signin.md` in the Fleetlayer repo (fleetlayer.ai/docs/plugins/signin),
the walk-through is `docs/guides/sign-in.md`, and the design is `docs/sso.md`. Read the reference before changing
settings: this skill covers what to do, not every field.

## What's yours and what's the person's

- **Theirs:** making a passkey (it needs their finger or face on their device), signing in, the provider's side of
  an upstream (an OAuth client made in Google's console or Okta's), and deciding who gets access. Give them the link
  or the steps, then wait for them.
- **Yours:** the settings, the control plane commands, the proxy config, the policy rules they asked for, and
  checking it works.
- You can't sign in as the person, and you never handle their passkeys, client secrets or tokens in chat. A secret
  goes into a file readable only by the service's user, or the person's secrets manager.

## Where things run

- The sign-in service: one always-on machine, behind HTTPS, at an address on a domain the person owns. Passkeys
  belong to that domain, so the address must be settled before anyone makes one.
- The control plane commands (`fleetlayer-control`) run on the control plane's machine.
- The `fleetlayer-signin` commands run on the sign-in service's machine, against its folder (`dir` in its settings).
- If the one you need isn't yours, ask the session that runs there (`fleetlayer msg send`) or tell the person.

## Setting it up

Follow "Set it up" in `docs/plugins/signin.md`. In short:

1. Its token, made on the control plane:

   ```sh
   fleetlayer-control signin token > ~/.config/fleetlayer/signin-token
   chmod 600 ~/.config/fleetlayer/signin-token
   ```

   - It may look people up, use invites, ask access checks and record sign-ins. It also reports passkey uses (which
     unlock changes to people for ten minutes), confirms an admin's passkey for a request, adds people an upstream
     vouches for (`join`) and sets teams from a provider's groups. Keep it from every agent.
   - `fleetlayer-control signin tokens` lists the tokens.
   - `fleetlayer-control signin revoke signin` stops it at once.
2. The plugin goes in the node's config with `dir`, `issuer`, `control_plane`, `token_file`, `listen` and
   `clients`, behind the HTTPS proxy. Add a heartbeat and a health check on `/healthz` in the same change. The
   health check fails when:
   - its folder can't be written
   - the control plane doesn't answer
   - rotating the signing key failed
   - a sign-out hasn't reached a service for an hour
   - reading Google groups has failed for over an hour, or no daily read has worked for a day
3. The first admin's invite: `fleetlayer-control invite add --handle <them> --role admin` prints a code. Give the
   person `https://<sign-in address>/invite?code=<code>`. They choose their name and make a passkey.

## Putting a service behind it

- **It speaks OpenID Connect** (most self-hosted apps): add it to `clients` with its exact `redirect_uris`, a
  `secret_file` (32 or more random characters; the service gets the same secret), `post_logout_redirect_uris`, and a
  `backchannel_logout_uri` if it takes one. Then set the service to the issuer `https://<sign-in address>`. Every
  client must use PKCE. The client's `name` is the service in the policy: `service:<name>`.
- **A service that needs an email** (it asks for the `email` scope; many refuse a sign-in without a verified
  address): the person needs one that someone vouched for. If they sign in through a provider, its verified email
  is used. For a passkey-only person, use
  `fleetlayer-control person email <person id> <address>` (IDs from `fleetlayer-control person list`). That address
  is trusted as verified, so set it only when your person tells you it's theirs.
- **Fleetlayer's own services:** each one's doc names its paths. For pages, see `docs/plugins/pages.md` ("Sign-in");
  its apps sign in through pages. For the Claude hub, see `docs/plugins/claude-auth-manager.md`. Keep a service's
  login key until the person has signed in with their passkey, then remove it.
- **It can't** (a dashboard with no login, a static site): put it behind forward-auth. That needs `forward_auth`
  in the sign-in settings and the proxy's forward-auth pointed at `/forward-auth`. "Services that can't sign people
  in themselves" in the reference has the configs for Caddy, Traefik and nginx. The service receives the person in
  `X-Fleetlayer-Person` and `X-Forwarded-User`. Make sure nothing reaches it except through the proxy, or anyone can
  send those headers.

## People and access

- Who may use a service is a rule in the tenant's policy:

  ```yaml
  allow:
    - who: "team:family"
      can: [sign_in]
      to: [service:pages]
  ```

  - A tenant's admins may sign in everywhere by default; anyone else needs a rule.
  - Before you change a rule, check what it decides:
    `fleetlayer-control acl explain --from person:<handle> --can sign_in --to service:<name>`.
  - A new decision applies within a minute.
- People, roles and teams:
  - Invite: `fleetlayer-control invite add --handle <h> [--role admin] [--team <t>]`.
  - Lost passkeys: `fleetlayer-control invite add --handle <h> --person <id>`, which gets the same person back in.
  - The rest: `fleetlayer-control invite list`, `invite revoke <h>`, `member list`, `member role <h> admin`,
    `member remove <h>`, `team add <t>`, `team member <t> <h> [--remove]`. Add `--tenant <slug>` when there's more
    than one place.
  - Admins can do the same on the dashboard's People and Teams pages.
- **Taking access away:** removing someone or dropping their rule stops new sign-ins at once. Services that ask
  about every request (pages, forward-auth) stop them within a minute, but other services keep an open session until
  it ends, because removing someone sends no sign-out. To end every session now as well, run this on the sign-in
  machine:

  ```sh
  fleetlayer-signin signout --dir <its dir> --person <person id>
  ```

  It tells every client with a `backchannel_logout_uri` and ends forward-auth sessions. A person can do it themselves
  with **Sign out everywhere** on the sign-in page.

## A company's provider

- Google Workspace or any OpenID Connect provider goes under `upstreams` (`kind: google` or `oidc`).
  - The return address to register on the provider's side is `https://<sign-in address>/upstream/<name>/callback`.
  - The person makes that client; you put its secret in `client_secret_file`.
  - `join: {role: member}` lets anyone from the `domain` in on first sign-in. Without it, people join by invite,
    with **Join with Google**.
- Accounts are linked by the provider's issuer and subject, never by email.
- **Groups as teams:** `teams` (Google: a service account's key file with read access to groups and users) or
  `teams_claim` (OIDC), and a `map` from group to team.
  - Only mapped groups count, and no team is ever created: make the teams first.
  - Teams an admin set by hand are kept.
  - See "Groups as teams" in the reference for the grace period and the daily re-read.

## Keys and moving

- The signing key rotates itself every 90 days. After a suspected leak, rotate it now and drop the old key at once:

  ```sh
  fleetlayer-signin keys rotate --dir <its dir> --drop-previous
  ```

  Every token the old key signed stops verifying, but sessions services already hold carry on. To end someone's,
  sign them out too (`fleetlayer-signin signout --dir <its dir> --person <id>`). Tell the person first.
- **Moving** to another machine: stop it, copy its whole `dir` and settings, start it there, and repoint the proxy.
  Keys, passkeys and sessions come along.
- **Changing its address:** put the old one in `previous_issuers` and repoint every client before the old address
  goes. See "Change its address" in the reference. Passkeys keep working only under the same domain.

## When something's refused

- "You're not allowed to use <service>." means the policy gives that person no `sign_in` there. Don't work around
  it. Tell your person, and if they want that person let in, add the rule they ask for.
- A service bouncing back to sign-in in a loop usually means a wrong `redirect_uris` entry (it must match exactly)
  or a secret that differs between the two sides.
