# One sign-in for your self-hosted services

A fleet grows web services: pages, the Claude sign-in hub, a Grafana, a Gitea, an admin tool an agent built last week.
Each one comes with its own accounts and passwords, or none at all behind a VPN. Then adding a person means making
an account in every one, and taking someone's access away means remembering every one.

Fleetlayer's sign-in service gives people one sign-in, with a passkey, for all of them. Who may use which service is a
rule in the same access policy your agents run under, so adding someone, or removing them, happens in one place and
is in the same audit log.

The shape:

- **The sign-in service** (`fleetlayer-signin`, a plugin) on an always-on machine, at an address on a domain you own.
- **Services that speak OpenID Connect** (most self-hosted apps) sign people in through it.
- **Services that don't** sit behind your reverse proxy's forward-auth, which asks the sign-in service about every
  request.
- **One rule per service** in the policy says who may sign in to it.

Status: `fleetlayer-signin` is in use. Passkeys, invites, OpenID Connect services and signing in with your company's
provider arrived in v0.3.9, signing out everywhere (step 6) in v0.3.11, and forward-auth (step 5) and a provider's
groups as teams (step 7) in v0.3.12.

Your agent does the steps on the server. The parts only you can do are marked **You:**. The reference for every
setting is [Sign-in](../plugins/signin.md), and the design behind it is [Sign-in and SSO](../sso.md).

## 1. Choose its address first

Passkeys belong to a domain. A passkey made at `signin.home.example.com` works anywhere under `example.com`, so the
sign-in service can later move to another machine or another name under it and every passkey keeps working. One made
at `localhost` or a bare machine name works only there.

So pick a domain you own, and a name under it for sign-in (`signin.example.com`), before anyone makes a passkey. It
needs HTTPS: put it behind the reverse proxy that already serves your other services. Run it on the always-on machine
your other services lean on, never the laptop ([A server VM for your agents](agents-vm.md)).

## 2. Run the sign-in service

On the control plane's machine, your agent makes the sign-in service's token:

```sh
fleetlayer-control signin token > ~/.config/fleetlayer/signin-token
chmod 600 ~/.config/fleetlayer/signin-token
```

That token may look people up, use invites, ask access checks and record sign-ins, and nothing more. Then it adds the
plugin to the node on the always-on machine, with the services that will sign in as `clients`:

```yaml
plugins:
  - name: signin
    command: fleetlayer-signin
    config:
      dir: ~/.local/state/fleetlayer/signin
      issuer: https://signin.example.com
      control_plane: https://control.example.com    # http://127.0.0.1:<port> on the same machine
      token_file: ~/.config/fleetlayer/signin-token
      listen: 127.0.0.1:7680
      clients: []                                   # step 4
      heartbeat:
        url: https://status.example.com/api/v1/endpoints/signin/external
        token_file: ~/.config/fleetlayer/signin-heartbeat-token
```

Give it a node of its own, under its own Unix user (or in a container), whose folder and token no agent can read.
Its signing keys are worth a sign-in as anyone. The control plane also takes its token's word that an admin
confirmed a request with their passkey ([Sign-in](../plugins/signin.md#confirming-a-request-with-a-passkey)). So an
agent that read them could approve things as you. A machine's agents usually run as other users, so a new user just
for sign-in, with its own node, is enough.

It points the proxy's `signin.example.com` at `127.0.0.1:7680`, restarts the node (`fleetlayer-node install` again,
or restart its service), and checks `https://signin.example.com/healthz`.

**You:** your agent makes an invite with `fleetlayer-control invite add --handle you --role admin` and gives you the
link. Open it, choose your name and make a passkey on your device. Admins can sign in to every service from the start.

## 3. Let people in

Everyone else reaches nothing until a rule lets them in. That's on purpose: a new service is closed until you say who
it's for. One rule per service, in your policy:

```yaml
allow:
  - who: "person:*"            # everyone you've invited
    can: [sign_in]
    to: [service:pages]
  - who: "team:engineering"
    can: [sign_in]
    to: [service:grafana]
```

Invite people from the dashboard's **People** page (**Invite someone** gives a link that works once, for a week),
or have your agent run `fleetlayer-control invite add`. Removing someone there takes their access away: no service
signs them in again, services that check access on each request (pages) and forward-auth stop them within a minute,
and a service that only checked at sign-in keeps its own session until its limit (12 hours for pages and the hub).
When access must end now, also sign them out everywhere (step 6). `fleetlayer-control acl explain` says why someone can or can't reach a service, the same as for
agents ([Access control](../access-control.md)).

## 4. A service that speaks OpenID Connect

Look in the service's settings for "OpenID Connect", "OIDC", "generic OAuth" or "SSO". Your agent:

1. **Makes the service's secret**, unless it's an app that runs in the browser and can't keep one:

   ```sh
   mkdir -p ~/.config/fleetlayer/signin-clients
   openssl rand -hex 32 > ~/.config/fleetlayer/signin-clients/grafana
   chmod 600 ~/.config/fleetlayer/signin-clients/grafana
   ```

2. **Registers it as a client** in the sign-in settings, with the exact return address the service's docs give (no
   wildcards, and it must match to the last character):

   ```yaml
   clients:
     - name: grafana                       # the client ID, and service:grafana in the policy
       redirect_uris: [https://grafana.example.com/login/generic_oauth]
       post_logout_redirect_uris: [https://grafana.example.com/login]
       secret_file: ~/.config/fleetlayer/signin-clients/grafana
   ```

