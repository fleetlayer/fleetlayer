# claude-auth-manager (design notes)

Status: built (the node half, readings and the hub); the hub awaits its first real sign-in. Fleetlayer's second plugin, after the herdr adapter. It shares one person's Claude sign-ins
and usage with every parallel agent they run, on every machine, while keeping Claude credentials on machines that
person controls. The control plane never holds them; it only relays them sealed to each node's key
([protocol section 8](../protocol.md#8-sync-and-sealed-secrets)).

## Two halves, one binary

- **The hub** runs once per person, on an always-on machine they control (a home server VM, a Mac mini, a VPS), never
  on the laptop they close and carry: the agents on servers depend on it. It holds each
  account's normal sign-in, used only to poll usage and identity, and its setup token (`claude setup-token`: lasts a
  year, inference only). It signs in and refreshes by running Claude Code's own binary, never by calling Anthropic's
  OAuth endpoints itself, and serves a small web UI for the sign-ins and for each machine's key fingerprint.
- **On every node** (`sync_target`), it writes the setup tokens into account slots, serves the rotator plugin's
  coordinator API, and checks that each token works.

## Token facts that shape it (tested November 2026)

- A normal sign-in's refresh revokes the old access token at once, so a normal sign-in can't be shared. The hub keeps
  its normal sign-ins in a directory of its own that no session uses; sessions on the hub's machine get setup-token
  slots like every other node.
- A setup token can't read profile or usage (403), so the target learns nothing from it: every bundle item carries
  what the slot and rotator need.

## What a setup token can't do

A setup token has only the `user:inference` scope. Sessions that use one run Claude normally but lose what needs a
full claude.ai sign-in: claude.ai connectors (gone from `claude mcp list`), and very likely artifacts, Remote Control
and cloud sessions, since the profile endpoint refuses the token. That's the default here, and the right choice for a
fleet that runs several accounts across several machines and uses its own replacements (a shared browser over
Fleetlayer, a phone relay for remote control).

For people who need those features, a second mode is planned, not built: the hub as the only thing that ever
refreshes each normal sign-in, sending every node the fresh short-lived access token (full scopes, no refresh token)
after each refresh. Tested so far: a session uses an entry with no refresh token as long as the server accepts it, and
re-reads the slot after a 401. Still to test: whether a refresh revokes the previous access token at once. The cost:
every node depends on the hub around the clock (an access token lasts about 8 hours), and each sign-in is redone
monthly.

## The tokens bundle

`claude-auth-manager/tokens`, secret. Items: one per account,
`{slot, accountUuid, email, expiresAt, token}`, where `expiresAt` is a year after the hub got the token, not after it
was sent. Applying it makes the node's managed slots match: writes each slot, removes managed slots no longer in it,
and replaces a sign-in someone made by hand: the hub manages every slot, and a hand sign-in would expire while a
setup token lasts a year. Account 1 goes into Claude Code's default sign-in (`~/.claude`); `default_slot: false` on a
node leaves that alone.

- **macOS:** the login keychain item `Claude Code-credentials-<first 8 hex of sha256(NFC slot path)>`, account `$USER`,
  written through `security -i` so the token never appears in a process list. This needs the login keychain unlocked,
  so **on macOS the node runs as a per-user LaunchAgent**, never a LaunchDaemon.
- **Linux:** `<slot>/.credentials.json`, mode 0600.
- The entry has no refresh token; Claude Code uses it as it is and leaves it alone.
- **Replacing a token under a running session works** (tested 26 November 2026). A session keeps the token it holds
  in memory while that token works; once it's rejected (401), the session reads the slot again on its retry and
  carries on with the new one, so nobody sees a failure. Still untested: how fast revoking a setup token takes
  effect.

## Readings

- Kind `claude.usage`, keyed by account UUID, each with its own `at` and reset times. Sessions report the five-hour
  and seven-day figures from response headers as they work, setup-token sessions included (seen in Claude Code's
  `rate_limit_event`; not yet confirmed through the rotator's `$.session.usage().rateLimits`); only the hub's polling has the per-model weeks, which go
  stale while the hub sleeps.
- Kind `claude.token_health`, keyed by account UUID, `data` `{ok, email, claudeVersion, error?}`: the node half runs
  `claude -p` from each managed slot (a few seconds, one small request) when the slot is new or its token changed,
  after each Claude Code upgrade, a day after its last check and an hour after a failed one, and straight after a
  bundle is applied. Separate from applying: `applied: true` only says the write worked. The control plane can alert
  on `ok: false` and on a reading that stops coming.
- The node half serves the rotator's coordinator API on its socket (`coordinator.sock` in `~/.claude/account-rotator`,
  `GET /state` long polls, `POST /readings`), moved from Quietdeck's usage module (`src/coordinator/`, API 0.7.1). It
  runs in **borrow mode**: nothing is polled, refreshed or signed in, since setup tokens can't read usage or profiles.
  Its accounts are the managed slots (the rotator's ID is the slot number); add, remove and sign-in answer 409.
  Readings sessions post are published as `claude.usage` keyed by account UUID, `data` the coordinator's reading shape
  `{fiveHour?, sevenDay?, models?, at, source}`; readings from elsewhere (the hub's polls, other nodes' sessions) come
  in by subscription and the newest whole reading wins, keeping model weeks. A token a session saw rejected, or a
  failed `token_health` check, shows the account as not working until a response or a passing check says otherwise; an
  account that leaves the bundle goes once its sessions have moved off. The version starts at the clock in
  milliseconds. It needs rotator 0.7.6 or later (which leaves `/hello` name `fleetlayer-claude-auth-manager` alone and
  handles a restarted coordinator). A rotator's own coordinator holding `coordinator.lock` is asked to shut down;
  Quietdeck's node holding it is waited for (turn its Claude part off where this plugin runs), and a
  `claude.coordinator` reading says which.
- The hub polls each signed-in account's usage every five minutes and publishes it as `claude.usage` with
  `source: poll`. When an access token has expired it has Claude Code refresh it first (`claude -p /usage` from that
  sign-in), which revokes the old one: nothing but the hub may read its sign-ins.
- Kind `claude.signin_age`, keyed by account UUID, `data` `{email, daysLeft, warn}`. A normal sign-in's refresh token
  expires about 30 days after the sign-in, on a fixed date that refreshing doesn't move, and the usage endpoint doesn't
  say when. The hub takes the date from the stored credential when it's there, else counts 30 days from the sign-in,
  and sets `warn` from day 25, so an agent can ask the person to sign in again before polling stops.
- Kind `claude.setup_token_age`, the same shape: setup tokens last a year from when they were made; `warn` a month
  ahead.

## The hub

One process: the plugin with `role: hub` and a `hub:` section, under a Unix user of its own on an always-on machine.

```yaml
plugins:
  - name: claude-auth-manager
    command: fleetlayer-claude-auth-manager
    grants: [sync.publish, reading.publish]
    config:
      role: hub
      targets: [laptop, agents, agents-triage]       # the nodes that get the tokens
      hub:
        dir: ~/.local/state/fleetlayer/claude-hub     # default; made 0700
        listen: 10.0.0.5:7660                         # behind a private HTTPS proxy, never the internet
        login_file: /etc/claudehub/login              # at least 32 random characters, 0600
        trusted_proxies: [10.0.0.2]                   # whose X-Forwarded-For is believed
        login_hint: "1Password: infra/claude-hub-login" # optional, shown under the login box; never the key
        heartbeat:                                    # optional, Gatus external-endpoint style
          url: https://status.example/api/v1/endpoints/claude-hub/external
          token_file: /etc/claudehub/heartbeat-token
```

- **The person does every sign-in from their own browser**, in the hub's page: it shows Claude Code's sign-in link,
  they sign in as that account, and paste the code the page shows back into the hub. Each account needs a normal
  sign-in (for usage, monthly) and a setup token (for the agents, yearly). The hub's account numbers are the account
  numbers on every machine: account 1 is each machine's default sign-in (`~/.claude`), 2 and up the rotator's slots. A
  new account takes the lowest free number. **Delete** signs the account's normal sign-in out, removes it, and every
  machine clears that slot on the next publish (a setup token itself can only be revoked on claude.ai). Claude Code's
  own messages (a code cut short, a code refused) appear
  on the page.
- **Its folder** holds `hub.json` (accounts and dates), `tokens.json` (the bundle, 0600, published whenever it changes)
  and `home/.claude/accounts/<n>` (the normal sign-ins). Losing it costs a few minutes of signing in again; the tokens
  already on the nodes keep working for up to a year, so leaving it out of backups is the safer choice.
- **Its login** is one long random value compared in constant time. Five failures in 15 minutes lock that client
  address out for 15 minutes (behind a proxy, set `trusted_proxies` so one person's typos don't lock everyone out).
  Sessions are an HttpOnly, Secure, SameSite=Strict cookie lasting 12 hours, kept in memory: a restart signs everyone
  out.
- **Monitoring:** `GET /healthz` answers 200 while the hub can read its folder, for an uptime check. The heartbeat is
  pushed after every five-minute round, `success=false` with the reasons when an account couldn't be polled or
  refreshed, so a hub that's up but no longer working alerts too.
- **What it protects against:** run it as a system service with its own user and hardening, and the agent users on
  the same machine can't read its sign-ins. Whoever administers the machine as root still can: that holds for anything
  on a host someone else manages.
## Signing in headless (tested 26 November 2026, Claude Code 2.1.332)

The hub runs on an always-on machine with no browser; the person opens its web UI from their own browser and does
every sign-in there. Both of Claude Code's flows work that way:

- `claude auth login --claudeai` (with `CLAUDE_CONFIG_DIR` and `CLAUDE_SECURESTORAGE_CONFIG_DIR` set to the account's
  private folder) prints `If the browser didn't open, visit:`, the sign-in URL (one opaque query token), then
  `Paste code here if prompted >`, and reads the code from stdin. It works with a plain pipe; no terminal needed.
- `claude setup-token` draws a terminal UI: `Browser didn't open? Use the URL below to sign in`, the URL, and a
  `Paste code here if prompted` box. It needs a pseudo-terminal, and it shows the year-long token on screen at the
  end, where the hub reads it.
- A code cut short gets `That doesn't look like the full code. Copy all of it, including the # in the middle.` and the
  prompt waits for another; a refused one gets `Couldn't log in: That code wasn't accepted…` and exits 1. In
  `setup-token`, a refused code leaves the screen on `Press Enter to retry`, which the hub treats as a failure. A
  terminal UI places text with escape codes, so the hub reads that screen through a terminal emulator (`vt100`).
- Still to confirm with a real sign-in: that the page shows a code after signing in when the hub's localhost callback
  can't be reached, and that the pasted code lands in the account's folder. Sign-in URLs and codes are as sensitive as
  tokens: never logged or published.
