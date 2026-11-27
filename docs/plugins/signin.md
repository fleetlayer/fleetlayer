# Sign-in

`fleetlayer-signin` gives people one sign-in, with a passkey, for every web service your fleet runs: the Claude
sign-in hub, pages, and anything else that can sign people in with OpenID Connect (most self-hosted apps can). Who
may use which service comes from the same access policy as your agents, so adding someone, or taking their access
away, happens in one place.

Everything a company needs is built on the same service: single sign-on, teams, separate organisations, audit. How it
fits together, and why, is in [Sign-in and SSO](../sso.md).

Status: experimental. Passkeys, invites, signing in to services and the admin pages work. Signing out of every service
at once, and Google Workspace as a way to sign in, come next.

## Set it up

1. **Make its token** on the control plane, and put it where the sign-in service can read it, readable by that user
   only:

   ```sh
   fleetlayer-control signin token > ~/.config/fleetlayer/signin-token
   chmod 600 ~/.config/fleetlayer/signin-token
   ```

   It may look up who belongs where, use invites, ask access checks and record sign-ins, and nothing else.
   `fleetlayer-control signin revoke signin` stops it at once.

2. **Add the plugin** to the node on an always-on machine, behind your HTTPS proxy:

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
             redirect_uris: [https://hub.example.com/auth/callback]
             secret_file: ~/.config/fleetlayer/signin-clients/hub
         heartbeat:
           url: https://status.example.com/api/v1/endpoints/signin/external
           token_file: ~/.config/fleetlayer/signin-heartbeat-token
   ```

3. **Invite yourself:** `fleetlayer-control invite add --handle you --role admin` prints a code. Open
   `https://signin.example.com/invite?code=<code>`, choose your name and make a passkey on your device.

4. **Let people in to each service** in your policy. With no rule, people have only their own things, so give the
   services you want them to use:

   ```yaml
   allow:
     - who: role:admin
       can: [sign_in]
       to: ["service:*"]
   ```

## Managing people

Admins open `https://signin.example.com/admin`: the people, with their role and a button to remove them, and **Invite
someone**, which gives a link to send (it works once, for a week). Teams, and the services that sign in here, are
under **Advanced**.

- Who can manage: the admins (`role: admin`), and anyone the policy gives `admin` on `service:signin`.
- Every change is made by the control plane, which checks again that it's that person's to make, and is recorded in
  the audit log as them.
- Removing someone, changing a role or taking someone off a team asks for the passkey again when it was last used
  more than ten minutes ago, so a browser left signed in can't do it.
- The last admin can't be removed or made a member: make someone else an admin first.
- An admin can make 20 invites an hour.

## Settings

| Setting | What it is |
|---|---|
| `dir` | All its state: `people.json` (passkeys), `keys/` (the token-signing key), `sessions.json`. Made readable by this user only. |
| `issuer` | The address people and services use. https, except on this machine (`http://signin.localhost:7680`). |
| `passkey_domain` | What passkeys are made for. Leave it out: it's the issuer's own domain (`example.com` for `signin.home.example.com`), so sign-in can move anywhere under it. |
| `control_plane`, `token_file` | The control plane's address and the token from step 1. |
| `listen` | Where it listens, behind your proxy. |
| `clients` | Each service that signs people in: `name` (as in the policy, `service:<name>`), its exact `redirect_uris`, a `secret_file` for a service that keeps a secret (leave it out for one that can't, such as an app in a browser), and `post_logout_redirect_uris`. |
| `heartbeat` | Pushed every five minutes; it fails when the folder can't be written or the control plane doesn't answer. |

## For a service

It's a standard OpenID Connect provider. Point the service at `https://signin.example.com/.well-known/openid-configuration`
with its client ID (the `name`) and secret.

- The authorization code flow, with PKCE (`S256`) required of every service, including ones with a secret.
- Before a service gets a code, the person's access to it is checked (`sign_in` on `service:<name>`). Someone without
  it sees "You don't have access" and the reason, and the service gets nothing.
- The ID token (RS256, ten minutes) says who: `sub` (the person's ID, never reused), `name`,
  `preferred_username`, `fleetlayer.dev/tenant`, `fleetlayer.dev/principal`, `fleetlayer.dev/role`,
  `fleetlayer.dev/teams`, and `groups` for services that map a claim to their own groups.
- The token never says what someone may do. For anything finer, a Fleetlayer plugin asks its node
  (`_fleetlayer.dev/access/check`), and any other service asks `POST /api/check` with its client ID and secret, about
  its own `service:<name>` only.

## Keep it running

- **Monitoring:** `GET /healthz` is 200 while the folder can be written and the control plane answers, and the
  heartbeat says the same every five minutes.
- **Back it up:** losing the folder means everyone makes new passkeys and every service gets a new secret. Back up
  `people.json` and `keys/`. Sessions can be lost: people sign in again.
- **Move it:** stop it, copy the folder and its settings, start it on the other machine and point the address there.
  The key and the passkeys move with the folder, so tokens services hold stay valid and passkeys keep working.
- **On this machine only** (`localhost`, or a machine's name with no domain of yours), the invite page warns that
  passkeys made there stop working if sign-in moves. Use a domain you own to avoid that.
