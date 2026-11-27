# Fleetlayer sign-in: one sign-in for every service

Status: design (27 November 2026). Steps 1a (people, tenants, teams, memberships, invites, node owners on the control
plane), 1b (decisions in the owning tenant, people in the policy, the check call), 1c (the sign-in token and its
routes), 2a (the sign-in service: OpenID Connect provider, passkeys, invites, the move test), 2b (the admin pages),
2c (back-channel logout, signing out everywhere, key rotation, `previous_issuers`), 5 (forward-auth), 3 (the hub and pages sign in with Fleetlayer, through the shared `crates/fleetlayer-oidc`) and 4a (Google Workspace
and generic OIDC upstreams, linking by issuer and subject, `join`) are built; the rest follows the steps at the end,
each one shippable. Workspace groups to teams (4b) come later.

People sign in once, with Fleetlayer, to every web service the fleet runs: the Claude sign-in hub, pages, and any
other service someone chooses to run. Who may use what comes from the same access policy that decides what agents may
do (`docs/access-control.md`), with the same `deny`/`only`/`allow`, the same `acl explain` and the same audit log.

- **Fleetlayer is an OpenID Connect provider** for services. A service that speaks OIDC is a client; one that doesn't
  sits behind its reverse proxy's forward-auth (Traefik's `forwardAuth`, Caddy's `forward_auth`, nginx
  `auth_request`), checked against the same sign-in.
- **Where people prove who they are is pluggable:** Fleetlayer's own accounts (passkeys, invites by link), or the
  company's own provider (Google Workspace first, then any OIDC provider: Okta, Microsoft Entra,
  Authentik, Keycloak, Pocket ID...), with Fleetlayer passing the sign-in through and mapping the person to its own
  people and teams. SAML later, if a library makes it cheap.
- **Permissions stay in Fleetlayer's policy**, never in each service.

## The model: tenants, teams, people

Fleetlayer serves many people in one deployment, from a hosted service used by many companies and individuals down to
one household on one server:

```
deployment (one control plane, one sign-in service)
├── people         global: one account each
│   └── nodes      a person's own devices (or a team's or tenant's shared machines)
└── tenants        an organisation or an individual customer; a household is a small tenant
    ├── teams      many per tenant, nested (Engineering > Platform)
    ├── members    people, and agents, each admitted by the tenant, with a role
    ├── spaces, services' data    owned by the tenant's members or teams
    └── one policy, one audit log
```

- **A person is global.** One account, one sign-in, one stable ID (`sub`, `p_` and random characters) across the
  whole deployment, with any number of ways to prove it's them: passkeys, Google, Okta...
- **Membership is per tenant and per team, with a role in each.** The same person can be an admin of their own
  household's tenant and a member of their employer's, and in several teams in each. In each tenant they have a
  **handle**, unique in that tenant, which its policy uses: `person:sam` in one tenant and `person:sbrown` in another
  can be the same person. A handle is never reused for someone else while rules may still name it.
- **A session or token is for one tenant at a time.** Signing in picks the tenant (the only one, or the person
  chooses); a service knows which tenant a request is for (its address, or its client registration), and switching
  tenant is explicit: a new authorization for the other tenant. Tokens carry the tenant.
- **Data never crosses tenants.** Policies, teams, owners, audit logs and checks are per tenant. A tenant's admin has
  nothing in another tenant; a person's membership of one tenant shows nothing about their others.
- **Admins and members.** A tenant's admins manage its members, teams, invites, owners and policy. **Being an admin
  doesn't open other people's things**: an admin can't read a member's inbox or pages unless the policy shares them,
  and every change an admin makes is in the audit log. A team's maintainers manage that team's members.

### Who owns what, and agents as members

- **A node belongs to a person, a team or a tenant.** A person's node is their device: every session, shell and
  plugin on it (`<tab>@<node>`, `-@<node>`, `plugin:<name>@<node>`) is that person's agent. A team's or tenant's
  node (a shared build VM) belongs to that one tenant: its agents are that tenant's members, and members of no other
  tenant unless that tenant admits them explicitly. Nodes are named once per deployment.
- **Whoever enrols a node isn't its owner for that.** Enrolling one for a team needs a maintainer of the team, for a
  tenant an admin of it; who enrolled it is recorded. Admins transfer ownership (person to team, team to team), with
  an audit entry in every tenant concerned.
- **When a person leaves a tenant**, their own nodes' agents lose that tenant and keep working in their others; team
  and tenant nodes are untouched, even ones they enrolled. Nothing is deleted. **When a person is removed from the
  deployment**, they leave every tenant and their own nodes' tokens stop working; team and tenant nodes carry on.
- **Agents are members too.** Each tenant admits agents as it admits people, by its admins, into its member list and
  under its policy: one agent (`finances@laptop`), every agent of a node (`*@laptop`), or a pattern. One agent can be a
  member of several tenants; most are members of one. An agent's owner must be a member of the tenant that admits it.
