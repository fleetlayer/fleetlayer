# Apps

Apps are served by the same service as pages (`fleetlayer-pages`, `docs/plugins/pages.md`). Pages are Markdown and
images, with no scripts. Apps are where interactive things go: HTML, CSS and JavaScript, each on a subdomain of its
own.

## What apps are for

Small interactive tools for you and the people you share them with, which an agent builds in minutes. Mostly
client-side JavaScript. For example:

- an interactive report: a QA run you can filter, a cost breakdown with charts you can click into, a comparison of
  options with sliders;
- a dashboard over data an agent writes into the app's folder (the state of your machines, a project's progress);
- a calculator or converter for something you do often;
- with the database (step 2): a household or team checklist, a shopping list or rota, a tracker (books, workouts,
  plants watered), a sign-up sheet or poll for a few people, a small form that collects answers into a table.

> [!WARNING]
> **Sensitive data may not belong in a Fleetlayer app**: financial information, health data, personal information
> about other people, credentials, legal or HR records, customer data. If you do use an app for it, audit every named
> query and make sure each piece of that information can only be read by the people allowed to read it. Otherwise
> build and host your own app backend, with proper authentication and authorization and a thorough security audit.

**When you outgrow a Fleetlayer app** (more than a few limited queries, server code, file uploads, real-time sync,
other services calling it), build it as its own service: self-hosted [Supabase](https://supabase.com) (Postgres with
row-level security), self-hosted [Convex](https://www.convex.dev), or a normal Rust, Python or TypeScript backend
running in a pod. That app is entirely separate from Fleetlayer apps and shares nothing with them.

## Making one

```
<space's apps folder>/build-times/
  index.html
  app.js
  style.css
  app.json        optional
```

It's live at `https://build-times--<space>.<apps domain>/` as soon as the files are there: no deploy, no restart.

- **Builds are fine.** If the folder has `dist/` (or the folder `app.json`'s `serve` names), that's what's served;
  else the folder itself. Build where you like. `node_modules`, hidden files, `app.json`, `migrations/` and
  `queries.sql` are never served.
- **Single-page apps:** a path with no file extension that doesn't exist (`/settings`) gets `index.html`.
- **No inline scripts and no files from other sites.** Scripts come from `.js` files in the app (no `<script>` with
  code in it, no `onclick=`, no `eval`); libraries are bundled or copied into the app, not loaded from a CDN. Inline
  styles are fine. The service shows a problem for anything its policy will block.
- **Names:** lower case letters, digits and single hyphens (`--` separates the app from the space in the address).

### `app.json`

```json
{
  "name": "Build times",
  "description": "How long each project's CI takes, by week.",
  "icon": "icon.png",
  "theme_color": "#3b4fd6",
  "serve": "dist",
  "network": ["https://api.example.com"],
  "live": true
}
```

| Field | What it does |
|---|---|
| `name`, `description` | Shown in the list and on the home screen |
| `icon` | An image in the served folder; a 512-pixel PNG works everywhere. Without one, the app gets its first letter |
| `theme_color` | The home-screen app's colour |
| `serve` | The folder to serve (default `dist/` when it exists) |
| `network` | https origins the app's scripts may connect to. None by default |
| `live` | Reload open copies when the app's files change (handy while an agent works on it) |

### What the app can use

- `GET /_fl/me`: `{principal, app, space}` for the person signed in (`principal` is null with the login key).
- `/_fl/manifest.webmanifest` and `/_fl/icon.svg`: added to the app's HTML for the home screen unless it has its
  own.

## The home screen

Every app gets a web app manifest and the iOS touch-icon tags, so "Add to Home Screen" on a phone gives an icon that
opens the app full screen. An app may ship its own manifest or service worker; the service then leaves them alone.

## Isolation and limits

- **Each app is its own origin**, `<app>--<space>.<apps domain>`, so the browser keeps its scripts away from every
  other app, from pages and from the sign-in. A wildcard certificate covers one level: either a wildcard for the apps
  domain (`*.apps.home.example`) or apps directly under an existing wildcard (`apps_domain: home.example`).
- **The app origin's Content-Security-Policy:** everything from the app itself; scripts only from files; connections
  only to the app and the origins in `network`; no framing by other sites.
- **Untrusted spaces** (a space whose agent reads outside input, such as email: `untrusted: true`): its apps can't
  reach any other site (entries in `network` are refused, and only the space's owner can change that), and opening
  one shows a notice first saying which space made it and what it can reach.
- A content policy makes leaking data hard, not impossible (a link the reader clicks still carries data out). The
  isolation is what makes that matter little: an app's origin holds only that app.

## Sign-in

Apps sign in through the pages site, which signs people in through Fleetlayer (`docs/plugins/pages.md`, "Sign-in").
The handshake is shaped like OpenID Connect:

1. An app with no session for you sends the browser to the pages site's `/_/apps/authorize` with a random `state`,
   kept in a short-lived cookie on the app's origin.
2. The pages site signs you in if needed, checks you may read the app (`read` on `app:<space>/<name>` for someone
   signed in through Fleetlayer; the login key's holder reads every app), shows the notice for an untrusted
   space, and sends you back to the app's own host, worked out from its name and never taken from the request, with a
   code that is single-use, under a minute old and bound to that host and `state`.
3. The app's origin exchanges the code for its own session cookie: `__Host-`, Secure, HttpOnly, SameSite=Lax, 12
   hours. No app ever sees the pages site's cookie, and a sibling app can't set or overwrite another's.

Over plain HTTP on a laptop (`http://pages.localhost`), browsers refuse Secure cookies on subdomains, so the cookies
drop the `__Host-` prefix and `Secure`. The service allows plain HTTP only for `localhost` addresses.

## Configuration

In the pages plugin's config (`docs/plugins/pages.md`):

```yaml
base_url: https://pages.home.example        # the pages site; apps send people here to sign in
apps_domain: apps.home.example               # app recipes of space personal: recipes--personal.apps.home.example
spaces:
  personal: { dir: /srv/pages/personal, apps: /srv/apps/personal, owner: "person:alice" }
  triage:   { dir: /srv/pages/triage, apps: /srv/apps/triage, untrusted: true }
```

Each apps folder holds a `.fleetlayer-space` marker, like a pages folder, checked by `/healthz` and the heartbeat.
The proxy sends `*.<apps domain>` to the service with the Host header intact.

Sync and backups: apps folders are synced and backed up like pages folders, with `node_modules` and build caches
(`.next`, `.turbo`, `.cache`, `target`, `.venv`, `__pycache__`) excluded on every share.

## Step 2: a small database (designed, not built)

- One SQLite database per app, in the service's own state folder, never in the synced share (a live database there
  makes sync noisy and can be copied mid-write), with consistent snapshots into the backed-up share and their age in
  the heartbeat.
- The schema from `migrations/*.sql` in the app's folder.
- **Named queries only**, in `queries.sql`, each with the permission it needs; the browser calls them by name and
  never sends SQL:

  ```sql
  -- name: my_recipes
  -- can: read
  select * from recipes where owner = :me;
  ```

  `:me` is always set by the server. Permissions come from the access policy (`read`, or the app's own verbs such as
  `edit`, on `app:<space>/<name>`).
- Enforced: default deny (no `-- can:`, no query; no `queries.sql`, no database); checked when loaded (one statement
  per query; no `ATTACH`, `PRAGMA` or extensions); read queries on a read-only connection; a warning on queries over
  owner-style tables that don't filter by `:me`; limits on rows, time, size and rate, each with an error naming it;
  every write audited. Untrusted spaces get no database unless the space's owner turns it on, and their
  `queries.sql` is untrusted SQL held to the same checks.
