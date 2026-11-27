# Your Mac's screenshots and downloads, for agents anywhere

You take a screenshot of a broken page, or download an invoice or a log file, and want an agent to look at it. When
the agent runs on the same Mac, you paste the path. When it runs on your home server or a VPS, you're copying files
around, every time.

This guide shares a few chosen folders on your Mac with the agents you pick, wherever they run. You say "look at the
screenshot I just took", and the agent fetches it and sees it. It's read-only and reaches only the folders you name.

## How it works

- **`fleetlayer-folders`**, which ships with Fleetlayer, is a small MCP server that lists and reads the folders it's
  given ([reference](../plugins/folders.md)).
- **The Mac's node shares it as a tool**, like [the shared browser](browser.md). Each session that connects gets its
  own copy.
- **An agent anywhere uses it** with `fleetlayer mcp folders@laptop` as its MCP server command. The control plane
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
  - name: folders
    command: /Users/you/.local/bin/fleetlayer-folders    # the full path: launchd's PATH is short
    args: ["--folder", "screenshots=~/Screenshots", "--folder", "downloads=~/Downloads"]
    allow: ["*@server"]                 # which sessions may connect: <tab>@<node> patterns
    networks: ["192.168.1.0/24"]        # optional: offer it only while the Mac is on your home network
```

Restart the node (`fleetlayer-node install` again, or restart its service) and check `fleetlayer status`.

`networks` works as it does for the browser: away from home the folders are refused unless you opt in for a while
with `fleetlayer tools enable folders --for 2h`.

## 3. Let the node into Downloads

macOS keeps programs out of Downloads, Desktop and Documents until you allow them, and that includes a program the
node starts. The first time an agent reads Downloads, macOS either asks you, on the Mac's screen, to let the node in,
or refuses quietly. The tool tells the agent which, and the agent tells you.

To allow it up front: System Settings > Privacy & Security > **Files and Folders**, find `fleetlayer-node` and turn on
**Downloads Folder**. If it isn't listed, add it under **Full Disk Access** instead: the `+` button, then
Shift-Command-G and the node's real path (`readlink ~/.local/bin/fleetlayer-node` prints it). Restart the node
afterwards.

Each Fleetlayer release is a new binary, and macOS may ask again after an update. Asking an agent to run the `folders`
tool shows at once whether each folder can be read.

## 4. Decide which agents see them

The node's `allow:` list is one half; the control plane's access policy is the other, and both must agree. From
v0.3.8 a fleet with no policy of its own asks you before each session's first use of a shared tool
(`fleetlayer-control approvals list`, then `approvals approve <id>`), and the approval holds for 30 days.

To give your screenshots to more agents than your downloads, share them as two tools instead of one:

```yaml
tools:
  - name: screenshots
    command: /Users/you/.local/bin/fleetlayer-folders
    args: ["--folder", "screenshots=~/Screenshots"]
    allow: ["*@server"]
  - name: downloads
    command: /Users/you/.local/bin/fleetlayer-folders
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

## 5. Use them from another machine

```sh
claude mcp add folders -- fleetlayer mcp folders@laptop
```

Or, in any other harness's MCP settings, the command `fleetlayer mcp` with the argument `folders@laptop`. Then just
ask: "look at my latest screenshot", "read the CSV I downloaded this morning". The agent calls `latest` with
`read: true`, and sees the image.

## When it doesn't work

- **"macOS refused access to the downloads folder"**: section 3.
- **"is kept in the cloud and isn't downloaded"**: the file is an iCloud Drive or Dropbox placeholder. Open it in
  Finder once to download it, and ask again. The tool never starts a download itself.
- **"more than the 8.0 MB this tool sends in one reply"**: big files don't fit through one reply. Images are scaled
  down, so this only hits other files; `--max-read-mb` raises it to 10 at most.
- **Refused, with a reason**: the Mac is asleep, off the networks you listed, or the session isn't allowed. The refusal
  says which.
