# Documentation Overhaul

Status: Draft for discussion. Broad documentation restructuring is not yet
authorized. Correctness updates required by the approved model-configuration
implementation are separate and may proceed with that implementation.

## Purpose

Help users install, configure, and operate cookie-agent without needing to
understand its internal implementation. Keep behavioral contracts precise while
removing duplication and details that do not affect a reader's decisions.

This document is maintained directly during planning. Use `story-writer` for
the eventual documentation editing, with technical review against source code.

## Problems to Address

- Similar permission and configuration explanations are repeated across guides.
- Some references describe types without explaining when a user needs them.
- Internal implementation details can obscure ordinary workflows.
- Confusing or inaccurate descriptions have survived prior audits, including
  the ineffective `api_path` option and the phrase "media output constraints"
  when current output support is text only.
- Configuration examples must keep pace with breaking schema changes.
- A successful documentation build verifies structure, not behavioral accuracy.

These observations motivate the pass; assess each page before deleting or
rewriting it rather than assuming all detailed documentation is unnecessary.

## Audience and Information Structure

Use the following user-approved navigation structure. Do not retain separate
top-level Start, Use, Configure, or Reference groups.

```text
Guide
  Installation
  Run
  Sessions
  Compaction
  Goal Mode
  Usage and Cost
Engine Configuration
  Agent
  Skill
  config.toml
  MCP [mcp]
  One page for every other top-level engine configuration key
TUI Configuration
  tui.toml
  One page for every top-level TUI configuration key
Development
  Architecture
  Building and testing
  Plugin development
  Rust API
Specs
  Existing specifications
```

### Guide

- Installation ends with the next step: use `/connect` to connect a provider,
  or configure custom models. Link directly to the custom-model section of the
  provider configuration page rather than duplicating the configuration there.
- Run is the home for TUI, separate server/client, and headless modes. Add any
  future implemented running modes here; do not publish placeholder instructions
  for modes that do not exist yet.
- Sessions covers session lifecycle, selection, resumption, and related user
  workflows. Remove its compaction section; use a short contextual link to the
  dedicated Compaction page where needed.
- Compaction explains the workflow and behavior, linking to the applicable
  engine configuration page for fields and defaults.
- Goal Mode separates using goals from configuring their settings.
- Usage and Cost explains usage reporting and accounting, linking to model-local
  pricing configuration for exact rate fields and precedence.

### Engine Configuration

- Agent and Skill cover their authored documents and relevant configuration
  semantics, including permission and tool-visibility behavior.
- `config.toml` covers file locations, discovery, precedence, inheritance,
  replacement versus merging, and shared syntax such as interpolation. It is
  an overview and index, not a second exhaustive field reference.
- Give every top-level engine configuration key its own page, including scalar
  settings as well as tables. MCP `[mcp]` is one such page, not an exception or
  a duplicate entry. Request Header `[headers]` is another example.
- Inventory keys from the final implemented schema and loader before creating
  pages. Do not document removed top-level keys such as `[pricing]` as current
  settings. Nested provider/model/variant settings stay on the provider page,
  with useful section anchors rather than a page per nested key.
- Each key page combines its purpose, examples, defaults, complete accepted
  fields, inheritance/override rules, and relevant constraints. Avoid a parallel
  configuration-reference copy of the same information.

### TUI Configuration

- `tui.toml` covers locations, discovery, precedence, inheritance, and shared
  syntax specific to TUI settings. Do not assume its loading behavior is the
  same as `config.toml`; verify both.
- Give every top-level TUI configuration key its own page. Use labels such as
  Minimum Event Level `[minimum_event_level]`; bracket notation identifies the
  key in navigation and must not imply scalar keys are TOML tables.
- Inventory accepted keys from the TUI schema, with exactly one canonical page
  per key. Keep runtime operation instructions in Guide / Run.

### Development and Specs

- Development contains Architecture, Building and testing, Plugin development,
  and the generated Rust API. Generated rustdoc is not manually rewritten.
- Inventory and collect existing specifications under Specs, including relevant
  protocol, event, schema, and tool contracts and repository design specs.
  Preserve useful contracts rather than deleting them to fit navigation.
- Include the model-configuration and documentation-overhaul specs. Clearly
  label draft, approved, and implemented status so proposals are not mistaken
  for current user configuration. Publish one canonical copy of each spec;
  decide file moves or build inclusion when preparing the page migration map.
- Map every existing page to a destination, consolidation, or justified removal.
  Security guidance belongs next to affected workflows/settings; cross-cutting
  developer contracts may live in Specs. Do not silently orphan existing API,
  tool, permission, or security content.

## Page Responsibilities

### Guides

- Start with the task or decision the reader needs to perform.
- Show a small working example before extensive explanation when practical.
- Explain consequences and meaningful alternatives.
- Link to the canonical reference for complete fields and limits.
- State whether snippets are complete files or fragments.

### Configuration Pages and Specifications

- Specify accepted values, defaults, omission/inheritance semantics, constraints,
  and errors that change how users configure or operate the application.
- Distinguish implemented behavior from capabilities of an upstream API.
- Document settings that only apply to certain adaptors or endpoints.
- Avoid narrating function calls, serialization machinery, or internal structs
  unless the page is explicitly a developer contract.

### Developer Documentation

- Preserve architectural invariants, ownership boundaries, extension contracts,
  and build/test requirements.
- Keep internal details here when they serve contributors, rather than deleting
  useful engineering information solely because end users do not need it.

## Canonical Ownership