3. **Points the service at it:** the discovery address `https://signin.example.com/.well-known/openid-configuration`,
   the client ID (the `name`) and the secret. Services that want each endpoint separately find them in that document.
   Ask for the scopes `openid profile`.

4. **Turns on PKCE** in the service. The sign-in service requires it of every client, including ones with a secret,
   and some apps have it as an option that's off by default (Grafana's `use_pkce`). Without it, the service gets
   `invalid_request` back instead of a code.

5. Restarts the node and signs in once to check.

The ID token names the person (`sub`, which never changes and is never reused, `name`, `preferred_username`) and their
teams (`groups`). A service that maps a claim to its own roles can use `groups`. Who may do what inside a service is
still the service's own business, unless it asks Fleetlayer ([For a service](../plugins/signin.md#for-a-service)).

## 5. A service that can't sign people in itself: forward-auth

From v0.3.12. For a service with no OpenID Connect at all (a dashboard with no accounts, an admin tool), the reverse proxy asks the
sign-in service about every request and lets it through only for people the policy allows. In the sign-in settings:

```yaml
forward_auth:
  cookie_domain: example.com      # sign-in and every host below are under it, and every name under it is yours
  hosts:
    - host: status.example.com
      service: status             # sign_in on service:status
      tenant: acme
```

The proxy passes the request on with `X-Fleetlayer-Person` and `X-Forwarded-User` set to the person's handle, which
the service can trust. That only holds if a browser can never send those headers itself. Your agent must remove them
from every request on **every site the proxy serves**, not only the protected ones: a forgotten unprotected route to
the same backend lets anyone claim to be anyone.

Two traps in the proxies themselves:

- **Caddy** runs `request_header` after `forward_auth` in its default order, so a plain `request_header
  -X-Forwarded-User` deletes the header forward-auth just set, and the service sees nobody. Put both in a `route`
  block, the strip first.
- **nginx**'s `auth_request` can't pass a redirect on, so the sign-in service answers 401 with a `Location`, and the
  config turns that into the redirect to sign in (`error_page 401 =302 $fl_signin`).

