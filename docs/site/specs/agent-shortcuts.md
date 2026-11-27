# Agent shortcuts: the resolved document, and links from the composer's menus

Status: **implemented** (2026-11-26; client protocol 1.103.0), approved
with the [preview](agent-shortcuts/preview.html). The user's decisions
are in [section 8](#8-decisions-2026-11-26), and where the code differs is
in [section 9](#9-implementation-notes).
Related: [agents](../extend/agents.md) (inheriting an agent, templates),
[ACP onboarding](acp-onboarding.md) section 7 (the agents panel),
[web polish](web-polish.md) P4 and P5 (the model menu and the agent rows).

## 1. Problem

- **The agents panel's rows do nothing when clicked.** The official
  agents, the ACP agents, and the templates above the documents answer
  only ⋯ and a right-click. Nothing shows what an agent such as
  `explore` runs.
- **An agent that inherits cannot be read whole anywhere.** A document
  with `inherit:` writes only what it changes. To see what the agent
  actually runs, a user has to merge the chain by hand. That chain can be
  three documents long, for example `agents/codex.md` →
  `agents/_acp.md` → `official_agents/_acp.md`, and the merge is per key
  with `permissions` merged per action.
- **The composer's menus lead nowhere.** The agent menu and the model menu
  list choices, but changing an agent or connecting a provider means
  leaving the session and finding the right view.

## 2. Goals and non-goals

**Goals:**

- Clicking an agent's name in the Agents view opens the agent on a
  **Resolved** tab: its document after every `inherit:` and
  `{{default_prompt}}` is applied, read-only.
- Resolved shows the chain of documents, marks the document each key came
  from and what it replaced, and keeps the documents' comments.
- The agent menu ends with **Edit agents**. The model menu ends with
  **Edit model providers**.

**Non-goals:**

- Editing in Resolved. The user's document is still edited in Form and
  File.
- A resolved view in the TUI or the CLI. Both can use the same call later.
- Changing how agent documents load. The loader's merge stays as it is
  (section 4.3).
- Resolving `${workspace_dir}` and other per-session values. Only the
  model variables are resolved (section 4.4).

## 3. The web client

### 3.1 Opening an agent

- **Panel rows** (official, ACP, templates). The name and the empty part
  of the row open the agent. ⋯ and right-click keep their menus.
- **Document rows** (Shared agents, each preset). A click opens the
  user's document, as today, but on **Resolved**, not File (decision 2).
- **New agent** still opens the new document on File.
- An agent with a document of the user's opens that document's editor,
  with the tabs **Form · File · Resolved**. Apply and Delete stay. Apply
  is disabled on Resolved, which has nothing to apply.
- An agent with no document of the user's (an official agent, an ACP
  agent that follows its template, an official template) opens a
  read-only page. It shows its name, where it comes from, Resolved, and
  **Customize** and **Turn off** as buttons. Each button runs the panel
  action of the same name.
- A phone shows the open agent as the page, as the editor is today.

### 3.2 Resolved

Top to bottom:

1. **The chain.** It reads "Inherits", then a pill per document from the
   agent's own to the root, such as `agents/primary.md →
   official_agents/primary.md`. An ACP agent with no document starts with
   a "no document of yours" pill. A pill opens that document: the user's
   documents in their editor, an official one read-only. A document that
   inherits nothing reads "From `official_agents/explore.md`. It
   inherits nothing."
2. **A checkbox,** "Mark where each key comes from", shown only when the
   chain has more than one document. It is on by default and is kept in
   the browser.
3. **The document,** in the editor's monospace face:
   - The frontmatter, with comments. A bar at each line's left takes the
     colour of the document the key came from: the accent for the
     agent's own document, a second hue for a document between, and a
     faint grey for the root. Beside each key's first line, a tag names
     that document and, when the key replaced an inherited one, what it
     replaced: "was `${default_model}`" for a one-line value, or
     "replaces 23 inherited rules" for a block.
   - Model variables are followed by what they resolve to, such as
     `${small_model}` → `anthropic/claude-sonnet-5-5 · medium`.
   - The body. Where `{{default_prompt}}` expanded, a line reads
     "`{{default_prompt}}` expands to official_agents/primary.md's
     prompt", and the inherited prompt follows with that document's bar,
     folded to its first 6 lines behind "Show all N lines".
4. **A note** when the editor's text differs from the saved document:
   "Resolved shows the applied document. Apply to see your changes."
5. **A note** when the daemon could not keep the comments (section 4.3):
   "Shown without comments".

On a phone the tags move under the line they label, so the code keeps its
width.

### 3.3 The menu links

- The agent menu (the agent chip's popover, and the Agent section of a
  phone's composer sheet) ends with a link row, **Edit agents**. It opens
  the Agents view with nothing open (decision 1) and closes the menu.
- The model menu ends with **Edit model providers**, below the effort
  control. It opens Settings › Providers and closes the menu.
- An ACP agent's model menu lists the harness's own models and has no
  providers link. A guest sees neither link.
- Each link is a row pinned under the list, outside it. The search
  field, the arrow keys, and Enter work as before.

## 4. The daemon

### 4.1 `agents.resolved`

A new read-only request (client protocol 1.103.0):

```text
agents.resolved
  params:  { path: "agents/primary.md" }        // a document of the tree
        or { id: "claude", preset?: "work" }     // an agent, with or without a document
  result:  {
    text: "---\n...\n---\n...",       // the resolved document
    comments: true,                    // false: the comment-less fallback (4.3)
    chain: ["agents/primary.md", "official_agents/primary.md"],
    own_document: true,                // false: the agent has no document of the user's
    keys: [ {
      key: "models",                   // or "permissions.bash", for an action
      source: "agents/primary.md",     // the chain document it came from
      first_line: 4, last_line: 5,     // 1-based lines of `text`
      replaced: "  - ${default_model}" // the inherited block it replaced, as written
    }, ... ],
    prompt: { source: "official_agents/primary.md", first_line: 22, last_line: 34 },
    values: [ { name: "${default_model}", value: "anthropic/claude-opus-5-5 · high" } ]
  }
```

- `path` names any agent document or template: `agents/<id>.md`,
  `agents/<preset>/<id>.md`, `official_agents/<id>.md`, or a `_<name>.md`
  of those folders. The chain pills use it.
- `id` (with `preset`) names an agent as the panel lists it. An agent
  with a document of the user's resolves that document. An ACP agent with
  none resolves the template it follows, as the loader picks it: the
  preset's `_acp`, then `agents/_acp.md`, then `official_agents/_acp.md`.
  In that case `own_document` is false. An official agent with no
  document of the user's resolves its official document.
- `prompt` is absent when no `{{default_prompt}}` expanded, and `values`
  is empty when the frontmatter names no model variable.
- **Errors:** an unknown document or agent, a chain that names a document
  that does not exist yet, and a cycle return the error the loader gives
  for them, as `invalid_params`.

### 4.2 Resolving with comments

A new module of the configuration crate, `agent_resolved.rs`, beside
`agent_inherit.rs`. It works on the text of each document in the chain:

1. Read each document's frontmatter as **blocks**. A block is a top-level
   key with every line under it (`models:` and its list), or one action
   under `permissions:` (`  bash:` and its rules). A block keeps its
   comments: the comment lines just above it, and the comments on its
   lines.
2. Pick the blocks by the loader's rules. A key the document writes
   replaces the inherited one, `permissions` is replaced per action (a
   `bash` action replaces every inherited `bash` and `bash@…` key), and
   `inherit` is dropped. Keys keep the root document's order, and keys
   added further down the chain follow it.
3. Join the blocks into the frontmatter. Each block keeps its source and
   lines for `keys`, and a replaced block keeps its text for `replaced`.
4. The body is the loader's: the agent's own body with
   `{{default_prompt}}` replaced, or the inherited body when its own is
   empty. `prompt` records the lines the inherited prompt took.

### 4.3 The check, and the fallback

The joined text is parsed, and its frontmatter compared with the
mapping the loader's merge (`agent_inherit`) produces for the same
document. When they are equal, the text is the result, with
`comments: true`. When they differ, or a document's frontmatter does not
read as blocks, the result is the loader's mapping written as YAML,
with `comments: false`. That can happen with flow-style
`permissions: { ... }`, a block scalar that starts at column 0, or
anchors. Its `keys` come from the mappings instead of the text.

So the two merges can never show different things. The loader's merge
decides what an agent runs, and the display merge only adds the
comments.

### 4.4 Model variables

`values` lists the model variables the frontmatter names
(`${default_model}`, `${small_model}`, `${utility_model}`), each with
the model and variant it resolves to in the daemon's configuration.
`${parent_model}` depends on the run, so it is left out, as are
`${workspace_dir}` and the other per-session values.

## 5. Protocol and versions

- Client protocol **1.103.0**: `agents.resolved`, `AgentsResolvedParams`,
  `AgentsResolvedResult`, `AgentResolvedKey`, `AgentResolvedPrompt`, and
  `AgentResolvedValue`. A 1.102.0 daemon does not know the method. The
  web client then shows "This daemon cannot show resolved documents;
  update it", and the File tab works as before.
- No configuration, history, plugin, or runtime protocol change.

## 6. Documentation

- [`extend/agents.md`](../extend/agents.md), "Inheriting an agent": one
  paragraph on reading the result in the web client's Resolved tab.
- [`clients/web.md`](../clients/web.md): the Agents view (clicking a
  name, Resolved), and the two menu links.
- [`reference/protocol.md`](../reference/protocol.md) and the changelog:
  `agents.resolved`.

## 7. Tests

Following the user's rule, no new tests are written unless the user asks
for them, and existing tests that the change breaks are fixed. The
protocol's schema corpus is regenerated, since the protocol crate's tests
check it.

## 8. Decisions (2026-11-26)

1. **Edit agents** opens the Agents view with nothing open, not the
   current agent.
2. Every click on an agent's name opens it on **Resolved**, including the
   rows of the user's own documents. New agent still opens File.
3. Resolved keeps the documents' comments, through the display-only merge
   of section 4.2 with the check and fallback of section 4.3. The
   loader's merge is unchanged.

## 9. Implementation notes

- **Errors.** `agents.resolved` returns the configuration's own errors (an
  unknown document, a cycle, an `inherit` with nothing there yet) as the
  other agents-panel calls do, not as `invalid_params`.
- **The tag's wording.** A key that replaced a one-line value says
  "was `<value>`", and a longer block says "replaces N inherited rules"
  for a permission action and "replaces N inherited lines" for any other
  key.
- **The prompt's line.** It reads "The prompt of `<document>`". It marks
  both a `{{default_prompt}}` that expanded and a body left empty, which
  takes the inherited body whole. The fold under the prompt's first
  lines reads "Show N more lines".
- **The loader's quirk, kept.** A document that writes both `bash` and
  `bash@<runtime>` keeps only the later of them, because each action
  removes every key of its kind kept so far, those of the same document
  among them. Resolved shows what the loader does.