- **Every access is decided, and audited, in the tenant that owns what's accessed**, with that tenant's policy for
  that subject. A subject that isn't a member of that tenant is refused before any rule is read. Nothing crosses
  tenants on its own: an agent sees a tenant's data only because the tenant admitted it.
- **Between agents** (messages, reading a session, tools, terminals, sync), each side's tenants guard their own: the
  action is allowed only if every tenant either agent is a member of allows it, and it's recorded in each. A tenant
  both agents belong to decides it as usual; a tenant only one of them belongs to sees the other as an outside agent
  (`outside` in a rule's `to` or `who`), so a company can let its agents message only its own members.
- **A tenant sees which of its member agents are also members elsewhere** (a count, never where), and can refuse them:
  `agents_elsewhere: deny` in its policy refuses every action by or to its member agents that are members of another
  tenant.
- **Isolation by Unix user stays a choice**, for an agent that reads untrusted input (its own user and node, as
  before), not a requirement of tenancy.

Things in a tenant belong to a member or to a team of it:

- **spaces of pages**, and **services' data**: everything under `service:<name>/<owner>/…` belongs to that owner
  (Quietdeck's inbox for David is `service:quietdeck/david/inbox`; a team's shared inbox is
  `service:quietdeck/team:support/inbox`). Each belongs to the tenant whose service holds it.

Owners and member agents are kept on the control plane (not secret). The policy file can say owners for patterns too
(`owners:`, for spaces before an admin page does); the most specific pattern wins.

- **A person sees their own and their teams'; sharing is explicit.** People's default is `own`: a person may do
  anything to what they own and read what their teams own, and nothing else until a rule shares it. A person's own
  agents count as theirs: `to: [owner]` means "whatever the subject's owner owns", so `allow: {who: "*", can: [read],
  to: [owner]}` lets each agent read its person's things in a tenant that's `default: deny`.
- A rule names someone's things with `owned-by:person:<handle>` or `owned-by:team:<name>`, in `who` ("an agent of
  `person:david`") or in `to` (their agents, spaces and data).
- **Multi-user services** learn whose request it is, and for which tenant, from the sign-in (the ID token's `sub`,
  `fleetlayer.dev/tenant` and `fleetlayer.dev/principal`), key their data by tenant and owner, and ask before acting
  on it (`read` on `service:quietdeck/david/inbox`): the owner passes, someone it's shared with passes, an admin
  doesn't by default. An agent acting for its owner is checked the same way, as itself, in the data's tenant.

## Where it runs, and why (the open point)

Two kinds of information, kept apart:

- **Who belongs where** (people's IDs, handles, tenant and team memberships and roles, member agents, teams, nodes'
  and things' owners, invites waiting
  to be used): not secret. **It lives on the control plane**, per tenant, next to the policy that uses it and the
  audit log that records its changes. Admins manage it there (CLI, API, and the admin pages the sign-in service shows).
- **Credentials** (passkey registrations, links to upstream accounts, upstream client secrets, people's sessions, and
  above all the **token-signing key**: whoever holds it can sign in as anyone to every service): **never on the control
  plane**, which holds no credentials and may be hosted by someone else. Emails stay out of it too.

**Decision: sign-in is its own component, `fleetlayer-signin`**, one per deployment, serving all its tenants, on
machines whoever runs the deployment controls: for a self-hoster, an always-on machine of theirs (run by a node as a
plugin, like the Claude sign-in hub, under its own Unix user, behind a private HTTPS proxy, with `/healthz` and a
heartbeat); for a hosted deployment, the operator's sign-in servers, kept apart from the control plane's database. It
proves who someone is, maps that to a global person ID, asks the control plane which tenants and teams that person is
in, and issues tokens for one tenant at a time.

It reaches the control plane with a **sign-in token** of its own (`fleetlayer-control signin token`), separate from
node tokens and from tenants' admin tokens: it may read memberships, redeem invites, record sign-ins and ask checks,
for every tenant, and nothing else. No node can claim to be it.

Why this split:

- The control plane can't mint a sign-in: the worst a control plane that lies can do is answer a check or a
  membership wrongly. And whoever runs the sign-in service can't change who belongs where without the audit log
  showing it.
- It's one design for a hosted and a self-hosted deployment. Someone running everything on one machine runs both
  there.
- **A tenant that won't trust the operator's sign-in** connects its own provider (Okta, Google Workspace, or its own
  self-hosted `fleetlayer-signin` as a plain OIDC provider) as an upstream, so its people's credentials stay with it.
  The operator's sign-in still issues the tokens services see; a tenant that wants no operator in that path runs its
  own deployment. That's said plainly in the hosted plan's terms.

## People, teams, groups and targets in the policy

| Kind | Written as | Example |
|---|---|---|
| A member of this tenant | `person:<handle>` | `person:sam` |
| A team of this tenant (with its sub-teams) | `team:<name>` | `team:engineering` |
| A group of the policy's own (agents, people, teams) | `group:<name>` | `group:family` |
| What someone owns | `owned-by:person:<handle>`, `owned-by:team:<name>` | `owned-by:team:support` |
| A web service | `service:<name>` | `service:hub` |
| A member's or team's data in a service | `service:<name>/<owner>/<path>` | `service:quietdeck/david/inbox` |
| A page, or a space of pages | `page:<space>/<path>` | `page:handbook/*` |
| An app in a space (a static site on its own address) | `app:<space>/<name>` | `app:home/budget` |
| The tenant's admins | `role:admin` | |

- A handle and a team name are lowercase slugs (`[a-z0-9][a-z0-9._-]{0,63}`). They have no `@`, so they can't
  clash with `<tab>@<node>`.
- **A pattern matches people only if it says so:** `person:*`, `person:d*`, `team:…`, `role:admin`, or a group with
  people in it. A bare `*` in `who` keeps meaning every agent, so policies written before people existed don't hand
  them anything.
- **Groups can name teams:** `groups: {staff: [team:engineering, team:support, "*@office"]}`. A team includes its
  sub-teams' members.
- **People have their own default, `own`** (above), whatever the policy's `default:` (which keeps deciding for
  agents). So adding someone to a tenant gives them their own things and their teams', and nothing of anyone else's,
  even in a fleet that's `default: allow` for its agents. `default_people: deny` (not even their own, until a rule
  allows it) and `allow` are the other values. `deny` rules still win over ownership.
