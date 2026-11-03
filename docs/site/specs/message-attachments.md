# Attachments in assistant messages

Status: **implemented** (2026-11-02; client protocol 1.73.0, plugin
protocol 1.38.0). The user's decisions are in section 10, and the
[implementation notes](#implementation-notes) say where the code differs.
Related: [Assets](assets.md), whose cards, sandbox, and permission
checks this reuses.

## 1. Problem

An agent that wants to give the user a file has to save it as an asset
(`write_asset`), even for a picture it shows once. An asset is kept,
listed, and can be replaced, which a one-off picture in a reply does not
need. Models also say what they want to show in the middle of their text,
where the picture belongs, rather than in a tool call before it.

## 2. Goals and non-goals

**Goals:**

- An agent attaches a file from one of the session's machines by writing
  a tag in its reply, `<attach src="…"/>`, where the file should show. The
  same tag attaches an artifact of the session (`artifact="…"`) or shows
  an asset (`asset="…"`), which replaces `show_asset` (section 11).
- The file is read once and kept with the session; nothing watches it.
- Every media type works, as for assets: pictures, video, and audio play,
  HTML pages run in the same sandbox, Markdown renders, and anything else
  downloads.
- ACP agents (Claude Code, Codex) can attach files the same way.
- The user can save an attachment as an asset.

**Non-goals:**

- Attachments of files and artifacts are not assets: no handle, no
  replacing, no listing, and no Assets tab. "Save as asset" makes an asset
  of one.
- Attachments from URLs. `src` is a path on a machine.
- Attachments in user messages, which have their own upload.

## 3. The tag

```markdown
The loss falls fastest in run 14:

<attach src="runs/loss.png" machine="labpc" title="Loss curves"/>

The full table: <attach src="runs/summary.csv"/>

The explorer, live: <attach asset="asset_3f9a1c2e"/>
The output it cut: <attach artifact="9f2c…e1" title="build.log"/>
```

| Attribute | Meaning |
|---|---|
| `src` | A path on the machine, resolved as `read` resolves one: against the machine's working directory, or under `~`. |
| `artifact` | An artifact of the session tree, by the 64-character hex SHA-256 the model sees it by (a `read_artifact` hint names it). |
| `asset` | An asset the session can use, by its handle: shown live, as `show_asset` showed one. |
| `machine` | Optional, with `src` only. The machine, named as the machines block and `list_runtimes` name it. Omitted: the machine of the session's workspace; for an ACP agent, the machine it runs on. |
| `title` | Optional, at most 200 characters. Omitted: the file's name, the asset's title, or the artifact's digest's start. |

A tag names exactly one of `src`, `artifact`, and `asset`.

- **Only a tag in the reply's text is an attachment.** One in inline code
  or a code block is text, so an agent can write about the tag. The
  Markdown parser tells them apart: an attachment is an HTML event whose
  whole content is one `<attach …/>` tag.
- **The tag is `attach` alone.** Other HTML stays escaped text, as now.
- **Attributes** are double- or single-quoted; unknown attributes are
  ignored; a tag without `src`, or not closed with `/>`, is text.
- **Limits:** a file of at most 25 MiB, as for assets, and at most 12
  attachments in one message. The thirteenth and later fail with "too many
  attachments in one message".

## 4. Resolving an attachment

- **When:** once the message that holds the tag is committed, when the
  model's response ends (an ACP agent's step, the same): only then is the
  tag's place, its text part and order, fixed. The run goes on meanwhile;
  the outcome is recorded as soon as the engine has it. While the message
  streams, clients show a complete tag as attaching.
- **Once:** the file is read through the machine's file transfer, as
  `write_asset` reads one, and its bytes go into the session tree's
  artifact store by their SHA-256, where a model's own images go. Nothing
  reads the file again or watches it, and the attachment goes with the
  session tree.
- **The permission:** the read is checked as the `read` action on that
  path and machine, with the rules of the session that wrote the tag:
  - **allow:** the file is attached;
  - **deny:** the attachment fails with "not allowed";
  - **ask:** the attachment waits for the user. Its card asks "primary
    wants to attach `runs/loss.png` from labpc", with *Allow* and *Deny*
    (section 6). The run does not wait for the answer. *Allow* reads the
    file then, as it is then.
- **Failures** say why: the file is missing, is a folder, or is too large;
  the machine is unknown or offline; the runtime cannot send files
  (`files: true`); the read was not allowed or was denied; too many
  attachments.
- **Ending the run** does not cancel a read in progress, nor an attachment
  waiting for the user.
- **An artifact** is attached without a read of any machine and without a
  permission check: it is the session's own. It must be in the session
  tree's artifact store. Its media type comes from its first bytes, then
  from the title's extension, as an asset's does.
