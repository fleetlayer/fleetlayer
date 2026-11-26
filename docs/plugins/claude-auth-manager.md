# claude-auth-manager (design notes)

Status: design, not built. Fleetlayer's second plugin, after the herdr adapter. It shares one person's Claude sign-ins
and usage with every parallel agent they run, on every machine, while keeping Claude credentials on machines that
person controls. The control plane never holds them; it only relays them sealed to each node's key
([protocol section 8](../protocol.md#8-sync-and-sealed-secrets)).

## Two halves, one binary

- **The hub** runs once per person, on a machine they control (laptop, Mac mini, home server VM, VPS). It holds each
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

## The tokens bundle

`claude-auth-manager/tokens`, secret. Items: one per account,
`{slot, accountUuid, email, expiresAt, token}`, where `expiresAt` is a year after the hub got the token, not after it
was sent. Applying it makes the node's managed slots match: writes each slot, removes managed slots no longer in it,
never touches a slot someone signed into by hand.

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
- The hub can run the same coordinator in its normal (polling) mode against its own sign-in directory
  (`Coordinator::new`), publishing what it polls as `claude.usage` with `source: poll`.