| Subject | Canonical owner |
|---|---|
| Permission matching, precedence, visibility, and approvals | Engine Configuration / Agent; link from related settings and workflows |
| Provider setup, custom models, and choosing an adaptor/API | Engine Configuration / Providers `[providers]` |
| Exact model fields, defaults, inheritance, pricing, and validation | Engine Configuration / Providers `[providers]` |
| Engine config locations and layer precedence | Engine Configuration / `config.toml` |
| TUI config locations and layer precedence | TUI Configuration / `tui.toml` |
| Tool arguments, output, retention, and limits | Specs / tool contract coverage |
| Network and local execution trust boundaries | Relevant Guide/configuration pages, linking to exact contracts in Specs |
| Breaking configuration migration | Relevant configuration pages, with an overview in `config.toml` if needed |

Finalize a page ownership map before moving sections. Avoid duplicating full
tables in multiple pages; a brief local summary plus a link is appropriate.

## Required Model Configuration Coverage

Reflect the approved `docs/model-configuration-spec.md` and the reviewed final
implementation, not earlier conversational examples:

- Unified `models`: custom definitions versus inherited managed overrides.
- `generation_options` versus `adaptor_options`, with examples of each.
- Variant creation, sparse updates, base inheritance, precedence, and disabling.
- `default_variant` versus generation settings.
- `request_endpoint = "completions" | "responses"`, with a verified adaptor
  support matrix and omitted-value behavior.
- Model-local pricing, precision, missing-rate behavior, and catalog precedence.
- Automatic native replay versus explicit declarations; separate this from
  prompt caching and avoid cache-hit guarantees.
- Cancellation derived from the adaptor, not authored as a model capability.
- Default-true tool and structured-output declarations, explicit opt-outs, and
  dependency validation.
- Input modalities and media limits; currently supported text-only output.
- Catalog assumptions versus verified endpoint capabilities.

Include one minimal custom-model example and one managed-model override example.
Use reasoning-only variants inheriting shared settings to demonstrate why the
new schema removes repetition. Credentials use environment placeholders, never
real keys. Do not imply a backend honors effort merely because configuration
validation accepts it.

## Pruning Rules

Remove or consolidate:

- Repeated descriptions of the same behavior in different guides.
- Internal function names, type plumbing, and implementation narration that do
  not help the page's audience.
- Redundant examples demonstrating identical behavior.
- Unsupported promises, obsolete settings, and speculative future features.
- Excessive nesting and long caveat chains that obscure the main instruction.

Preserve:

- Security boundaries, permission effects, and execution consequences.
- Distinct examples that demonstrate meaningful precedence or inheritance.
- Operational limits that explain failures, truncation, or cost behavior.
- Breaking-change migration instructions and explicit unsupported behavior.
- Developer-facing implementation details that protect important invariants.

Do not optimize for deleted line counts. A shorter page is not automatically a
better page, and an accurate warning must not disappear for brevity.

## Example and Accuracy Verification

- Validate TOML/YAML syntax for examples where practical.
- Validate complete configuration examples through the actual loader/compiler
  using fixtures or isolated temporary configuration, without changing the
  user's config or making paid model requests.
- Clearly mark fragments and validate them within representative complete
  fixtures when possible.
- Check that agent examples expose the tools they use under deny-by-default
  and permission-based tool visibility.
- Verify model IDs and capability assumptions; use explicit placeholders when
  examples cannot depend on a stable catalog entry.
- Verify claims against runtime request generation, not merely accepted fields.
- Check links and anchors after consolidation; avoid gratuitous URL churn.
- Record source evidence for significant claims in review notes, not as noisy
  implementation citations throughout user-facing prose.

## Workflow

1. Inventory existing pages and navigation, classify reader tasks, and identify
   duplicate or obsolete sections.
2. Use the agreed navigation above; inventory actual top-level keys and map every
   current page to its destination before broad moves.
3. Have `story-writer` edit in coherent groups, using reviewed behavior and this
   pruning policy. Do not change application behavior to make prose true.
4. Independently review factual accuracy, example validity, and information lost
   through deletion. Send prose fixes back to the writer.
5. Run strict documentation build and relevant example validation. Resolve
   findings before committing or pushing.

The configuration implementation owns its necessary schema/contract updates.
Avoid concurrent ownership of the same documentation files; schedule the broad
rewrite after those edits settle, or assign explicit non-overlapping groups.

## Acceptance Criteria

- A reader can choose a provider, configure a model, adjust a variant, grant its
  tools, and run the agent without reconstructing instructions across references.
- Every changed behavioral claim matches the final implementation.
- Removed configuration names appear only where intentionally documenting
  migration or rejection; active examples use the new schema.
- No secrets or unsupported endpoint guarantees appear in examples.
- Canonical contracts remain complete after pruning.
- Navigation follows the five agreed groups. Every accepted top-level engine
  and TUI configuration key has one canonical page based on a schema inventory.
- Installation links to `/connect` guidance and custom-model configuration;
  Run covers all implemented run modes, and Sessions does not duplicate
  Compaction.
- Existing specifications are reachable under Specs with clear status labels.
- Strict MkDocs/rustdoc build and whitespace checks pass; changed examples are
  validated or their verification limitations are explicitly reported.
- Independent review passes before commit and push.

## Decisions for Discussion

1. Keep the existing page URLs and primarily rewrite content/navigation, or
   permit page consolidation and moves? Recommendation: preserve URLs where
   practical; justify moves by reader tasks.
2. Add automated example-validation fixtures now or validate this pass manually?
   Recommendation: automate a small representative set of complete configs;
   do not build a general Markdown execution framework.

No broad documentation rewrite is authorized by this draft alone.
