# Pages for people instead of vendor artifacts

Your agents make things for you to look at: QA screenshots of every screen in light and dark, a report on last night's
failures, a mock-up of a settings page, a comparison of three options with a recommendation. Vendor artifacts are the
easy way to show them, and they work well while you run one agent on one account. Claude's artifacts open for the
account that published them, and other vendors' hosted previews belong to that vendor. Once you run several accounts,
or Codex and Gemini next to Claude, every agent publishes somewhere different, and a page made by one session may not
open for you at all.

This guide sets up the alternative the author uses: plain folders in a synced vault, written by any agent, and hosted
by Fleetlayer's pages plugin behind one sign-in.

## The shape

- **A page is a folder.** `index.md` (or `index.html` for a mock-up) and its images, in a folder named after the date
  and a short name. Any agent or script that can write files makes one: Claude, Codex, Gemini, a shell script.
- **A space is a folder of pages.** Each agent writes into its own space's folder in a synced, backed-up vault. An
  agent you trust less gets only its own space's folder, so the filesystem keeps it out of the others.
- **`fleetlayer-pages` hosts whatever is there.** There's no publish step and nothing to build. A page's address
  follows from its path (`/<space>/<folder>/`), so the agent that wrote it knows its link, and an open page reloads by
  itself when its files change.
- **People sign in once and read the spaces they're allowed.** Today that's one login key that reads every space;
  with Fleetlayer's single sign-on, each person reads the spaces the access policy gives them.

The same service hosts **apps**: interactive HTML and JavaScript, each on a subdomain of its own. Pages have no scripts
at all; anything interactive is an app.