- **A tenant's admins sign in to its services by default**, and administer them (`sign_in` and `admin` on
  `service:<name>`, never on someone's data under it, `service:<name>/<owner>/…`). So the first admin can use the hub,
  pages and the sign-in admin pages with no policy written, and members get a service once a rule gives it to them.
  A `deny` rule, or `default_people: deny`, takes it away.

### Verbs

| Verb | Target | Means |
|---|---|---|
| `sign_in` | `service:<name>` | May sign in to this service at all. Checked before a token is issued, and on every forward-auth request. |
| `admin` | `service:<name>` | May use the service's admin features. `admin` on `service:signin` manages the tenant's members and teams. |
| `read` | `page:<space>/<path>` | May see this page (pages asks per request). The existing `read` verb, for a new kind of target. |
| `read` | `app:<space>/<name>` | May open this static app. Owners and the default are as for spaces. Pages signs people in to its apps itself (it's the client `service:pages`) and checks this before letting anyone in. |
| `write` | `page:<space>/<path>` | May push to this space (`fleetlayer pages push`, for agents without file sync). Reserved here, defined by pages. |

**Approvals:** `admin` and `write` join Fleetlayer's default `ask:` rules (approved once, held 30 days, like the first
`use_tool`), since they change things; `sign_in` and `read` never ask by default, or every page load would.

A service asks with these verbs about any target it defines (`service:grafana` with `admin`, say): the check call
below takes `service:`, `page:` and `app:` targets. The list of verbs stays exhaustive in the code, so a policy can't
name one Fleetlayer doesn't know.

A policy for a company tenant:

```yaml
version: 2
default: allow            # agents
default_people: own       # people: their own things and their teams' (the default when left out)

groups:
  staff: [team:engineering, team:support]

allow:
  - who: role:admin
    can: [sign_in, admin]
    to: ["service:*"]
  - who: group:staff
    can: [sign_in]
    to: [service:pages]
  - who: group:staff
    can: [read]
    to: ["page:handbook/*"]

deny:
  - who: "person:*"
    except: [role:admin]
    can: [sign_in]
    to: [service:hub]               # the hub holds Claude sign-ins: admins only, whatever else allows
    why: The hub holds every Claude sign-in
```

## A worked example

One deployment, its control plane run by person A on their home server. Two tenants: **Acme**, a company, and
**Home**, a family, each with its `everyone` team.

- **A** is in both: an admin of Acme (handle `person:a`) and of Home (handle `person:a` there too, by chance).
- **B**, A's partner, is in Home only, as an admin.
- **C**, A's colleague, is in Acme only, as an admin. C may later make a family tenant of their own.
- A's node `laptop` runs A's agents: `billing@laptop` (Acme work), `recipes@laptop` (Home), and `finances@laptop`,
  which needs both A's personal finances and Acme's. C's node `c-vm`, a VM on C's own home server, runs C's agents.

