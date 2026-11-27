# pages

Status: being built (27 November 2026). The renderer, the web app and the login key first; `fleetlayer pages push` and
sign-in through Fleetlayer next.

Pages are for people to look at: QA screenshots, reports, mock-ups, decision write-ups. Any agent or script that can
write files makes one (Claude, Codex, Gemini, Pi, a shell script), as a plain folder in a synced, backed-up vault.
`fleetlayer-pages` hosts whatever is in the folder: there's no publish step and nothing to build. It replaces vendor
"artifacts", which open only for the account that published them.

## Spaces and addresses

A **space** is one folder of the vault, readable by the people the policy names. Each page is a folder in it:

```
<space folder>/
  .fleetlayer-space             the marker that says the folder is really there (see Monitoring)
  2026-11-27-signing-flow-qa/
    index.md
    img/
      desktop-light.jpg
      phone-dark.jpg
```

A page's address follows from its path: `https://<pages address>/<space>/<folder>/`. `https://<pages address>/<space>/`
lists the space's pages, newest first, and `/` lists the spaces you may read. There's no command to print a link: the
agent that wrote `personal/2026-11-27-signing-flow-qa/` knows its address.

Folder names: start with the date (`2026-11-27-`) so the list sorts by it, then a short name in lower case with
hyphens. A folder without a date is dated by its last change. Folders and files starting with `.` or `_` are never
served.

## The page format

### `index.md`

```markdown
# Signing flow: QA on the new editor

What this shows, from when and from where, in one paragraph: this paragraph is the page's summary in the list.

> [!WARNING]
> The phone layout clips the signature box below 360 pixels.

## Desktop

![Light, 1440 wide](img/desktop-light.jpg)
![Dark, 1440 wide](img/desktop-dark.jpg)

## Phone

![Light, 390 wide](img/phone-light.jpg)
![Dark, 390 wide](img/phone-dark.jpg)

| Screen | Light | Dark |
|---|---|---|
| Editor | passed | passed |
```

