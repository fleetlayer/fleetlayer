---
name: fleetlayer-cookies
description: Use a website as the person from the command line on a machine without their browser (an X client, yt-dlp, curl against a site they're signed in to), through cookies Fleetlayer copies from the signed-in browser on their laptop. Use when a CLI needs the person's browser cookies or session ("no cookies found in Chrome", "not logged in", "needs auth_token"), or to ask the person to share a site's cookies with this machine.
---

# Cookies from the person's browser

Fleetlayer's cookies plugin copies a site's cookies from the browser the person is signed in to (usually on their
laptop) to other machines, and writes them where command-line tools look. The person decides which sites are shared
and with which machines: you can't fetch a site's cookies yourself.

## What this machine already has

```sh
fleetlayer readings cookies.receive
```

Each reading is one set of cookies this node (or another) receives: `set`, `files` (the paths written), `cookies` (how
many), `earliestExpiry`, `appliedAt` and `problem`. Use the files of a reading whose node is yours:

- **An env file** (`AUTH_TOKEN=…`, `CT0=…`): load it into the tool's environment for one command,
  `set -a; . <file>; set +a; <tool> …`. bird, an X client, reads `AUTH_TOKEN` and `CT0` from its environment.
- **A Netscape `cookies.txt`**: `curl -b <file>`, `yt-dlp --cookies <file>`, `wget --load-cookies <file>`.
- **A JSON file**: Playwright's `storageState` (`browser.newContext({storageState: "<file>"})`).

Never print, copy, paste or send a cookie's value: in a message, a log, a commit, a page or your reply. Anyone holding
it can act as the person on that site. Read the file only into the tool that needs it.

When a reading has a `problem` (a cookie expired, nothing arrived for days), tell the person what it says: usually they
need to sign in to the site again in their browser, or wake the laptop. A tool that answers "unauthorised" while the
reading looks fine also means signing in again.

## Asking for a site's cookies

When a tool needs cookies this machine doesn't have, ask the person, in your reply to them. Say which site, which
cookies the tool needs (as few as possible; bird needs only `auth_token` and `ct0` on `x.com`), and why. They set it
up, or tell an agent on the laptop to:

- on the machine with the browser, a `share:` entry in the cookies plugin's config: the set's name, its `domains`, its
  `names` and the node to send to (`to:`);
- on this machine, a `receive:` entry naming the same set and the files to write;
- then they approve the first delivery when Fleetlayer asks.

`docs/plugins/cookies.md` in the Fleetlayer repository (fleetlayer.ai, Plugins, Cookies) has the configuration.

Prefer the person's shared browser (the `fleetlayer-browser` skill) when you only need to look at a page or click
through it: nothing is copied there. Don't read a browser's cookie database or the Keychain yourself, and don't ask for
a whole site's cookies when a tool needs two.