- **An asset** is checked as `asset` with the resource `read`, as
  `show_asset` was: allowed, it is shown; denied, it fails; a rule that
  asks waits for the user, as for a file. It is shown as it is now, so a
  replaced asset shows its new content and a deleted one says so.

## 5. What is recorded

- **The event `assistant_attachment`**, one per tag and outcome:

  ```text
  assistant_attachment {
    turn_seq, content_index,   the text part the tag is in
    ordinal,                   which tag of that part, from 0
    src, machine?, title,      as written, the title filled in
    outcome:
      pending_approval
      | attached { file: PersistedFilePart }   the artifact, its type and size
      | shown { asset: AssetBlock }            an asset, as it was when shown
      | failed { reason }
  }
  ```

  A tag's later event replaces its earlier one: `pending_approval`, then
  `attached` or `failed`.
- **The model's history keeps the text as written.** The model never gets
  the file back: it wrote the reference.
- **Failures reach the model** before its next request, as one line each,
  materialized as user history (as tool-emitted system messages are, so
  the cached prefix stays the same). The engine holds them until its next
  request, after the round's tool results, so a line never splits a call
  from its result; a daemon restart in between drops them:

  ```text
  [attachment not attached] runs/loss.png on labpc: no such file
  ```

  The transcript shows the line, as it shows everything the engine adds to
  a model's input. An ACP agent gets the same line with its next prompt.

## 6. In the clients

**Web:** the tag's place in the text shows the attachment, in the shelf's
card styles ([assets](assets.md) section 8.1). Tags written next to each
other, with only white space between them, are one row of cards whose
widths let them share rows, pictures and video gathered in one deck, as
the shelf was:

- an image or SVG as a picture, which opens the media viewer;
- video and audio with a player;
- an HTML page as a card with a live thumbnail, and a Markdown document as
  a card with its first lines; both open in the side panel, the page in
  the same sandbox as an asset's (section 7);
- anything else as a file chip with a download button.

Every card of a file or an artifact has *Save as asset* (section 8); an
asset's card has *Copy handle* instead, and follows the asset live. While
the file is read, the card says it is attaching; a failed one names the
file and why; one waiting for the user asks, with *Allow* and *Deny*.
Handles of shown assets named in the text are chips, as before.

**TUI:** the tag's place shows one line, such as
`📎 Loss curves — loss.png (image/png, 48 KB)`, with a link into the web
page, `#session=<id>&attachment=<sha256>`, which opens the session there
and the attachment as its card does. One waiting for the user shows
*Allow* and *Deny* as click targets, with hover, as the TUI's other
buttons.

**Older clients** show the tag as text, as they do now.

## 7. The sandbox

An HTML attachment runs exactly as an HTML asset does
([assets](assets.md) section 8.2): the page in an iframe whose `srcdoc` it
is, inside an iframe of `viewer.html`, both with `sandbox="allow-scripts
allow-popups"`, so its origin is opaque. `viewer.html` is served with its
own Content-Security-Policy.

What it allows: scripts, inline and `eval`; loading scripts, styles,
fonts, images, and media from anywhere; `fetch`, XHR, WebSockets, and
workers to anywhere; frames; opening links and windows (`window.open`,
`target="_blank"`).

What it does not allow, besides reaching the page:

- **Storage and cookies:** its origin is opaque, so `localStorage`,
  `sessionStorage`, IndexedDB, and cookies throw or are empty. A page that
  needs to remember something cannot.
- **Its requests come from origin `null`:** a server that checks CORS must
  allow `null` or `*`, and no cookies are sent. Public APIs and CDNs work;
  a site that needs a login does not.