| Situation | What happens | Why |
|---|---|---|
| C connects `c-vm` | `fleetlayer-control node add c-vm --owner c`; Acme's admins admit `*@c-vm` | A node belongs to its owner. The tenant admits its agents; the control plane can be run by someone outside the tenant. |
| A's agents on one node | Acme admits `billing@laptop` and `finances@laptop`; Home admits `recipes@laptop` and `finances@laptop` | Agents are members, admitted per tenant. |
| `finances@laptop` reads Acme's ledger, then Home's budget | Each read is decided in its own tenant (Acme's policy, then Home's) and audited there | Decided in the tenant that owns the data. |
| `recipes@laptop` asks for Acme's ledger | Refused: not a member of Acme | Membership comes before any rule. |
| `finances@laptop` messages `recipes@laptop` | Home (both members) decides as usual; Acme (finances only) sees a message to an outside agent and must allow it too | Each side's tenants guard their own members. |
| Acme sets `agents_elsewhere: deny` | `finances@laptop` loses Acme; Acme's admins see "1 member agent is also a member elsewhere", not where | A tenant can refuse cross-tenant agents. |
| A opens pages | The sign-in asks "Acme or Home?"; the page shows which, with a switch | A person's token is for one tenant; switching is explicit. |
| B tries an Acme service | "You're not a member of Acme"; no token | Membership is per tenant. |
| A, a Home admin, looks at Acme's settings | Allowed only because A is also an Acme admin; while signed in for Home, nothing of Acme shows | Roles are per tenant. |
| C asks for Home's pages or B's inbox | No token, and no check can even name them | Data never crosses tenants. |
| B shares a Home space with A | A rule in Home's policy (later, B's sharing page): `allow: {who: person:a, can: [read], to: ["page:recipes/*"]}` | Owner-scoped by default, sharing explicit. |
| C enrols a shared build VM for Acme's engineering team | `node add build-vm --owner team:engineering`, needing C to be a maintainer of the team; `enrolled_by` records C | A team's node belongs to Acme alone. |
| C leaves Acme | C's own `c-vm` agents lose Acme (and keep C's other tenants); `build-vm` stays with the team and keeps working | Shared machines aren't tied to whoever started them. |
| C makes a family tenant | C is its first admin; A sees nothing of it; `c-vm`'s agents join it only if C admits them there | Tenants are separate; one person, many memberships. |

## How a service asks

Two ways, used together:

1. **At sign-in, by the sign-in service.** Before it hands a service an authorization code, `fleetlayer-signin`
   checks `sign_in` on `service:<client>`. Refused means no token: the person sees "You don't have access to Hub" and
   the rule that says so. So a service that only needs "may this person use me" needs no code beyond plain OIDC.
2. **Per request, by the service.** For anything finer (which spaces, which pages, admin pages), the service asks:
   - a Fleetlayer plugin (pages, the hub) asks its own node: `_fleetlayer.dev/access/check {tenant, who, can, to}`
     → `{allowed, rules, reason}`, with grant `access.check`, `tenant` being the tenant that owns what's asked about
     (the space's, the data's). The node forwards it as `link/access_check`; the control plane answers only if the
     asking node's `-@<node>` is a member of that tenant, so a service can ask only about tenants that admitted it;
   - any other service asks the sign-in service: `POST /api/check` with its client credentials, `{tenant, person,
     can, to}`; the sign-in service asks the control plane with its sign-in token.

   A service may cache an answer for up to 60 seconds. Every check is decided with the tenant's compiled policy and
   written to its audit log like any other decision.

**The ID token** says who, never what they may do (the policy can change after the token was issued):

```json
{
  "iss": "https://signin.example", "aud": "pages", "sub": "p_4f0c…",
  "preferred_username": "a", "name": "A", "email": "…",
  "fleetlayer.dev/tenant": "acme",
  "fleetlayer.dev/principal": "person:a",
  "fleetlayer.dev/role": "admin",
  "fleetlayer.dev/teams": ["engineering"],
  "groups": ["acme", "acme/role:admin", "acme/engineering", "acme/engineering/platform"],
  "amr": ["hwk"], "auth_time": 1795…, "exp": "+10 minutes"
}
```

- `sub` is the person's global ID, the same in every tenant, never reused. `preferred_username` and
  `fleetlayer.dev/principal` are their handle in this token's tenant. Changing a handle isn't offered (remove and
  re-add), so a rule can't silently start meaning someone else.
- Services key data by tenant and owner, never by `sub` alone, so nothing can leak between a person's tenants.
- `role` and `teams` are for display and for services that can only read claims; services should still ask.
- `groups` is the same in a flat array of strings for clients that map a claim to their own groups (Proxmox VE's
  OpenID Connect realm, Grafana, most others): the tenant, `<tenant>/role:<role>`, and each team as
  `<tenant>/<team>` with its parents' path, for the token's tenant only.
- To ask for another tenant, a service sends `fleetlayer.dev/tenant=<id>` with the authorization request; without it,
  a person in several tenants chooses.
