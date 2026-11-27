---
name: pages
description: Show a person something (QA screenshots, a report, a comparison, a mock-up, a decision write-up) as a page in your space of the pages vault, hosted by Fleetlayer's pages service, and give them its link. Use in place of vendor artifacts and other hosted previews, whenever you would publish an artifact, show screenshots or say "here is a page".
---

# Pages

A page for a person to look at is a folder in your space of the pages vault. The pages service hosts whatever is in
the folder: there's no publish step. Vendor artifacts open only for the account that published them; a page opens from
one link for everyone who may read the space, whichever agent or model wrote it.

## Where

- Your space's folder: `~/Pages/` (each agent user has its own space mounted or synced there). If it isn't there, ask
  your person where your space is; don't write into another agent's folder.
- The address: `https://<pages address>/<space>/<folder>/`. Your setup's `AGENTS.md` names the pages address and your
  space.

## Make one

1. **Make a folder** named `<yyyy-mm-dd>-<short-name>` (lower case, hyphens), with `index.md` and the images in
   `img/`:

   ```
   ~/Pages/2026-11-27-signing-flow-qa/
     index.md
     img/desktop-light.jpg
   ```

   Write into a folder in your scratch directory first and move it into place when it's ready, or write straight into
   `~/Pages/`: an open page updates within a second as its files change.

2. **Write the Markdown:**
   - The first line is `# The page's title`. The paragraph after it is the summary: what this shows, from when and from
     where. It's what the list of pages shows.
   - Lead with what the reader needs (what passed, what was found, what wasn't tested, what you need them to decide),
     then the evidence.
   - An image is `![What it shows](img/shot.jpg)`; the text in the brackets is its caption. Images on consecutive lines
     (no blank line between them) make a gallery; phone screenshots get narrower columns. An image in a paragraph of
     its own is shown at its own size. Every image opens in a lightbox.
   - `> [!NOTE]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!WARNING]` and `> [!CAUTION]` on the first line of a block quote
     make a callout.
   - Tables, lists, task lists, code blocks, footnotes and links are ordinary Markdown. Link to the page's own files by
     relative path, and to other pages in the space with `../<folder>/`.
   - No raw HTML (it's shown as text) and no scripts. For a chart, render an image.
   - Screenshots as JPEG (quality about 82), PNG only where it matters, each under 5 MB.
   - Never put credentials or tokens in a page or its images (crop or mask a signing link), nor anything the space's
     readers mustn't see.

3. **Check it:** fetch the page's address. A page with problems (no title or summary, a missing image, raw HTML) shows
   them in a box with `id="problems"` at the top; fix them until it's gone.

4. **Give the person the link** on a line of its own, with one sentence on what the page shows.

## A mock-up that has to be HTML

When the page itself is the thing to look at (a design mock-up with its own layout and CSS), put `index.html` in the
folder in place of `index.md`. It's shown in a sandboxed frame. HTML and CSS only: no `<script>`, no `on…=`
attributes, nothing from other sites; its own files by relative path; a `<title>` (its name in the list) and
`<meta name="description" content="…">` (its summary).

## Update one

Edit the files in the folder. The link stays the same and an open page shows the change by itself. A new round that
should be kept beside the old one gets a new folder (a new date or name).

## With no file sync

On a machine without the vault (a cloud VM with a Fleetlayer node), `fleetlayer pages push <folder> --space <space>
--name <name>` sends the folder to the pages node (planned), checked against the access policy; pushing again sends
only what changed.

## Keep the record

The page is how someone looks at the work. The record (the plan, review notes, what was decided) still goes where it
always does: the PR, the issue, the notes. Link the page from there.
