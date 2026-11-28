# Notes for agents that build software on your machines

Agents that write and test code on small always-on machines (a VM with a few cores, a home server, a cheap VPS) keep
falling into the same few traps. Each one costs an afternoon the first time and a minute once you know it. This page
collects them, each as the trap, why it happens and the fix. Put the ones that apply into your agents' instructions
(`AGENTS.md` or `CLAUDE.md` in the repo), so the next session doesn't rediscover them.

## Big builds go to a build server

A Rust workspace's `target/` folder grows to tens of gigabytes, and a full build or test run keeps every core busy for
minutes. On a VM with four cores and 8 GB that means a full disk, swapping, and every other agent on the machine
slowing to a crawl. Keep the agents where they are and send the builds somewhere bigger.

Fleetlayer's own [`scripts/on-dev`](../../scripts/on-dev) is a worked example, about 60 lines of shell. Each command
copies the checkout to the build server over SSH, then runs cargo there:

```sh
scripts/on-dev check            # cargo check on the build server
scripts/on-dev fmt              # cargo fmt --check
scripts/on-dev clippy           # clippy --all-targets -D warnings
scripts/on-dev test -p fleetlayer-node    # anything after the command goes to cargo
scripts/on-dev lock             # cargo update there, Cargo.lock copied back
scripts/on-dev format           # cargo fmt there, the formatted .rs files copied back
scripts/on-dev clean            # delete this checkout's copy and its target/ there
```

What makes it work:

1. **The host is set per machine, outside git.** It reads `$FLEETLAYER_DEV_HOST`, or a gitignored `.dev-host` file at
   the checkout's root, and stops with a clear message if neither is set. The repo never names a hostname.
2. **One copy per checkout.** The remote folder is named after the checkout's directory, so each git worktree gets
   its own copy and its own `target/`. Two agents in two worktrees never fight over one build folder.
3. **Only the source is copied.** `rsync --filter=':- .gitignore' --exclude=.git --delete-after` skips everything
   gitignored (the local `target/` included) and removes files you deleted locally.
4. **It leaves room for others.** Cargo runs under `nice -n 10`, pinned to 8 cores, inside a memory scope (12 GB with
   `-j 8` for build and test, 6 GB with `-j 4` for fmt, check and clippy), after taking a build slot, so many agents
   share the build server without overloading it ([Many agents, a few build machines](shared-build-machines.md)).
   Pick numbers that suit your server.
5. **The exit code comes back.** The script returns cargo's status (it reads `PIPESTATUS` past the output filter), so
   an agent can tell a failed test from a passing one.

The same shape works for any heavy toolchain: copy the source, run the command remotely, copy back only what the
command was meant to change.

### Syncing with `rsync -a` can make cargo skip a changed file

The trap: you change a file, sync it with `rsync -a`, build on the server, and the build uses the old code. It reports
errors about things that plainly exist in your copy, or passes when it shouldn't.

Why: `-a` includes `-t`, which copies each file's modification time from your machine. Cargo decides whether a file
changed by comparing its modification time with the last build's. If the copied time is older than the server's last
build (a local clock running behind is one way), cargo thinks the file is unchanged and skips it.

The fix: copy by content and let changed files get a fresh time on the server.

```sh
rsync -rlpD --checksum --delete-after --filter=':- .gitignore' --exclude=.git ./ server:/path/to/checkout/
```

`-rlpD` is `-a` without `-t` (and without owner and group), so every file rsync writes gets the server's current time,
which is always newer than the last build. `--checksum` makes rsync compare files by content. Without it, the
modification times would never match and rsync would rewrite every file on every sync, which gives each one a new time
and makes cargo rebuild the whole workspace.

## A GitHub organization can't be created from the CLI or the API

The trap: an agent setting up a new project tries to create its GitHub organization with `gh` or the REST API and gets
a 404, however many scopes the token has.

Why: the endpoint for creating organizations, `POST /admin/organizations`, exists only on GitHub Enterprise Server.
On github.com it answers 404.

The fix: a person creates the organization once, in the web UI at github.com/account/organizations/new. After that an
agent can do the rest from the CLI, starting with the repos:

```sh
gh repo create my-org/my-project --private
```

If your agents set up projects end to end, write this into their instructions as a step that waits for you, so they
ask instead of hunting for a workaround.

## Screenshots of a web UI from a small headless VM

The trap: an agent has built or changed a web page and needs to see it, on a VM with no desktop. Installing a fresh
browser is a large download, the version it fetches may not match the Playwright library already there, and signing in
to real services to reach each state of the page is slow and leaves real side effects.

The fix has two parts.

**Use a browser that's already on the machine.** If anything on the machine has used Playwright, its browsers are in
`~/.cache/ms-playwright`. The small headless build, `chromium_headless_shell`, is enough for screenshots. Use
`playwright-core`, which doesn't download browsers, and when its version expects a different browser build than the one
installed, point `executablePath` at the binary you have:

```sh
ls ~/.cache/ms-playwright/
# chromium-1255  chromium_headless_shell-1255  ffmpeg-1013
```

