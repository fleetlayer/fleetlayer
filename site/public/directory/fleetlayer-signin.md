# fleetlayer-signin

One sign-in, with passkeys or your company's provider, for every web service your fleet runs.

| | |
|---|---|
| Kind | Plugin |
| Roles | None (a service the node runs) |
| Status | experimental |
| Ships with Fleetlayer | Yes |
| Licence | Apache-2.0 |
| Maintainer | Fleetlayer |
| Source | https://github.com/fleetlayer/fleetlayer/tree/main/plugins/signin |
| Install | In every Fleetlayer release |
| Command | `fleetlayer-signin` |
| Platforms | Linux, macOS |
| Works with | Fleetlayer 0.3.9 and later |
| Plugin protocol | version 1 |
| Needs | a control plane from the same release (its sign-in token); an always-on node; an HTTPS proxy in front |

## What it does

People sign in once, with a passkey, and use every service that speaks OpenID Connect: the Claude sign-in hub,
pages, and most self-hosted apps. Who may use which service comes from the same access policy as your agents.
Experimental: passkeys, invites, signing in to services, the admin pages, and signing in with Google Workspace or
any OpenID Connect provider work; signing out everywhere at once and Workspace groups as teams come next.

## Setup

It comes with Fleetlayer, so there's nothing else to install. To start from:

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
```

## What it can read and do

Signs people in to your other services, so whoever controls it can sign in as anyone: run it on an always-on machine you administer, behind HTTPS (plain HTTP only on *.localhost). Its control plane token may look up who belongs where, use invites, ask access checks and record sign-ins, and nothing else; fleetlayer-control signin revoke stops it at once. Every client must use PKCE, and sign_in is checked against your access policy before any code is issued. Its folder (dir) holds the token-signing key and everyone's passkeys: keep it readable only by its user, and treat any backup of it like a secret.

More: https://fleetlayer.ai/plugins/signin.md.