- **Forms do not submit** (no `allow-forms`; the policy's `form-action
  'none'`). A form whose script handles `submit` itself still works.
- **No dialogs:** `alert`, `confirm`, `prompt`, and `print` do nothing (no
  `allow-modals`).
- **No downloads** started by the page (no `allow-downloads`); the card's
  *Download* still works.
- **No navigating the cookie page** (no `allow-top-navigation`).
- **Windows it opens are sandboxed too** (no
  `allow-popups-to-escape-sandbox`).
- **No camera, microphone, location, clipboard writes, full screen,
  pointer lock, or other permissions:** the frames grant none. The panel's
  own *Full screen* works, from outside the page.
- **No plugins** (`object-src 'none'`).
- **It shares the tab's process:** a page that loops forever slows the
  web client.

## 8. Save as asset

- **The card's *Save as asset*** asks for the scope, *This session* (the
  default) or *Project*, and makes an asset of the attachment's bytes,
  titled with its title, as `write_asset` would: a new handle, the session
  that wrote the message as its writer.
- **The method `attachment.save_asset { session, turn_seq, content_index,
  ordinal, scope }`** returns the asset. It is the user's action, so no
  rule is checked. An attachment not attached is refused.
- **The card then names the asset's handle,** which the user can copy and
  give an agent.

## 9. Protocol and compatibility

**Client protocol, minor:**

- the event `assistant_attachment` (section 5);
- the methods `attachment.decide { session, turn_seq, content_index,
  ordinal, allow }`, which answers one waiting for the user, and
  `attachment.save_asset` (section 8);
- the link `#session=<id>&attachment=<sha256>`.

**Plugin and runtime protocols:** no change. Reading the file needs
`files: true` (runtime protocol 1.13.0), as `write_asset` does.

**History:** a new event kind, which earlier builds skip, and no
converter: the text is unchanged.

**Configuration:** none. The `read` rules decide what can be attached.

**The prompt:** a block on the tag, `attachments`, joins every session's
system prompt after the runtimes block; a session with only `global` is not
told of the `src` form. It is composed when a run starts and
frozen with it, and the system prompt row shows it. ACP agents get it in
the context cookie sends with their first prompt, after the machines block.

## 10. Decisions

**Decided by the user (2026-11-01):**

- **The tag is `<attach …/>` alone,** for every type.
- **A read whose rule asks waits for the user,** whose card asks.
- **The model is told when an attachment failed,** with its next request.
- **ACP agents attach files too,** from the start.
- **HTML attachments run live** in the same sandbox as assets.
- **Cards have *Save as asset*** from the start.
- **The tag also takes `artifact`,** an artifact of the session by its
  digest, and **`asset`,** an asset by its handle.
- **`show_asset` is removed,** and so are `write_asset`'s `show` and
  `caption`: an agent shows assets and files with the tag, and its own
  text is the caption (section 11).
- **No shelf:** cards show where their tags are, tags next to each other
  in one row.
- **Old figure calls** (`show_asset`, `write_asset` with `show`) render as
  plain tool rows, their assets as figures in the open row.
- **One read, when the tag closes,** and never again.
- **Attachments live in the session tree's artifact store,** and go with
  it; they are not listed anywhere but their message.
- **At most 12 per message, 25 MiB each,** as `show_asset` and assets.
- **The run never waits** for a read or for the user's answer.

No questions are open.

## 11. Assets without show_asset

- **`show_asset` is gone** from the `assets` group, and `write_asset` loses
  `show` and `caption`; its description, and the group's summary, say to
  show an asset with `<attach asset="…"/>`.
- **The shelf is gone:** the engine no longer gives calls the `figure`
  hint, and clients no longer gather figure calls' assets under the text
  after them. A call of an older build that showed assets is a plain tool
  row; open, it shows its `asset` blocks as figures. The `asset` display
  block and the `figure` hint stay in the protocol for those logs.
- **The tag's cards** take the shelf's place: the same cards, where the
  agent writes them.

## 12. Phases

1. Engine and protocol: parsing tags in text parts (native and ACP), the
   read, the permission, `assistant_attachment`, `attachment.decide`,
   failure lines, the prompt section.
2. Web: cards in the text, the approval card, the side panel, *Save as
   asset* and `attachment.save_asset`, the link.
3. TUI: the line, the link, *Allow* and *Deny*.
4. Docs and the browser scenario.

## Implementation notes

Implemented on 2026-11-02. Where the code differs from the sections above:

- **The tag's parser is shared:** `cookie_agent_protocol::attach_tags` and
  `split_attach_tags` read tags from the Markdown's HTML events, with the
  options every client parses replies with
  (`cookie_agent_protocol::markdown_options`).
- **Rules and the permission mode:** a tag's read is checked as a tool's
  would be, so the session's permission mode applies: in yolo mode nothing
  asks. A rule that asks never goes to the approval reviewer; the card
  asks the user.
- **Failures waiting to be told are held in memory:** a daemon restart
  between a failure and the model's next request drops them. An answer to
  a tag that waits finds the tag in the log after a restart.
- **The failure note** is a `message_injected` user message, which clients
  show as a warning row, "The agent was told:" and its lines. An ACP agent
  reads it at the start of its next prompt, beside the machines note.
- **The prompt section** is a block of the engine's own, `attachments`,
  after the runtimes block, not a tool provider's instructions: it holds
  whatever tools the session is offered, and reaches ACP agents beside the
  machines block (the user, 2026-11-02; it was first the `read_artifact`
  provider's "Attachments" section). Without a runtime besides `global` it
  leaves out the `src` sentence (the user, 2026-11-02).
- **The TUI** names a tag `📎` and its label in the text, and lists the
  part's attachments under it, each a line; its *Allow* and *Deny* are
  lines of their own, each a click target.

