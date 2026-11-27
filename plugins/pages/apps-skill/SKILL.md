---
name: apps
description: Build a small interactive app (an interactive report, a dashboard, a calculator, a tool to click through) as a folder of HTML, CSS and JavaScript in your space's apps folder, hosted by Fleetlayer on a subdomain of its own behind sign-in, and give the person its link. Use when a page needs interactivity (filters, charts you click, sliders); for static reports and screenshots use the pages skill instead.
---

# Apps

> [!WARNING]
> **Never put sensitive data in an app** (financial information, health data, personal information about other
> people, credentials, legal or HR records, customer data) unless your person asks for it explicitly. If they do, list
> for them every query or file that holds it and who can open the app, so they can check it. Anything that needs real
> security, server code or more than a few simple queries is a separate backend (self-hosted Supabase or Convex, or a
> normal service in a pod), not a Fleetlayer app.

An app is a folder of HTML, CSS and JavaScript. It's live on its own subdomain as soon as the files are there, signed
in with the person's Fleetlayer sign-in, and each app is isolated from every other app and from pages.

## Where

- Your space's apps folder: `~/Apps/` (each agent user has its own). If it isn't there, ask your person.
- The address: `https://<app>--<space>.<apps domain>/`. Your setup's `AGENTS.md` names the apps domain and your
  space.
- Names: lower case letters, digits and single hyphens (never `--`).

## Make one

1. **A folder** `~/Apps/<name>/` with `index.html`, your `.js` and `.css` files and any images.
2. **Scripts in files only:** `<script src="app.js" defer></script>`. Inline `<script>` code, `onclick=` attributes
   and `eval` are blocked; use `addEventListener` in your `.js`. Inline styles are fine.
3. **Everything from the app itself:** bundle or copy libraries into the folder (or build with Vite or similar into
   `dist/`, which is served automatically). Files from CDNs or other sites are blocked.
4. **`app.json`** (optional): `{"name": "…", "description": "…", "icon": "icon.png", "live": true}`. `live: true`
   reloads open copies when you change a file, handy while you work. An `icon.png` of 512 pixels makes a good
   home-screen icon.
5. **Talking to other sites:** none by default. If the app must call an API, list its https origin in
   `"network": ["https://api.example.com"]`. Some spaces don't allow it at all. Never put API keys in an app: anyone
   who can open it can read them.
6. **Builds:** fine, but `node_modules` and build caches stay out of the synced folders by design. Build, keep the
   source and `dist/`.

## Check it

Fetch the space's page in the pages site: each app is listed with any problems the service found (no `index.html`,
an inline script, a file from another site, a refused `network` entry). Fix them until there are none, then open the
app's address.

## What it can use

- `fetch("/_fl/me")` gives `{principal, app, space}` for whoever is signed in.
- The page works on phones: make the layout responsive (it can be added to the home screen as an app).
- No server and no database yet (a small database with named queries is coming). Keep state in the page, in
  `localStorage` for the one viewer, or in data files your agent writes into the app's folder (a dashboard's
  `data.json`).

## Give the person the link

On a line of its own, with one sentence on what the app does. The record of the work (what was decided, the review)
still goes where it always does.