- Signed RS256 (every OIDC client must support it), keys rotated every 90 days, the previous key kept in the JWKS for
  two weeks. Standard endpoints: discovery, JWKS, authorize (code flow with PKCE required), token, userinfo,
  end-session, and back-channel logout to clients.

**Clients** are registered per tenant (a tenant's admin adds them; for a self-hoster, the sign-in plugin's config):
name (the `service:` name), redirect URIs, and a secret (confidential clients) or none (public clients with PKCE). A
client for a service that serves many tenants (a hosted pages) is registered once for the deployment and names the
tenant per request. No dynamic registration.

## Sessions and signing out

- **The sign-in session** is a cookie on the sign-in service's own origin (`__Host-`, HttpOnly, Secure, SameSite=Lax
  so redirects from services carry it), lasting 30 days from the last passkey (or upstream) sign-in, kept hashed in
  the sign-in folder so a restart doesn't sign everyone out.
- **A service's session** is the service's business, but it should last no longer than 12 hours and re-check access
  per request or at least every few minutes. Silent re-authentication (`prompt=none`) gets a fresh ID token without a
  passkey while the sign-in session holds.
- **Signing out** (the person, from any service or the sign-in page) ends the sign-in session and sends an OIDC
  back-channel logout to every service the session signed in to.