- **The first line is `# Title`.** The first paragraph after it is the summary, shown under the title and in the list.
- **Images:** `![Caption](img/shot.jpg)`. The alt text is the caption. Images on consecutive lines (no blank line
  between them) make a **gallery**; tall images (phone screenshots) get narrower columns. An image in a paragraph of
  its own is shown at its own size, up to the page's width. An image inside a sentence stays inline. Every image opens
  in a lightbox (arrow keys move between the page's images, Escape closes); without scripts it opens the file.
- **Callouts:** a block quote that starts with `[!NOTE]`, `[!TIP]`, `[!IMPORTANT]`, `[!WARNING]` or `[!CAUTION]`
  (GitHub's alerts). A plain block quote is a plain box.
- **Tables, lists, task lists, code blocks, strikethrough and footnotes** are ordinary GitHub-flavoured Markdown.
- **Links:** `https://`, `http://` and `mailto:` links, `#anchors`, and relative links to files in the page's folder or
  to other pages in the same space (`../2026-11-20-first-round/`). Any other scheme (`javascript:`, `data:`) becomes
  plain text.
- **No raw HTML and no scripts.** HTML in Markdown is shown as text. For a chart, render an image.
- Images are files in the page's folder: JPEG (quality about 82) for screenshots, PNG where it matters, SVG, WebP,
  GIF, AVIF. Images from other sites aren't loaded.

### `index.html`, for a mock-up

When the page itself is the thing to look at (a design mock-up with its own layout and CSS), put `index.html` in the
folder in place of `index.md`. It's shown in a sandboxed frame under the app's header, and reloaded when its files
change.

- HTML and CSS only: no `<script>`, no `on…=` attributes, no `javascript:` links. The frame has no `allow-scripts`
  and the file is served with a policy that forbids scripts, so nothing would run anyway.
- Its own files (CSS, images, fonts) by relative path from the page's folder. Nothing from other sites.
- A `<title>`: it's the page's title in the list. `<meta name="description" content="…">` is its summary.

### Problems are shown on the page

The service checks every page as it shows it and lists what's wrong in a box at the top, and the space's list marks
the page:

- no `# Title` on the first line, or no summary paragraph after it (`<title>` for HTML);
- an image or link to a file that isn't in the folder, or a path that leaves the space;
- raw HTML in Markdown (shown as text);
- in `index.html`: a script, an `on…=` attribute, a `javascript:` link, or a file from another site;
- an image over 5 MB;
- neither `index.md` nor `index.html` in the folder.

Agents check their page by fetching it (the problems box has `id="problems"`), or with `fleetlayer pages push --check`.

## Live

An open page updates by itself within about a second of its files changing, and a space's list within a few seconds:
the service checks the folders it's showing (polling, not filesystem events, which miss writes made on the other side
of a network or virtual-machine share) and tells the browser over server-sent events.

## Running it

A Fleetlayer plugin with its own role, run like the Claude sign-in hub: by a node on an always-on machine, under a Unix
user of its own, behind a private HTTPS proxy.

```yaml
plugins:
  - name: pages
    command: fleetlayer-pages
    config:
      listen: 10.0.0.5:7670                     # behind a private HTTPS proxy, never the internet
      base_url: https://pages.example           # optional: for the links the service prints
      spaces:
        personal: { dir: /srv/pages/personal, title: Personal }
        triage:   { dir: /srv/pages/triage }
      login_file: /etc/fleetlayer-pages/login   # at least 32 random characters, 0600
      trusted_proxies: [10.0.0.2]               # whose X-Forwarded-For is believed
      login_hint: "1Password: infra/pages-login"  # optional, shown under the login box; never the key
      heartbeat:                                # optional, Gatus external-endpoint style
        url: https://status.example/api/v1/endpoints/pages/external
        token_file: /etc/fleetlayer-pages/heartbeat-token
```

`fleetlayer-pages serve <config.json>` runs the same web service without a node (the config's JSON form), for trying
a page format change locally.

### Who writes where

Every agent writes straight into its own space's folder. An agent you trust less, with its own Unix user, gets only
its own space (a mounted sub-folder or a synced folder of its own), so the filesystem keeps it out of the others. An
agent with no file sync (a cloud machine on your network with a Fleetlayer node) uses `fleetlayer pages push`
(planned), checked against the access policy.

### Sign-in

Until Fleetlayer's single sign-on is built, the service has the hub's kind of login: one long random key, compared in
constant time, five failures in 15 minutes lock that address out for 15 minutes, and a session cookie (HttpOnly,
Secure, SameSite=Strict) lasting 12 hours, kept in memory. Whoever has the key reads every space. With single sign-on,
each person reads the spaces the access policy gives them (`read` on `page:<space>/*`).

### Security

- The app's own page, styles and script are built into the binary; every answer carries a strict
  Content-Security-Policy (scripts and styles only from the service, no framing by other sites), `Referrer-Policy:
  no-referrer` and `X-Content-Type-Options: nosniff`.
- A page's own files are served with `Content-Security-Policy: sandbox` and no scripts, so an SVG or an HTML file
  opened directly can't run anything. Paths are checked inside the space's folder, symbolic links included.
- Markdown is rendered with raw HTML turned into text and links limited to safe schemes.

### Monitoring

Each space's folder holds a `.fleetlayer-space` file, made when the space is set up. A share that has dropped looks
like an empty folder, so the marker is what tells the two apart. `GET /healthz` answers 200 while every space's
folder and marker can be read, else 503 with the spaces that can't. The heartbeat is pushed every minute,
`success=false` with the reasons in the same case.

## For agents: the AGENTS.md snippet

```markdown
## Pages for people

To show a person something (screenshots, a report, a mock-up), write a page: a folder in `~/Pages/` named
`<yyyy-mm-dd>-<short-name>/` with `index.md` (first line `# Title`, then a one-paragraph summary) and its images in
`img/`. Images on consecutive lines make a gallery, alt text is the caption, `> [!NOTE]` / `> [!WARNING]` make
callouts. No HTML, no scripts. Its address is `https://<pages address>/<space>/<folder>/`: fetch it to check it shows
no problems, then give the person that link.
```

The `pages` skill (`plugins/pages/skill/SKILL.md`) is the longer version: install it as a skill, or sync it to every node with the files plugin.
