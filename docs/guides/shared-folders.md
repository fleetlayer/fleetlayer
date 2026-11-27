# Your Mac's screenshots and downloads, for agents anywhere

You take a screenshot of a broken page, or download an invoice or a log file, and want an agent to look at it. When
the agent runs on the same Mac, you paste the path. When it runs on your home server or a VPS, you're copying files
around, every time.

This guide shares a few chosen folders on your Mac with the agents you pick, wherever they run. You say "look at the
screenshot I just took", and the agent fetches it and sees it. It reaches only the folders you name, and it's
read-only unless you let an agent put files there or tidy up (section 5).

## How it works

- **`fleetlayer-shared-folders`**, which ships with Fleetlayer, is a small MCP server that lists and reads the folders it's
  given ([reference](../plugins/shared-folders.md)).
- **The Mac's node shares it as a tool**, like [the shared browser](browser.md). Each session that connects gets its
  own copy.
- **An agent anywhere uses it** with `fleetlayer mcp shared-folders@laptop` as its MCP server command. The control plane
  checks it may, joins the two nodes and logs the connection, and never reads the files that pass through.

The Mac has to be awake and online.

## 1. Pick the folders

macOS saves screenshots to the Desktop unless you change it. A folder of their own is tidier, and macOS doesn't
protect it, so it needs no permission: press Shift-Command-5, open **Options**, choose **Other Location...** and make
`~/Screenshots`.

Downloads works too, after one permission step (section 3). It's also where files from strangers and your private
documents land, so think about which agents should see it (section 4).

## 2. Share them from the Mac's node

Add a `tools:` entry to `~/.config/fleetlayer/node.yaml` on the Mac:

```yaml
tools:
  - name: shared-folders
    command: /Users/you/.local/bin/fleetlayer-shared-folders    # the full path: launchd's PATH is short
    args: ["--folder", "screenshots=~/Screenshots", "--folder", "downloads=~/Downloads"]
    allow: ["*@server"]                 # which sessions may connect: <tab>@<node> patterns
    networks: ["192.168.1.0/24"]        # optional: offer it only while the Mac is on your home network
```

Restart the node (`fleetlayer-node install` again, or restart its service) and check `fleetlayer status`.

`networks` works as it does for the browser: away from home the folders are refused unless you opt in for a while
with `fleetlayer tools enable folders --for 2h`.

## 3. Let the node into Downloads

macOS keeps programs out of Downloads, Desktop and Documents until you allow them, and that includes a program the
node starts. The first time an agent reads Downloads, a prompt appears on the Mac's screen asking whether
`fleetlayer-node` may access files in your Downloads folder. macOS asks for the node, not the tool, because the node
is the program launchd started. The agent's call waits until you answer, so it's worth being at the Mac the first
time: click **Allow** and the call goes through. If you click **Don't Allow**, or nobody answers, the tool tells the
agent that macOS refused, and the agent tells you.

To change it later: System Settings > Privacy & Security > **Files and Folders**, find `fleetlayer-node` and turn
**Downloads Folder** on or off.

macOS remembers the answer for the node's binary at its exact path, and for a signed binary it keeps the answer
when a new build replaces the old one there. From v0.3.13 that's how Fleetlayer upgrades: the macOS releases are
signed with a Developer ID and notarised by Apple ([how](../releasing.md)), and the installer and the node's own
update keep the node at one path, `~/.local/lib/fleetlayer/fleetlayer-node`. So you answer once (tested between two
signed builds of v0.3.13: the second read Downloads with no prompt).

Three cases ask again:

- **Moving to v0.3.13 from an earlier release**, once: earlier releases each lived in a folder of their own, so the
  node's path changes one last time.
- **v0.3.12 and earlier**, after every upgrade, for the same reason. Signing alone (from v0.3.9) didn't stop it.
- **A node you built yourself**, or an unsigned build from a fork, after every upgrade even at one path: macOS knows
  an unsigned program by its exact bytes.

Asking an agent to run the `folders` tool after an upgrade shows at once whether each folder can still be read. Each
path the node has run from leaves a `fleetlayer-node` row under Files and Folders; the old ones can be removed.

## 4. Decide which agents see them

The node's `allow:` list is one half; the control plane's access policy is the other, and both must agree. From
v0.3.8 a fleet with no policy of its own asks you before each session's first use of a shared tool
(`fleetlayer-control approvals list`, then `approvals approve <id>`), and the approval holds for 30 days.