Copy the configs for Caddy, Traefik and nginx from [forward-auth](../plugins/signin.md#services-that-cant-sign-people-in-themselves-forward-auth)
rather than writing them from memory.

## 6. Signing out, everywhere

With single sign-on, each service keeps its own session once you're in. Signing out of one service, or at the sign-in
page, doesn't end the others by itself, and neither does removing someone whose account was taken over.

Give each service that supports it a back-channel logout address, and the sign-in service tells it whenever a session
it signed in through ends:

```yaml
clients:
  - name: pages
    redirect_uris: [https://pages.example.com/_/auth/callback]
    backchannel_logout_uri: https://pages.example.com/_/auth/backchannel-logout
```

Then:

- **Signing out** at the sign-in page, or of a service that passes it on (pages, the hub), signs that browser out of
  every service it signed in to. A service that only ends its own session doesn't reach the others.
- **Sign out everywhere**, on the sign-in page, ends every session the person has, on every browser.
- **For an account that may be in someone else's hands,** your agent runs `fleetlayer-signin signout --dir
  ~/.local/state/fleetlayer/signin --person p_…` on the sign-in machine, then you remove the passkey or the account
  they signed in with.

A service that's down when the logout is sent gets it when it's back: undelivered logouts are kept and retried, and
one that's waited an hour fails `/healthz`, because a sign-out that never arrives leaves someone signed in. A service
with no back-channel logout keeps its session until its own limit: check what that is before relying on it.
Forward-auth services are signed out at once.

## 7. Your company's sign-in, and its groups

A company's people can sign in with their Google Workspace or another OpenID Connect provider (Okta, Entra, Authentik)
instead of a passkey, and the provider's groups can put them in your teams. (Groups as teams arrive in v0.3.12.) Your agent adds an `upstreams` entry
([Signing in with Google Workspace](../plugins/signin.md#signing-in-with-google-workspace-or-another-provider)).
**You:** create the OAuth client in the provider's console, and for Google groups, the service account and its admin
role, following the steps there.

Three things to know before relying on groups:

- **Groups are read at sign-in.** Someone removed from a group, or suspended, would keep the access until they next
  sign in, which may be never. For Google Workspace, the sign-in service reads the directory again every day, and a
  suspended or deleted account loses its teams and is signed out everywhere. Other providers can't be asked between
  sign-ins, so their teams lapse after 30 days without one.
- **Only direct members count.** Someone in a group that's inside the mapped group isn't in it for this. Map each
  group people are directly in.
- **When Google can't be read,** people keep the teams from the last read for 24 hours, and `/healthz` fails after an
  hour, so you hear about it before anyone is locked out.

## 8. Keep it running

- **Watch it.** The heartbeat from step 2 fails whenever `/healthz` would: the folder can't be written, the control
  plane doesn't answer, a sign-out has waited over an hour, or groups can't be read. Add it to your
  [monitoring](monitoring.md).
- **Back it up.** Losing the folder means everyone makes new passkeys and every service gets a new secret. Back up
  `people.json`, `keys/` and `logouts.json` (sign-outs not yet delivered) from it ([Backups](backups.md)).
- **Keys rotate by themselves** every 90 days, and the old key stays published for two weeks so nothing breaks. If a
  key may have leaked: `fleetlayer-signin keys rotate --dir … --drop-previous`, and accept that people sign in to
  services again.
- **Moving it to another machine** is stop, copy the folder, start it there, point the name there. Passkeys and keys
  move with the folder.
- **Changing its address** is harder than it looks. Every service holds the issuer address and checks it in every
  token, and a service posting a code to the token endpoint won't follow a redirect. Set the new `issuer`, put the old
  one in `previous_issuers` with the old name still pointing here, change the issuer in every service's settings,
  and only then turn the old name off. A proxy in front must pass the request's `Host` on (nginx: `proxy_set_header
  Host $host`).

## If you're writing the service yourself

An agent building a web app that signs in here will meet three problems every self-hosted app does. Put them in its
instructions:

- **A `SameSite=Strict` session cookie set on the callback isn't sent on the redirect that follows**, because the
  browser counts the whole chain as the sign-in service's cross-site navigation, and the person lands on the login
  page again. Answer the callback with a small page that moves on (`<meta http-equiv="refresh">`), or use `Lax`.
- **A `form-action 'self'` Content Security Policy blocks the sign-out form's redirect** to the sign-in service, and
  the button does nothing. The same small page that moves on avoids it.
- **A server can't resolve `*.localhost`.** Browsers send every `*.localhost` name to this machine, but a server's
  resolver often doesn't (glibc on Linux without systemd-resolved), so fetching `http://signin.localhost:7680/.well-known/…`
  fails with a DNS error. On one machine, give server-to-server calls `127.0.0.1:<port>`.

And if it takes back-channel logouts itself rather than through a library, it checks each logout token's signature,
`iss`, `aud`, an `iat` within a few minutes, the `events` claim, that there's no `nonce`, and that it hasn't seen the
`jti` before. A loose check lets anyone sign people out, or passes an ID token off as a logout.

## When it doesn't work

- **"You don't have access" after signing in:** no rule gives that person `sign_in` on `service:<name>`, or the
  `name` in the clients list differs from the one in the rule. `fleetlayer-control acl explain` says which.
- **The service says the return address is invalid:** its `redirect_uris` entry differs from what the service sends,
  often by a trailing slash or `http` instead of `https`.
- **The service gets `invalid_request` and no code:** turn PKCE (S256) on in the service.
- **Signed in, then straight back at the service's login page:** its session cookie is `SameSite=Strict` (above).
- **Signing out of one service leaves you signed in to another:** that service has no `backchannel_logout_uri`, or
  it's down and the logout is still waiting (`/healthz` says so after an hour).
- **Behind forward-auth, the service sees nobody signed in** (Caddy): the header strip runs after `forward_auth`. Put
  both in a `route` block, strip first.
- **Someone left a Google group but still has the team:** it reaches their teams within a day. Take them off the team on
  the dashboard's Teams page for now; it lasts until the next read.
- **Passkeys stopped working after a move:** the new address is outside the domain the passkeys were made for. Move
  back under it, or have people make new ones.

Related: [Pages instead of artifacts](pages.md), [Many Claude accounts](claude-accounts.md),
[Access control](../access-control.md).
