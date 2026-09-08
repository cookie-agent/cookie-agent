# Documentation Reorganization

Documentation is published at <https://cookie-agent.github.io/doc/>. The
development version is at <https://cookie-agent.github.io/doc/dev/>; versioned
release documentation remains in the same site.

## Navigation and canonical ownership

The five navigation groups are Guide, Engine Configuration, TUI Configuration,
Development, and Specs. Engine settings have one page per authored top-level
key; TUI settings have separate pages for `minimum_event_level` and `theme`.
Runtime-only pricing is not an authored top-level key: prices belong to models
on the provider page.

Provider definitions, models, variants, generation/adaptor options, cache policy,
and model-local pricing share one canonical page. The Agent page owns permission
matching, visibility, precedence, approval modes, and overlays. Exact tool and
media contracts remain under Specs.

## Page migration map

Paths in this table are relative to `docs/site/`.

| Previous source | Destination and disposition |
|---|---|
| `index.md` | Retained root index; no sixth navigation group |
| `install.md` | Guide / Installation; run details moved to Run |
| `guide/tui.md` | Consolidated into `guide/run.md`; theme settings moved to `tui/theme.md` |
| `guide/headless.md` | Consolidated into `guide/run.md#headless-runs` |
| `guide/sessions.md` | Retained; resumption added, compaction links to its canonical guide |
| `guide/compaction.md` | Retained workflow; settings moved to `engine/context_compaction.md` |
| `guide/permissions.md` | Consolidated into `guide/agents.md#permissions` |
| `guide/configuration.md` | Canonical locations, layering, strictness, interpolation, and key index |
| `guide/providers.md` | Canonical provider, model, variant, wire-ID, replay, cache, and pricing reference |
| `guide/mcp.md`, `guide/plugins.md` | Canonical settings and lifecycle contracts |
| `guide/security.md` | Retained URL, published under Specs |
| `reference/configuration.md` | Split among configuration, Agent, provider, MCP, plugin, engine-key, and TUI pages |
| Reference engine settings | `engine/{server,tool_output,agent_md,approval,model_retry,context_compaction,session_title,delegation,headers}.md` |
| Reference TUI settings | `tui/configuration.md`, `tui/minimum_event_level.md`, `tui/theme.md` |
| `architecture.md` | Retained URL, under Development |
| `reference/api.md`, generated `api/` | Retained under Development / Rust API |
| `development/goal-mode.md` | Retained URL and approved design under Specs |
| Protocol, events, schemas, system prompt, and tools references | Retained URLs under Specs |

The four retired source pages were consolidated by reader task. Internal links
use their destinations; no redirect plugin or duplicate reference pages were
added. The canonical `docs/model-configuration-spec.md` and
`docs/documentation-overhaul-spec.md` are included at build time by their Specs
pages, not copied into separate authored snapshots. The Specs index distinguishes
historical draft text from current user configuration and implemented contracts.

## Publishing

`.github/workflows/docs.yml` builds locked workspace rustdoc with warnings denied
and MkDocs in strict mode. Eligible main-branch pushes and manual runs publish
`dev`; published release events publish the release version and update `latest`.

The workflow fetches the existing `cookie-agent/doc` version tree and publishes
with `mike --remote docs --push`. Cross-repository writes use the organization
secret `DOCS_DEPLOY_TOKEN`, with Contents write access to `cookie-agent/doc`.
The source repository's `GITHUB_TOKEN` does not provide that access. GitHub Pages
deploys the destination repository's `gh-pages` branch.

Generated `site/` and `docs/site/api/` outputs are excluded from source commits.
Retirement of the source repository's old `gh-pages` branch is a separate
operation after successful publication has been confirmed.