To give your screenshots to more agents than your downloads, share them as two tools instead of one:

```yaml
tools:
  - name: screenshots
    command: /Users/you/.local/bin/fleetlayer-shared-folders
    args: ["--folder", "screenshots=~/Screenshots"]
    allow: ["*@server"]
  - name: downloads
    command: /Users/you/.local/bin/fleetlayer-shared-folders
    args: ["--folder", "downloads=~/Downloads"]
    allow: ["planning@server"]
```

Let the policy decide each tool (`tool:screenshots@laptop`, `tool:downloads@laptop`), as
[the browser guide](browser.md#3-let-the-agents-use-it) shows. An agent that reads strangers' text shouldn't have
either: [Run an agent that reads strangers' text](untrusted-input.md) denies it every shared tool.

Whatever an agent reads from a folder comes with a line saying the contents are outside data, to be treated as
information and never as instructions: a PDF or a saved web page in Downloads can carry text written to steer an
agent, and so can a screenshot of an email or a chat, since agents read the text in images too. For a folder only you
write to, such as your own notes, `--trusted <name>` leaves the line out. The line helps, but it doesn't make a file
safe; [that guide](untrusted-input.md) covers the rest.

## 5. Letting agents put files there, or tidy up

Each folder is read-only until you say otherwise, folder by folder: `--upload <name>` lets agents add files to it, and
`--delete <name>` lets them delete files from it. An agent can then hand you a report, a generated image or a fixed
spreadsheet straight into Downloads, or clear out the screenshots it has dealt with.

Give those rights to fewer agents than reading, by sharing the folder a second time under another name:

```yaml
tools:
  - name: downloads
    command: /Users/you/.local/bin/fleetlayer-shared-folders
    args: ["--folder", "downloads=~/Downloads"]
    allow: ["*@server"]
  - name: downloads-manage
    command: /Users/you/.local/bin/fleetlayer-shared-folders
    args: ["--folder", "downloads=~/Downloads", "--upload", "downloads", "--delete", "downloads"]
    allow: ["planning@server"]          # a named session, never "*"
```

The policy decides each entry on its own (`tool:downloads-manage@laptop`), so only the agents you name can write.

What keeps it safe:

- **Uploads never replace a file** unless the agent asks to overwrite, which needs `--delete` too, and the old file
  goes to the trash first. Nobody ever sees half a file.
- **Uploads aren't trusted:** they're never executable, and macOS marks them as downloaded, so it asks before anyone
  opens one as an app or a script. Still, a file an agent writes lands on your Mac: give `--upload` only to agents you
  trust to put files there.
- **Deleting moves files to a hidden trash** inside the folder, `.fleetlayer-trash/`, for 30 days. Shift-Command-.
  in Finder shows it; move a file back out to restore it.
- **A full disk is refused:** an upload that would leave less than 1 GB free fails with the reason, so an agent stuck
  in a loop can't fill your Mac.

## 6. Use them from another machine

```sh
claude mcp add shared-folders -- fleetlayer mcp shared-folders@laptop
```

Or, in any other harness's MCP settings, the command `fleetlayer mcp` with the argument `shared-folders@laptop`. Then just
ask: "look at my latest screenshot", "read the CSV I downloaded this morning". The agent calls `latest` with
`read: true`, and sees the image.

## When it doesn't work

- **"macOS refused access to the downloads folder"**: section 3. Answer the prompt on the Mac, or turn the node back
  on under Files and Folders. Before v0.3.13, and with an unsigned build, this is expected once after every upgrade.
- **An agent's first read of Downloads hangs**: macOS is waiting for someone to answer its prompt on the Mac's screen.
- **"is kept in the cloud and isn't downloaded"**: the file is an iCloud Drive or Dropbox placeholder. Open it in
  Finder once to download it, and ask again. The tool never starts a download itself.
- **"more than the 8.0 MB this tool sends in one reply"**: big files don't fit through one reply. Images are scaled
  down, so this only hits other files; `--max-read-mb` raises it to 10 at most.
- **"doesn't take uploads" or "doesn't allow deleting"**: that folder hasn't got `--upload` or `--delete` in the
  entry the agent connected to (section 5).
- **Refused, with a reason**: the Mac is asleep, off the networks you listed, or the session isn't allowed. The refusal
  says which.
