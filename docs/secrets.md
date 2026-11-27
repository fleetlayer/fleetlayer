# Secrets

Agents need credentials: a GitHub token, a cloud API key, a site's cookies, a Claude sign-in. Fleetlayer delivers
each one to the machines that need it, sealed so only that machine can open it, and keeps it out of everything that
leaves the machine afterwards. It's off until you ask an agent to set it up; most people start with a token in their
shell, and that keeps working.

## One shape for every secret

Claude setup tokens ([claude-auth-manager](plugins/claude-auth-manager.md)), browser cookies
([cookies](plugins/cookies.md)) and password-manager items go the same way:

1. **A source** holds the secret: the Claude hub, the browser on your laptop, a password manager.
2. **A grant** says which role gets it. A role is a Unix user with its own node, because that's the boundary the
   machine enforces: every process of one user can read what any of its sessions can. Grants never name a session.
   Sessions start, end and get renamed all day; a new session in a role has the role's secrets with no setup.
3. **Sealed delivery**: the source's node seals the secret to the target node's key, and the control plane relays
   ciphertext it can't read and never stores ([protocol, section 8](protocol.md#8-sync-and-sealed-secrets)). Your
   [access policy](access-control.md) decides which nodes may receive which bundles (`sync`), and the first delivery
   to each node waits for your approval.
4. **A writer** on the target puts it where tools look: an env file, a file, a keychain item. Each file is mode 0600,
   replaced in one rename, and never written over a file a person made.
5. **Readings** say when each secret last arrived and when it expires; never its value. The control plane's audit log
   records every delivery; never the value.

Every plugin that writes secrets on a node also lists them in the node's **registry**
(`~/.local/state/fleetlayer/secrets/`, readable only by the node's user), by reference: `op://vault/item/field`,
`cookie://x/ct0`, `claude://account/2`. That's how the node knows what to scrub.

## Scrubbing

The node knows exactly which secrets it delivered, so it removes exact matches, with no guessing, from everything it
sends the control plane: messages, events, readings, session titles, summaries and catch-ups. A match is replaced by
`[REDACTED <reference>]`, such as `[REDACTED op://github/deploy-bot/token]`.

It matches the forms a value takes once something has encoded it:

- the value itself, and without a trailing newline;
- JSON-escaped, once and twice (a value in a JSON string, and in JSON carried inside a JSON string, as transcripts
  and tool results do);
- URL-encoded;
- base64, standard and URL-safe, wherever it starts in a longer encoded text;
- for values of 32 characters or more, a long prefix (half the value, at least 16 characters), so a token cut short
  on screen is still caught.

Values shorter than 8 characters aren't matched (too many ordinary strings would look like them); the
`secrets.delivered` reading lists them under `tooShort`.

**A match means the secret has already leaked**: it was on a session's screen and went to the model's API before the
node caught it on the way out. So the node also:

- logs it, naming the reference and form, never the value;
- publishes a `secrets.redacted` reading keyed by the reference (`{reference, forms, hits, firstAt, lastAt,
  lastContext, action}`) and a `secrets.redacted` event on the `secrets` channel, for whichever agent watches for it;
- fails its heartbeat for a day, with a message that says to rotate the secret.

Rotate it. Redaction keeps the value out of the fleet view, other agents' messages and the control plane, but it
can't take it back from the model provider or the session's own transcript.

Not scrubbed yet: what goes over a tool stream (a shared MCP server's calls and results), terminal streams, and pages.

## Removing everything from a node

`fleetlayer secrets forget` (and a role's window ending) asks every plugin on the node that applies bundles to remove
what it wrote and its registry list, removes whatever is left in the registry, and counts those bundles as never
applied, so the next delivery puts them back. The node keeps redacting the removed values until it restarts, since
its sessions saw them.

## Readings

| Kind | Key | Data |
|---|---|---|
| `secrets.delivered` | node | `{references, redacted, tooShort, errors}`: what the node redacts now (references only) |
| `secrets.redacted` | reference | `{reference, forms, hits, firstAt, lastAt, lastContext, action}` |