- **Removing someone from a tenant** ends their tokens for that tenant at once (per-request checks refuse a principal
  who's no longer a member, even before a service handles the back-channel logout); their other tenants are untouched.
  **"Sign out everywhere"** (the person themselves, or the deployment's operator for a compromised account) ends all
  their sessions.
- If the control plane can't be reached, no new service sign-ins are issued (a check that can't be made is a refusal)
  and per-request checks fail closed. Sessions services already hold carry on until they re-check.

## Signing in: passkeys, invites, and the admin pages

- **What the sign-in service keeps per person:** their global ID, display name, email (optional, for the ID token),
  passkeys and upstream links. Memberships, handles, teams and roles it asks of the control plane at each sign-in.
- **Invites by link:** a tenant's admin invites someone (handle, role, teams); the control plane keeps the invite (the
  code's hash, 7 days). Whoever opens the link signs in with their existing account (a new membership) or creates one
  by registering a passkey (a new person). **Recovery** is an invite too: a person who lost their passkeys gets a new
  link from an admin of any of their tenants, which lets them register a new passkey and, if they choose, revoke the
  old ones. (A tenant admin can recover only the account's sign-in, never see its other tenants.)
- **Passkeys** (WebAuthn, with [webauthn-rs](https://github.com/kanidm/webauthn-rs), MPL-2.0, used unmodified): a
  person can register several; discoverable credentials, so signing in needs no name typed. The relying party ID is
  the sign-in service's host.
- **The admin pages** (`/t/<tenant>/admin`, need `admin` on `service:signin` in that tenant): members, teams, roles,
  invites, clients; for the operator, `/operator` (people, sessions, the upstreams and the deployment's own settings).
- **CLI:** `fleetlayer-control tenant add`, `member add|remove|role`, `team add|remove|member`, `invite`, `node add
  --tenant --owner`, run with the control plane's admin token; `fleetlayer-control signin token` makes the sign-in
  service's token. The first admin of a deployment: `fleetlayer-control --tenant home invite add --handle a --role admin`
  prints their link.
- **The control plane's side** (built in 1c), `POST` with the sign-in token as a bearer token; no node, API or admin
  token reaches these, and the sign-in token reaches nothing else:
  - `/v1/signin/person {person}` → `{id, display_name, tenants: [{tenant, slug, name, handle, role, teams, groups}]}`:
    what each tenant's ID token says (`teams` with the teams above them and without `everyone`, `groups` the flat
    claim). 404 for an unknown ID.
  - `/v1/signin/invite_redeem {code, display_name | person}` → `{tenant, person, handle, recovery}`: a new account
    (its name) or someone already signed in (their ID).
  - `/v1/signin/check {tenant, who, can, to}` → `{allowed, rules, reason}`, `who` a person's ID or `person:<handle>`:
    decided and audited in that tenant like a node's `link/access_check`.
  - `/v1/signin/event {tenant, person, kind, client?, detail?}`: `kind` is `signed_in`, `signed_out`, `refused`,
    `passkey_added`, `passkey_removed` or `upstream_linked`, `detail` one line (how, or why refused, never a
    credential). Recorded in the tenant's audit log as `signin.<kind>`, with the token's name.
  - `/v1/signin/admin/view {tenant, as}` and `/v1/signin/admin/<action> {tenant, as, …}` (`invite`,
    `invite_revoke`, `member_role`, `member_remove`, `team_add`, `team_remove`, `team_member`), for the admin pages.
    The control plane checks `as` may manage the tenant (an admin, or `admin` on `service:signin`) on every request,
    and records each change as that person (`person:<handle> (signin:<token>)`). The sign-in service adds a passkey
    step-up (ten minutes) before removals and role changes, a CSRF token, and 20 invites an hour per admin.
  - `fleetlayer-control signin token [--name signin]` makes (or replaces) the token and prints it once; `signin
    tokens` lists them; `signin revoke <name>` stops the service using it at once.

  Every membership change (invited, joined, removed, role or team changed) is written to that tenant's audit log,
  whoever made it.

## Upstream providers

Upstreams belong to the deployment (their secrets are in the sign-in service's config) and are offered to the tenants
that use them: Acme's Google Workspace for Acme's people.

```yaml
upstreams:
  - name: acme-google
    kind: google                      # Google Workspace: OIDC, plus `hd` checked against the domain
    domain: acme.example
    tenant: acme                      # who this upstream signs people in for
    client_id: "…apps.googleusercontent.com"
    client_secret_file: /etc/fleetlayer-signin/acme-google-secret
    teams:                            # optional: read Workspace groups with a service account (Admin SDK)
      service_account_file: /etc/fleetlayer-signin/acme-google-sa.json
      map: {"eng@acme.example": engineering}
    join: {role: member}              # optional: anyone from the domain may join Acme as a member
  - name: okta
    kind: oidc                        # any OIDC provider
    issuer: https://example.okta.com
    tenant: acme
    client_id: "…"
    client_secret_file: /etc/fleetlayer-signin/okta-secret
    teams_claim: groups
    map: {Engineering: engineering}
```

- An upstream identity (issuer and `sub`) links to one global person: the first time by an invite opened while signed
  in upstream (or by `join` for the domain), after that by `sub`. A person can link several (their work Google and
  their personal passkey), and one account then reaches all their tenants.
- Upstream groups are mapped to the tenant's teams on each sign-in; mapped teams are replaced, teams an admin set by
  hand are kept.
- An upstream signs people in for its own tenant only: signing in with Acme's Google doesn't, by itself, give access
  to Home.
- **`join` needs a `domain`**, so only people with a verified email there (and, for Google, the Workspace's `hd`) can
  join. Without one, `join` would let in anyone the provider vouches for, which on a public provider (GitLab.com, a
  social sign-in) is anyone on the internet, so the settings must say it outright: `join: {role: member,
  anyone_it_vouches_for: true}`. A `domain` works on `oidc` upstreams too (a verified email at it).

## Forward-auth

For services with no OIDC support. The proxy calls `GET /forward-auth` on the sign-in service with the original
request's headers; the service is found by host (`forward_auth: [{host: grafana.example, service: grafana}]`).

- A valid forward-auth cookie and an allowed `sign_in` on that `service:` (cached 60 seconds): 200, with
  `X-Fleetlayer-Tenant`, `X-Fleetlayer-Person`, `X-Fleetlayer-Teams` and `X-Forwarded-User` for the service to trust.
  Each forward-auth host belongs to one tenant.
- No cookie: a redirect to the sign-in page, then back to the original URL.
- Allowed sign-in but refused by the policy: 403 with the reason.
- The cookie is set on a parent domain shared by the services (`cookie_domain`), separate from the sign-in session's
  cookie, and holds only an opaque, revocable session ID. Services behind forward-auth must strip any
  `X-Fleetlayer-*` headers the client sends (the guide shows the proxy config).

## Audit

- Every check (at sign-in, per request, forward-auth) is a policy decision, written to the audit log as any other:
  `person:sam sign_in service:hub allowed`, with the node that asked. Repeats within a minute are counted, as
  usual.
- Membership changes are written by the control plane as they happen (above).
- Sign-in attempts: `signin/event`, written to the tenant's audit log (or the deployment's, before a tenant is
  chosen) and published on the tenant's `access` channel (`signin.failed` for an Inbox card or an alert after
  several).

## The plugin

Set up and settings: [`plugins/signin.md`](plugins/signin.md). In short:

```yaml
plugins:
  - name: signin
    command: fleetlayer-signin
    config:
      dir: ~/.local/state/fleetlayer/signin     # made 0700: people.json (credentials), keys/, sessions.json
      issuer: https://signin.example
      control_plane: https://control.example
      token_file: ~/.config/fleetlayer/signin-token   # the sign-in token
      listen: 127.0.0.1:7680                    # behind a private HTTPS proxy
      clients:
        - name: hub
          redirect_uris: [https://hub.example/auth/callback]
          secret_file: ~/.config/fleetlayer/signin-clients/hub
      heartbeat: {url: …, token_file: …}
```

- **Monitoring:** `GET /healthz` is 200 while the folder and signing key load and the control plane answers. The
  heartbeat is pushed every five minutes with `success=false` when the control plane couldn't be reached, or the
  signing key is due to rotate and couldn't.
- **Back it up:** losing this folder means every person registers new passkeys and every service's client secret is
  reissued. Back up `people.json` and `keys/`; sessions can be lost.

## Moving it

Every component can move (a Mac first, a home server or VPS later), and the sign-in service is built for it:

- **All its state is one directory** (`dir`: `people.json`, `keys/`, sessions) and **all its settings one file**. It
  assumes no hostname: the issuer URL, the passkey relying party ID and the listen address are settings.
- **Moving to another machine with the same address** (`signin.example` now points at the new machine): stop it,
  copy the directory and the settings file, start it there. The signing key moves with it, so tokens services hold
  stay valid, passkeys keep working and clients change nothing. A test moves a directory between two instances and
  signs in with an existing passkey and token.
- **Changing its address** is the hard case, because **a passkey is bound to the relying party ID it was made for**
  (a domain). Two ways:
  - **The default plans for it.** Setup asks for a domain the person owns and makes passkeys for its registrable
    domain (from the Public Suffix List), even when sign-in runs at `signin.home.example.com`: passkeys made for
    `example.com` keep working at any address under it. A tailnet name works the same way: the registrable domain of
    `laptop.tail1234.ts.net` is the tailnet's own `tail1234.ts.net`, so sign-in can move between the tailnet's
    machines.
  - **On localhost or a machine's own name** (no domain they own), the page says, before the first passkey is made
    and outside any Advanced section: "Passkeys made now stop working if sign-in moves to another address; use a
    domain you own to avoid that." After such a move, people sign in once with an invite link or an upstream provider
    and register a new passkey; the admin page offers "Invite everyone again" for this.
  Either way, services need the new issuer URL. While the old name still points at the service (a setting,
  `previous_issuers`, for 30 days), it answers discovery with a redirect and keeps serving the JWKS, so tokens already issued verify until they expire; each client's issuer setting is
  changed (the guide lists the hub's and pages' settings). Sessions on the old address end; people sign in again.
- The control plane's address is a setting too, and its sign-in token moves with the settings file.

## On a laptop

On a laptop "mini home server" (every service at `http://<name>.localhost:7680` behind one proxy, no TLS), sign-in is
its own service, `http://signin.localhost:7680`, never part of the control plane: it holds credentials, and it moves
on its own later.

- Browsers treat `*.localhost` as a secure context, so passkeys work there. The passkey domain is
  `signin.localhost`, and the page shows the warning above before the first passkey.
- The issuer is an `http://` URL. Fleetlayer's own clients (the hub, pages) accept `http://` only for `localhost` and
  `*.localhost`; everywhere else the issuer must be HTTPS.
- Server-to-server calls (token, JWKS, back-channel logout) can't rely on resolving `*.localhost` (it fails on Linux
  without systemd-resolved), so each side has a `backend_url` setting next to the public one, and services check
  tokens' `iss` against the public issuer while calling the backend one.
- Cookies keep `Secure` and the `__Host-` prefix where the browser allows them on `http://localhost` (Chrome,
  Firefox); a browser that doesn't is told so on the sign-in page rather than signed in with weaker cookies.

## On the control plane: tables and commands

Step 1a. Every table is per tenant except `people`, and nothing here is a credential.

| Table | Holds |
|---|---|
| `tenants` | `id` (never changes: it's in every row and every audit entry's hash), `slug` (what people type and see, renamable), `name` |
| `people` | `id` (`p_` and 20 random characters, the global `sub`), display name; the only table without a tenant |
| `members` | tenant, person, `handle` (unique in the tenant), `role` (`admin` or `member`), when added, when removed. A removed member's row stays, so its handle can't be given to someone else while rules may name it. |
| `teams` | tenant, name, parent team (or none). Every tenant has `everyone`, made with it, with every member in it; simple views leave it out until there's a second team |
| `team_members` | tenant, team, person, `role` (`maintainer` or `member`) |
| `invites` | tenant, the code's SHA-256, handle, role, teams, an existing person (for recovery, or to add someone who already has an account) or none, who made it, expiry (7 days), when and by whom it was used |
| `member_agents` | tenant, an agent pattern (`finances@laptop`, `*@c-vm`), who admitted it, when. A person's agents count only while that person is a member; a team's or tenant's node's agents are its tenant's without a row |
| `audit_outbox` | entries commands queue for the running control plane to chain with its key, so a command never holds the audit key or races the server |
| `nodes` | gains `owner` (a person's ID, `team:<tenant id>/<team>` or `tenant:<id>`) and `enrolled_by` |

Commands, with `--tenant <slug>` (default: the deployment's first tenant):

- `fleetlayer-control tenant add <slug> [--name …]`, `tenant list`, `tenant rename <slug> <new-slug> [--name …]`
- `member list`, `member role <handle> admin|member`, `member remove <handle>`
- `team add <name> [--parent <team>]`, `team remove <name>`, `team list`, `team member add|remove <team> <handle>
  [--maintainer]`
- `invite --handle <h> --role admin|member [--team <t>]… [--person <id>]` prints the invite code (and its link once a
  sign-in service is set up), `invite list`, `invite revoke <code>`; until the sign-in service exists, `invite redeem
  <code> --new "<display name>" | --person <id>` uses one from the command line
- `person list` (the operator's view: IDs, names, how many tenants), `person remove <id>` (from every tenant; their
  own nodes go offline)
- `agent admit <pattern>`, `agent remove <pattern>`, `agent list` (each says whether another tenant may also admit
  it, never which)
- `node add <name> [--owner <person>|team:<name>|tenant] [--no-admit]` (a person's node's agents are admitted to the
  tenant unless `--no-admit`), `node owner <name> <person>|team:<name>|tenant|none`; `node list` shows owners
- `--as <handle>` names who made a change, for the audit log (default `admin`)

Every change to tenants, members, roles, teams, invites and owners is written to the tenant's audit log, with who
made it (`admin`, the person's handle, or `signin`).

**Nodes, tenants and the existing `tenant` column.** Nodes and their sessions are a person's, so they belong to the
deployment, not to a tenant. Every row's `tenant` stays `default` (where a node's sessions, messages and events are
kept); decisions don't use it. Since 1b each request is decided and audited in the tenants that own what it touches:
every tenant either agent is a member of, or for a service's data, a page or an app, the tenant the service names. An
agent no tenant admitted is refused (`agent admit` lets it in). A tenant without a version 2 policy keeps the version 1
rules for its agents, and decides people with the built-in defaults (`default_people: own`).

## Migrating an existing deployment

Everything today is in the tenant `default` (nodes, policy, audit log, summaries, readings).

- The migration adds the tables and one `tenants` row: `id` and `slug` `default`. Nothing else changes, and a running
  control plane keeps serving while it runs.
- **Naming it** is `fleetlayer-control tenant rename default home --name "Home"`: one row, while the control plane
  runs. Every row keeps `tenant = 'default'` (the ID), so no data moves and every audit entry's hash still verifies.
- **The first admin:** `fleetlayer-control --tenant home invite --handle a --role admin`, then nodes' owners with
  `node owner`, and the tenant admits the existing agents (`agent admit "*@laptop"`). On migrating, every existing
  node's agents are admitted to `default`, so nothing that works today stops. A node with no owner belongs to nobody:
  owner-scoped rules don't match it, and `node list` says so.
- **Adding a second tenant** (a company next to a personal one): make it, admit the agents that work for it, give it
  its policy. Nodes stay as they are. History stays in the audit log it was written to; moving existing rows between
  tenants isn't offered, since it would break the audit chain.

## Steps

1. **The control plane's side**, in three releases, each deployed before the next:
   - **1a.** Global people, tenants, teams, memberships (people and agents), roles, handles, invites and node
     owners, with the commands and the migration above. Decisions stay in `default`.
   - **1b.** Decisions in the owning tenant (membership first, then its policy; both sides' tenants between agents;
     `outside`, `agents_elsewhere`); `person:`, `team:`, `role:`, `owned-by:`, `service:`, `page:` in the policy; the
     new verbs (`sign_in`, `admin`, `write`; approvals for `page:` held per space), `default_people`, `owners:`;
     `link/access_check {tenant, who, can, to}` and `_fleetlayer.dev/access/check`; `acl explain`/`acl test` for
     people.
   - **1c.** The sign-in token and `signin/*`.
2. **The sign-in service**, in three releases:
   - **2a.** OIDC provider (one token per tenant, PKCE required of every client, `sign_in` checked before any code),
     passkeys, invites, `/api/check`, health and heartbeat, the move test.
   - **2b.** The admin pages (members, invites, clients; teams, tenants and providers under Advanced).
   - **2c.** Back-channel logout, key rotation, `previous_issuers`. Built: logouts wait on disk and are retried until
     delivered or the session would have expired, and one undelivered for an hour fails `/healthz`; receivers in
     pages and the hub; `fleetlayer-signin keys rotate [--drop-previous]` and `fleetlayer-signin signout --person`.
3. **The hub signs in with Fleetlayer** (coordinated with the hub's owner): "Sign in with Fleetlayer" next to the
   login key, which keeps working until a deployment turns it off.
4. **Google Workspace** as an upstream, then generic OIDC.
5. **Forward-auth.** Built: `/forward-auth` and `/forward/start`, the cookie tied to the sign-in session, decisions
   cached a minute, configs for Caddy, Traefik and nginx in `docs/plugins/signin.md`. Per-host cookies (no shared
   domain) are for later.
6. Later: SAML upstreams, per-page sharing (pages), members' own sharing pages, `_fleetlayer.dev/access/check`
   decisions cached on the node for when the control plane is briefly away.