The page format, the config and the security model are in [Pages](https://fleetlayer.ai/plugins/pages.md) and [Apps](https://fleetlayer.ai/plugins/apps.md).
This guide covers the path through them and what goes wrong.

## 1. Try it on your laptop

[Start on your laptop](https://fleetlayer.ai/guides/laptop.md) sets up pages with everything else:

```sh
fleetlayer up
```

It makes one space, `personal`, with its pages in `~/Fleetlayer/personal/pages/` and its apps in
`~/Fleetlayer/personal/apps/`, and prints where the login key is (`~/.local/state/fleetlayer/pages/login`). Write a
page by hand to see the format:

```sh
mkdir -p ~/Fleetlayer/personal/pages/2026-11-27-first-page
cat > ~/Fleetlayer/personal/pages/2026-11-27-first-page/index.md <<'EOF'
# First page

A page to check that pages work: this paragraph is its summary in the list.

> [!NOTE]
> Images on consecutive lines make a gallery.
EOF
```

Open `http://pages.localhost:7680/personal/2026-11-27-first-page/` and paste the login key. Edit the file and the open
page updates within about a second.

If something on the page is wrong (no title, a missing image, raw HTML), the page shows a box at the top listing it.
Agents check their own work the same way: they fetch the page and look for the box, which has `id="problems"`.

## 2. Tell your agents

Agents use pages only if they know about them. Add the snippet from
[Pages: the AGENTS.md snippet](https://fleetlayer.ai/plugins/pages.md#for-agents-the-agentsmd-snippet) to the `AGENTS.md` your agents
read, with your own folder, address and space in place of the placeholders (on a laptop set up by `fleetlayer up`,
the folder is `~/Fleetlayer/personal/pages/` and the address `http://pages.localhost:7680/personal/<folder>/`).

For more than a few lines of instructions, there are two skills that ship with the plugin:
`plugins/pages/skill/SKILL.md` (pages) and `plugins/pages/apps-skill/SKILL.md` (apps), which the node installs on
every machine from v0.3.9 ([Tell your agents about Fleetlayer](https://fleetlayer.ai/guides/agents-know-fleetlayer.md)). They tell an agent to use a
page wherever it would have published an artifact, how to lay out screenshots, and what an app may and may not
contain.

## 3. A space for each agent

One space is enough to start. Add more when you want to keep agents apart, or share some pages with other people and
not others. Spaces are entries in the plugin's config in `node.yaml`:

```yaml
spaces:
  personal: { dir: /srv/pages/personal, apps: /srv/apps/personal, owner: "person:alice" }
  triage:   { dir: /srv/pages/triage, apps: /srv/apps/triage, untrusted: true }
```

- **Every space folder needs a `.fleetlayer-space` marker file** (an empty file is enough), and so does every apps
  folder. `fleetlayer up` makes them for `personal`; make them yourself for each new space. The health check fails
  without one (see "When it doesn't work").
- **An agent with a Unix user of its own** gets only its own space: mount or sync that one folder into its home, and
  nothing else of the vault. It can't write into another space because it can't see it.
- **`untrusted: true`** is for a space whose agent reads outside input, such as email (see
  [Run an agent that reads strangers' text](https://fleetlayer.ai/guides/untrusted-input.md)). Its apps can't reach other sites, and opening one
  shows a notice first saying which space made it.

## 4. Move it to an always-on machine

A laptop that's asleep serves nothing, and agents on other machines can't write into its folders. When you want pages
for agents elsewhere, move the plugin to a home server or a VPS: [Pages: moving it](https://fleetlayer.ai/plugins/pages.md#moving-it)
lists the steps, and [Start on your laptop](https://fleetlayer.ai/guides/laptop.md#moving-a-piece-to-a-server) shows how the laptop's address keeps
working afterwards.

On the server it sits behind your HTTPS proxy, never on the internet:

- **`base_url`** is the site's `https://` address. Plain `http://` is allowed only for `localhost` addresses.
- **`apps_domain`** is where apps live (`<app>--<space>.<apps domain>`). The proxy sends every name under it to the
  service with the Host header intact, and the certificate must cover them (see the wildcard footgun below).
- **`trusted_proxies`** lists the proxy's address, so failed-login lockouts apply to the real client.
- **`heartbeat`** pushes Gatus-style every minute, failing when a space's folder or marker can't be read. Point it at
  your uptime checker, and send its alerts where an agent listens.

The vault folders move with the share, not with the service: the server mounts or syncs the same folders, with their
markers.

## When it doesn't work

- **An open page stops updating when another machine writes to it.** Filesystem events (inotify) only see changes made
  through the local kernel. A vault shared into a virtual machine, an SMB or NFS mount, or a folder another machine
  writes over a network share never fires an event for writes made on the other side. Pages polls instead: the files
  of an open page every second and a space's list every five seconds. Anything else you build that reacts to new files
  in a shared folder needs to poll too.
- **A space suddenly has no pages.** A synced or mounted folder that goes away looks like an empty folder, not an
  error, and a service reading it happily shows an empty list. That's what the `.fleetlayer-space` marker is for:
  while it's missing, `/healthz` answers 503 naming the space and the heartbeat reports a failure. Remount the share
  rather than recreating the marker in an empty folder.
- **Sync and backups crawl after an agent builds an app.** A `node_modules` tree is hundreds of thousands of files
  that sync slowly, churn every backup and can be rebuilt from the lock file. Exclude it on every share and every
  backup, with the usual build caches: `node_modules`, `.next`, `.turbo`, `.cache`, `target`, `.venv`,
  `__pycache__`. In Syncthing that's the folder's `.stignore`; see [Backups](https://fleetlayer.ai/guides/backups.md#what-not-to-back-up) for the
  backup side. Pages never serves `node_modules` from an app either way.
- **App sign-in works on the server and silently fails on the laptop.** Browsers treat `http://localhost` as secure
  enough for a `Secure` cookie, but not a subdomain such as `http://recipes--personal.apps.localhost`. Pages drops
  the `__Host-` prefix and `Secure` when its address is plain HTTP, and allows plain HTTP only for localhost. If you
  put another sign-in in front of apps on a laptop, it has to do the same.
- **The certificate doesn't cover your apps.** A wildcard certificate covers one level: `*.home.example` covers
  `pages.home.example` but not `recipes--personal.apps.home.example`. Either get a wildcard for the apps domain
  (`*.apps.home.example`), or put apps directly under the wildcard you have (`apps_domain: home.example`, so apps are
  `recipes--personal.home.example`).
- **A mock-up's CSS and images don't load behind the login.** A mock-up (`index.html`) is shown in a sandboxed frame.
  A frame with `sandbox` and no `allow-same-origin` gets an opaque origin, so its own CSS and images are requested
  cross-site and the SameSite=Strict session cookie isn't sent. Pages frames mock-ups with
  `sandbox="allow-same-origin"` and no `allow-scripts`, and serves the files with a policy that forbids scripts, so
  nothing on the page can use that origin. Any sandboxed frame you build behind a login needs the same pair.
- **An agent's app shows a blank screen.** Agents often write inline `<script>` blocks, `onclick=` attributes and
  libraries loaded from a CDN. An app's content policy allows only scripts from its own `.js` files, so the browser
  blocks all three. The service lists each one as a problem on the space's page, and the apps skill tells agents up
  front: code in files, `addEventListener`, libraries bundled or copied into the app.
- **Why each app gets an odd-looking subdomain.** Each app is its own origin, so the browser keeps its scripts away
  from every other app, from pages and from the sign-in. Siblings still share a site, though, and one could set a
  cookie for the parent domain to shadow another's. Over HTTPS, app cookies are `__Host-` cookies (host-only, Secure,
  Path=/), which a sibling can't set or overwrite. To sign in to many apps without registering each one, the pages
  site hands each app a one-time code bound to that app's host and a random state value
  ([Apps: sign-in](https://fleetlayer.ai/plugins/apps.md#sign-in)).