```js
// shot.mjs (an ES module, so top-level await works): node shot.mjs
import { chromium } from 'playwright-core';

const shell = 'chromium_headless_shell-1255/chrome-headless-shell-linux64/chrome-headless-shell';
const browser = await chromium.launch({
  executablePath: `${process.env.HOME}/.cache/ms-playwright/${shell}`,
});
const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
await page.goto('http://127.0.0.1:8080/');
await page.screenshot({ path: 'home.png', fullPage: true });
await browser.close();
```

The folder names carry the browser's build number, so list the folder rather than copying the path above.

**Serve the real page against a small mock of its API, on localhost.** A short script that serves the page's real HTML,
CSS and JavaScript files and answers its API calls with canned responses lets the agent show every state: an error, a
half-finished flow, a form filled in wrongly, the finished screen. Nothing signs in to anything, and nothing listens
beyond `127.0.0.1`. Because the page files are the real ones, a screenshot shows what users will see.

## A web page for secrets that ships inside a Rust binary

Some small services need a page of their own: a sign-in hub that passes login links and codes through, a settings page
for an internal tool. When that page handles secrets, a strict Content Security Policy is worth having, and building
the page into the binary makes one easy. Fleetlayer's Claude sign-in hub (`plugins/claude-auth-manager/src/hub.rs`
and its `web/` folder) does it this way.

The design:

- **The page's files are built into the binary.** The HTML, CSS and JavaScript are embedded with `include_str!` and
  served from the program's own routes (`/assets/app.js`, `/assets/hub.css`). Nothing is fetched from a CDN, and the
  page can't drift from the program that serves it.
- **One response layer sets the security headers on every answer:**

  ```text
  Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self';
                           connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'
  X-Frame-Options: DENY
  Referrer-Policy: no-referrer
  X-Content-Type-Options: nosniff
  ```

  Since scripts and styles come only from the page's own address, no `unsafe-inline` is needed, and an injected
  `<script>` won't run.
- **A test checks it.** The hub's tests fetch each route and assert that the policy is there and has no
  `unsafe-inline`, and that the page has no inline script.

The footguns:

1. **Inline `style=""` attributes are blocked too.** `style-src 'self'` covers them, so a progress bar written as
   `style="width: 40%"` silently loses its width. Set a CSS custom property from JavaScript instead, which the policy
   allows, and use it in the stylesheet:

   ```js
   el.style.setProperty('--pct', `${el.dataset.pct}%`);
   ```

2. **A link to an outside sign-in page leaks your page's address** in the `Referer` header unless the page sends
   `Referrer-Policy: no-referrer`.
3. **A native `<dialog>` closed with Escape fires `cancel`, and nothing else.** If the dialog stands for work running
   on the server (a sign-in program waiting for a code), the `cancel` handler must also tell the server to stop it.
   Otherwise the program keeps running until its own timeout. The hub's handler calls `preventDefault()` and then the
   same cancel function as its Cancel button, which sends `DELETE /api/flows/<id>`.

## Many agents share one GitHub API limit

The trap: CI checks start failing with "API rate limit exceeded", in every session at once, and nothing works on
GitHub from the command line until the hour turns over.

Why: GitHub allows 5,000 API requests an hour per user, and every session that runs `gh` as you draws from the same
5,000. `gh run watch` polls every 3 seconds by default, about 1,200 requests an hour for each run being watched. Five
agents watching CI after a push use the whole allowance in under an hour, and a loop over `gh run list` does it faster.

The fix:

- **Watch with a long interval, once per run, with a timeout:**

  ```sh
  timeout 1020 gh run watch <run-id> --interval 60 --exit-status
  ```

  Sixty seconds is plenty for a CI run that takes minutes. Never poll `gh run list` in a loop.
- **Check the allowance** when something fails oddly: `gh api rate_limit` shows what's left and when it resets.
- **Let CI tell the agent instead of asking.** A webhook on the repository can deliver each result to the session
  that pushed the commit, with no polling at all: [CI results to the agent that pushed](ci-results.md).
- **Put the rule in the agents' shared instructions** (`AGENTS.md`), since every new session would otherwise start
  with the default.

## `pkill -f` can match your own shell

The trap: an agent stops the processes its tests started with something like `pkill -f fleetlayer-node`, and its own
command dies with them, or the tool call ends with no output.

Why: `-f` matches the pattern against each process's full command line. When an agent runs a command, its harness
usually starts a shell with the whole command as an argument (`bash -c '... pkill -f fleetlayer-node ...'`). That
shell's command line contains the pattern, so `pkill` kills it. `pkill` leaves out its own process, but not the shell
that started it.

The fix: wrap one character of the pattern in brackets.

```sh
pkill -f 'bin/[f]leetlayer-node'
```

As a regular expression, `[f]leetlayer-node` still matches `fleetlayer-node`, but the shell's command line now holds
the text `[f]leetlayer-node`, which doesn't match. Including part of the path (`bin/`) also keeps it to the binary you
mean.

Don't reach for `pkill -x fleetlayer-node` instead. It matches the process name exactly, so on a machine where the
node also runs as a service it stops that one too, and with it every session's link to the fleet. Match the path of
the copy your tests started.
