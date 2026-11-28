# Sign-in

`fleetlayer-signin` gives people one sign-in, with a passkey, for every web service your fleet runs: the Claude
sign-in hub, pages, and anything else that can sign people in with OpenID Connect (most self-hosted apps can). Who
may use which service comes from the same access policy as your agents, so adding someone, or taking their access
away, happens in one place.

Everything a company needs is built on the same service: single sign-on, teams, separate organisations, audit. How it
fits together, and why, is in [Sign-in and SSO](../sso.md).

Status: in use. Passkeys, invites, signing in to services, signing in with Google Workspace or
another OpenID Connect provider (with its groups as teams), signing out of every service at once, and forward-auth for
services that can't sign people in themselves all work. SAML providers come later.

## Set it up

1. **Make its token** on the control plane, and put it where the sign-in service can read it, readable by that user
   only:

   ```sh
   fleetlayer-control signin token > ~/.config/fleetlayer/signin-token
   chmod 600 ~/.config/fleetlayer/signin-token
   ```

   It may look up who belongs where, use invites, ask access checks and record sign-ins, and nothing else.
   `fleetlayer-control signin revoke signin` stops it at once.

2. **Add the plugin** to a node of its own, under its own Unix user (or in a container), on an always-on machine,
   behind your HTTPS proxy. No agent may be able to read its folder or its token. Its signing keys are worth a
   sign-in as anyone, and the control plane takes its token's word for a passkey confirmation by any admin of any
   place ([below](#confirming-a-request-with-a-passkey)). So an agent that could read them could approve things as
   your admins:

   ```yaml
   plugins:
     - name: signin
       command: fleetlayer-signin
       config:
         dir: ~/.local/state/fleetlayer/signin
         issuer: https://signin.example.com
         control_plane: https://control.example.com
         token_file: ~/.config/fleetlayer/signin-token
         listen: 127.0.0.1:7680
         clients:
           - name: hub
             redirect_uris: [https://hub.example.com/_/auth/callback]
             post_logout_redirect_uris: [https://hub.example.com/_/signed-out]
             backchannel_logout_uri: https://hub.example.com/_/auth/backchannel-logout
             secret_file: ~/.config/fleetlayer/signin-clients/hub
         heartbeat:
           url: https://status.example.com/api/v1/endpoints/signin/external
           token_file: ~/.config/fleetlayer/signin-heartbeat-token
   ```

3. **Invite yourself:** `fleetlayer-control invite add --handle you --role admin` prints a code. Open
   `https://signin.example.com/invite?code=<code>`, choose your name and make a passkey on your device.

4. **Let people in to each service.** Admins can sign in to every service from the start. Anyone else needs a rule
   in your policy for the services they use:

   ```yaml
   allow:
     - who: "person:*"
       can: [sign_in]
       to: [service:pages]
   ```

## Managing people

People, invites and teams are managed on the [dashboard](../dashboard.md) (**People** and **Teams**): everyone with their
role, **Invite someone** (a link to send, which works once, for a week), making someone an admin, removing them, and
the teams. The sign-in service keeps only the pages that must be its own: signing in, accepting an invite, confirming a
request with a passkey, and "no access". Its signed-in page links admins to the dashboard when `dashboard:` is set.

- Who can manage: the admins (`role: admin`), and anyone the policy gives `admin` on `service:dashboard`.
- Every change is made by the control plane, which checks again that it's that person's to make, and is recorded in
  the audit log as them.
- Removing someone, changing a role or taking someone off a team asks for the passkey again when it was last used
  more than ten minutes ago, so a browser left signed in can't do it.
- The last admin can't be removed or made a member: make someone else an admin first.
- An admin can make 20 invites an hour.

## Signing in with Google Workspace, or another provider

A company's people can sign in with their work account instead of a passkey:

```yaml
upstreams:
  - name: acme-google
    kind: google                  # Google Workspace
    domain: acme.example          # only accounts in this Workspace, with a verified email there
    tenant: acme                  # the place it signs people in for
    client_id: "…apps.googleusercontent.com"
    client_secret_file: ~/.config/fleetlayer/signin-upstreams/acme-google
    join: {role: member}          # optional: anyone from the domain joins as a member on first sign-in
  - name: okta
    kind: oidc                    # any OpenID Connect provider
    issuer: https://example.okta.com
    tenant: acme
    client_id: "…"
    client_secret_file: ~/.config/fleetlayer/signin-upstreams/okta
    domain: acme.example          # optional for oidc: only verified emails at this domain
```

`join` is refused on an upstream with no `domain`, unless you write `join: {role: member, anyone_it_vouches_for:
true}`: on a public provider that would let in anyone on the internet.

- **The provider's side:** an OAuth client for a web application, with the return address
  `https://signin.example.com/upstream/<name>/callback`. For Google: Google Cloud console, APIs & Services,
  Credentials, Create OAuth client ID, Web application; an Internal consent screen keeps it to the Workspace.
- The sign-in page offers **Continue with Google**, and the invite page **Join with Google**.
- **Linking:** the first time, by an invite opened with "Join with Google" (or by `join`, for someone from the
  domain). After that, by the account's issuer and subject. Never by email, which can change hands.
- **One place only:** signing in with Acme's Google gives tokens for Acme. Someone who's also in another place signs
  in with their passkey for that one.

### Groups as teams

A provider's groups can put people in your teams, so access follows the groups you already manage:

```yaml
upstreams:
  - name: acme-google
    kind: google
    # … as above, then:
    teams:
      service_account_file: ~/.config/fleetlayer/signin-upstreams/acme-google-sa.json
      map:
        eng@acme.example: engineering
        leads@acme.example: [engineering, leads]
  - name: okta
    kind: oidc
    # … as above, then:
    teams_claim: groups           # the ID token claim listing someone's groups (the default)
    map:
      Engineering: engineering
```

- **Only mapped groups count**, and only into teams that already exist (make them on the dashboard's **Teams**
  page): a group that maps to a missing team is skipped and logged. Group names are matched ignoring case.
- **Teams an admin set are kept.** The provider's own memberships are replaced each time its groups are read. The
  dashboard's Teams page marks them "from acme-google"; taking someone off one lasts until their groups are read again.
- **When groups are read:** at every sign-in through the provider. For Google Workspace, also every day for everyone
  linked to it: leaving a group reaches their teams within a day without a sign-in, and an account that's suspended
  or deleted loses its teams and is signed out everywhere. An OpenID Connect provider can only be read at a sign-in,
  so its teams lapse after `teams_expire_days` (30) without one: leaving a group there reaches teams at the next
  sign-in or at that expiry, whichever comes first.
- **Only direct members.** Someone in a group that's inside `eng@acme.example` isn't in it for this: map each group
  people are directly in.
- **If Google can't be read** (an outage, or the service account lost its access), people keep the teams from the last
  read for `teams_grace_hours` (24), then lose that provider's teams until a read works. Failing for over an hour, or
  no successful daily read for a day, fails `/healthz`.

For Google, the service account, once:

1. In the Google Cloud console, in any project of the Workspace: enable the **Admin SDK API**, then IAM & Admin,
   Service Accounts, create one, and under Keys, Add key, JSON. Put the file at `service_account_file`, readable by
   the sign-in service's user only.
2. In the Google Admin console: Account, Admin roles, create a role with the Admin API privileges **Groups: Read** and
   **Users: Read**, and assign it to the service account's email address.

   Or, with domain-wide delegation instead: Security, Access and data control, API controls, Domain-wide delegation,
   add the service account's client ID with the scopes
   `https://www.googleapis.com/auth/admin.directory.group.readonly` and
   `https://www.googleapis.com/auth/admin.directory.user.readonly`, and set `subject:` under `teams` to an admin's
   address for it to act as.

## Settings

| Setting | What it is |
|---|---|
| `dir` | All its state: `people.json` (passkeys), `keys/` (the token-signing keys), `sessions.json`, `logouts.json` (sign-outs still to deliver), `revocations.json` (people signed out by `signout`). Made readable by this user only. |
| `issuer` | The address people and services use. https, except on this machine (`http://signin.localhost:7680`). |
| `passkey_domain` | What passkeys are made for. Leave it out: it's the issuer's own domain (`example.com` for `signin.home.example.com`), so sign-in can move anywhere under it. |
| `control_plane`, `token_file` | The control plane's address and the token from step 1. On one machine, use `http://127.0.0.1:<port>`: a server often can't resolve `*.localhost` names the way a browser does. |
| `listen` | Where it listens, behind your proxy. |
| `clients` | Each service that signs people in: `name` (as in the policy, `service:<name>`), its exact `redirect_uris`, a `secret_file` for a service that keeps a secret (leave it out for one that can't, such as an app in a browser), `post_logout_redirect_uris`, and `backchannel_logout_uri`, where it's told someone signed out (below). |
| `upstreams` | Providers people may sign in with (above): `name`, `kind` (`google` or `oidc`), `issuer` (oidc), `domain` (required for google), `label`, `tenant`, `client_id`, `client_secret_file`, `join`; groups as teams: `teams` (google: `service_account_file`, `map`, `subject`), `teams_claim` and `map` (oidc), `teams_grace_hours`, `teams_expire_days`. |
| `previous_issuers` | Addresses it had before, still pointed at it while services are repointed (below). |
| `forward_auth` | `cookie_domain` and `hosts` (`host`, `service`, `tenant`), for services behind a proxy's forward-auth (above). |
| `stepup_return_origins` | Origins, such as `https://deck.example.com`, that a passkey confirmation may send the browser back to, besides the sign-in service's own and its clients' (below). |
| `dashboard` | The [dashboard](../dashboard.md)'s address (`https://dashboard.example.com`): the signed-in page sends admins there to manage people, invites and teams. |
| `heartbeat` | Pushed every five minutes; it fails when `/healthz` would. |

## For a service

It's a standard OpenID Connect provider. Point the service at `https://signin.example.com/.well-known/openid-configuration`
with its client ID (the `name`) and secret.

- The authorization code flow, with PKCE (`S256`) required of every service, including ones with a secret.
- `max_age` is honoured: someone whose passkey is older than that many seconds (with ten seconds' slack, so `max_age=0`
  right after signing in doesn't loop) signs in with it again first, and the ID token's `auth_time` is when they last
  used it. The dashboard asks for it before changing people.
- Before a service gets a code, the person's access to it is checked (`sign_in` on `service:<name>`). Someone without
  it sees "You don't have access" and the reason, and the service gets nothing.
- The ID token (RS256, ten minutes) says who: `sub` (the person's ID, never reused), `name`,
  `preferred_username`, `fleetlayer.dev/tenant`, `fleetlayer.dev/principal`, `fleetlayer.dev/role`,
  `fleetlayer.dev/teams`, and `groups` for services that map a claim to their own groups.
- **Email:** a service that asks for the `email` scope gets `email` and `email_verified: true` when the person has an
  address someone vouched for. Many apps need one: some to sign anyone in at all, others to allow only addresses at
  your domain. The address comes from one of two places:
  - **An operator set it** on the control plane: `fleetlayer-control person email <person id> <address>`
    (`fleetlayer-control person list` shows the IDs; `--clear` removes it). The address is trusted as verified, so
    set only one you know is theirs. It's what a passkey-only account needs, and it comes first.
  - **A provider verified it**: the address from the person's last sign-in through a linked upstream, if it said
    `email_verified`.

  If neither applies, no email is given. An address nobody vouched for never is, and nothing signs anyone in by
  email.
- A worked example, pages (`docs/plugins/pages.md`): the client in the sign-in settings is

  ```yaml
  - name: pages
    redirect_uris: [https://pages.example.com/_/auth/callback]
    post_logout_redirect_uris: [https://pages.example.com/_/signed-out]
    secret_file: ~/.config/fleetlayer/signin-clients/pages   # pages reads the same secret (its signin.secret_file)
  ```

  and the pages plugin gets the `access.check` grant, so it can ask which spaces each person may read.
- **Signing out:** give the service a `backchannel_logout_uri` and it's told, with an OpenID Connect back-channel
  logout token (`sid` and `sub`, signed like an ID token, `typ` `logout+jwt`), whenever a sign-in session it signed
  in through ends. Pages and the hub take them at `/_/auth/backchannel-logout`. A service that checks them itself
  must check the signature, `iss`, `aud`, an `iat` within a few minutes, the `events` claim, that there's no `nonce`,
  and that it hasn't seen the `jti` before.
- The token never says what someone may do. For anything finer, a Fleetlayer plugin asks its node
  (`_fleetlayer.dev/access/check`), and any other service asks `POST /api/check` with its client ID and secret, about
  its own `service:<name>` only.

## Services that can't sign people in themselves: forward-auth

For a service with no OpenID Connect support, the reverse proxy in front of it asks the sign-in service about every
request (`GET /forward-auth`), and lets it through only when the person may use it:

```yaml
forward_auth:
  cookie_domain: example.com          # the sign-in service and every host below are under it
  hosts:
    - host: grafana.example.com
      service: grafana                # as in the policy: sign_in on service:grafana
      tenant: acme
```

- **Signed in and allowed:** the request goes through with `X-Fleetlayer-Person` and `X-Forwarded-User` (the
  person's handle), `X-Fleetlayer-Tenant` and `X-Fleetlayer-Teams` (comma-separated, cut at 2 KB), for the service to
  trust.
- **Not signed in:** sent to sign in, then back to the page they asked for (only ever to a host in this list).
- **Not allowed:** "You're not allowed to use grafana."
- **Signing out**, signing out everywhere and the `signout` command end forward-auth at once. Access is checked again
  at most a minute later, so removing someone, or a rule, takes up to a minute to reach forward-auth.
- **The cookie** (`__Secure-fleetlayer-fwd`) is set on `cookie_domain`, so every host under it receives it. It's only
  an opaque ID and can't sign anyone in to a service through OpenID Connect, but a host under that domain run by
  someone else could replay it to your forward-auth hosts: pick a domain whose subdomains are all yours.

**The proxy** must remove any `X-Fleetlayer-*` and `X-Forwarded-User` headers a browser sends, on every site it
serves, not only the protected ones, or a browser could claim to be anyone. `/forward-auth` is best reachable only
from the proxy; nothing breaks if it isn't (it answers only about the caller's own cookie).

Caddy (`route` keeps the order: the headers go before `forward_auth` copies its own in):

```caddyfile
(strip_identity) {
	request_header -X-Fleetlayer-Person
	request_header -X-Fleetlayer-Tenant
	request_header -X-Fleetlayer-Teams
	request_header -X-Forwarded-User
}

grafana.example.com {
	route {
		import strip_identity
		forward_auth 127.0.0.1:7680 {
			uri /forward-auth
			copy_headers X-Fleetlayer-Person X-Fleetlayer-Tenant X-Fleetlayer-Teams X-Forwarded-User
		}
		reverse_proxy 127.0.0.1:3000
	}
}

other.example.com {
	route {
		import strip_identity
		reverse_proxy 127.0.0.1:4000
	}
}
```

Traefik (strip first, then check):

```yaml
http:
  middlewares:
    strip-identity:
      headers:
        customRequestHeaders:
          X-Fleetlayer-Person: ""
          X-Fleetlayer-Tenant: ""
          X-Fleetlayer-Teams: ""
          X-Forwarded-User: ""
    fleetlayer:
      forwardAuth:
        address: http://127.0.0.1:7680/forward-auth
        authResponseHeaders: [X-Fleetlayer-Person, X-Fleetlayer-Tenant, X-Fleetlayer-Teams, X-Forwarded-User]
  routers:
    grafana:
      rule: Host(`grafana.example.com`)
      middlewares: [strip-identity, fleetlayer]
      service: grafana
```

nginx (`auth_request` can't pass a redirect on, so the sign-in service answers 401 with where to go):

```nginx
location = /_fleetlayer/auth {
    internal;
    proxy_pass http://127.0.0.1:7680/forward-auth;
    proxy_pass_request_body off;
    proxy_set_header Content-Length "";
    proxy_set_header X-Original-URL $scheme://$http_host$request_uri;
}

location / {
    auth_request /_fleetlayer/auth;
    auth_request_set $fl_signin $upstream_http_location;
    auth_request_set $fl_person $upstream_http_x_fleetlayer_person;
    auth_request_set $fl_tenant $upstream_http_x_fleetlayer_tenant;
    auth_request_set $fl_teams $upstream_http_x_fleetlayer_teams;
    error_page 401 =302 $fl_signin;
    # Set on every request, so a browser's own values never get through.
    proxy_set_header X-Fleetlayer-Person $fl_person;
    proxy_set_header X-Forwarded-User $fl_person;
    proxy_set_header X-Fleetlayer-Tenant $fl_tenant;
    proxy_set_header X-Fleetlayer-Teams $fl_teams;
    proxy_pass http://127.0.0.1:3000;
}
```

On nginx's other sites, `proxy_set_header X-Fleetlayer-Person "";` (and the same for the other three) removes them.

## Confirming a request with a passkey

Some requests need the person who approves them to confirm with a passkey there and then. Being signed in isn't
enough. Opening an admin window is one ([The admin window](../guides/admin-window.md)). The control plane asks for
it, and the link that comes with the request opens
`https://signin.example.com/stepup?tenant=<place>&approval=<id>&return=<where to go after>`:

- The page shows the request: who asks, what for and on what. If it was decided already, or there's no such request,
  it says so and has no button.
- **Confirm with your passkey** runs a new passkey check, with a fresh challenge and user verification (a fingerprint,
  face or PIN, not only a touch). The passkey must be the signed-in person's own.
- Then the sign-in service tells the control plane, over its token, that this person just confirmed this request.
  The control plane keeps that for five minutes, uses it once, and accepts the approval only from that person, and
  only if they're an admin of the place. It records it in the audit log (`signin.stepup`).
- Afterwards the browser goes to `return` only if it's a path on the sign-in service, or an address at one of three
  origins: its own, a client's (from its `redirect_uris`), or one in `stepup_return_origins`. Anywhere else, the page
  says "you can close this tab", so the link can't be used to send someone to another site.

## Signing out

- **Signing out** of a service, or on the sign-in page, ends the sign-in session and tells every service that
  session signed in to (those with a `backchannel_logout_uri`), so they end theirs too.
- **Sign out everywhere**, on the sign-in page, ends every session the person has, on every browser, and tells every
  service each signed in to.
- **For an account that may be in someone else's hands**, the operator runs, on the sign-in machine as its user:

  ```sh
  fleetlayer-signin signout --dir ~/.local/state/fleetlayer/signin --person p_…
  ```

  The running service signs them out everywhere within seconds. Signing in again afterwards works, so also take the
  passkey or the upstream account away from whoever has it (`Managing people`).
- **How long things last after that:** there are no refresh tokens. Access tokens last ten minutes and are dropped at
  once by signing out everywhere. ID tokens last ten minutes. A service's own session lasts until its logout
  arrives, or its own limit (pages and the hub: 12 hours).
- **A service that's down** gets its logout when it's back: they wait in `logouts.json` and are retried, backing off
  to once an hour, until delivered or the session would have expired anyway. One undelivered for more than an hour
  fails `/healthz`, because a sign-out that never arrives leaves someone signed in.

## Keys

The token-signing key is replaced every 90 days on its own. The previous key stays in the JWKS for two weeks, so
tokens it signed still verify, then it's deleted. Services fetch the JWKS again when a token names a key they don't
have, so nothing changes for them.

If the key may have leaked, replace it now and take the old one out at once:

```sh
fleetlayer-signin keys rotate --dir ~/.local/state/fleetlayer/signin --drop-previous
```

The running service uses the new key within a minute. Every token the old key signed stops verifying, so services
whose sessions check the token again send people to sign in; sessions they already hold carry on until they end
(sign everyone out with `signout` too, if that matters). Without `--drop-previous` it's an ordinary early rotation.

## Keep it running

- **Monitoring:** `GET /healthz` is 200 while the folder can be written, the control plane answers, the key can be
  rotated, and no sign-out has waited more than an hour; otherwise it's 503 with the reasons. The heartbeat says the
  same every five minutes.
- **Back it up:** losing the folder means everyone makes new passkeys and every service gets a new secret. Back up
  `people.json` and `keys/`. Sessions can be lost: people sign in again. Losing `logouts.json` loses sign-outs not yet
  delivered.
- **Move it:** stop it, copy the folder and its settings, start it on the other machine and point the address there.
  The key and the passkeys move with the folder, so tokens services hold stay valid and passkeys keep working.
- **Change its address:** passkeys keep working anywhere under the passkey domain. Set the new `issuer`, and put the
  old address in `previous_issuers` with the old name still pointing at the service: there, the JWKS is served as
  is, and everything else is redirected to the same path at the new issuer (only ever there). Then change the issuer
  in every service's settings (the hub's and pages' `signin.issuer`) before turning the old name off: a service
  won't follow a redirect when it posts a code to the token endpoint, and new tokens name the new issuer. Your proxy
  must pass the request's `Host` on (Caddy does; nginx needs `proxy_set_header Host $host`). Sessions on the old
  address end: people sign in again.
- **On this machine only** (`localhost`, or a machine's name with no domain of yours), the invite page warns that
  passkeys made there stop working if sign-in moves. Use a domain you own to avoid that.
