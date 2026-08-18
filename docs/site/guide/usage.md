# Usage and Cost

Cookie agent records normalized provider usage after every committed model
turn and completed internal-agent model request. Records include inclusive input and output token totals and cache read or
write counts when the provider reports them. Anthropic cache creation maps to
cache writes and cache reads map directly; OpenAI cached input maps to cache
reads. Providers that omit a field leave it unknown rather than estimating it.

The event log is the source of truth. Session rollups and their per-model rows
are rebuilt on restart and obey revert/fork visibility. Agent rollups use the
agent that owned each model turn. Session-tree rollups combine the selected
session with all of its delegated descendants, including descendants that have
been evicted from memory.

Open `/usage` in the TUI to compare the selected session with its session-tree
totals. Programmatic clients can call `session.usage`, `session.tree_usage`,
`agent.usage`, and `usage.global`. Cache hit rate is cache-read tokens divided
by inclusive input tokens. It is unavailable unless every included request
reports both fields; an explicitly reported zero remains a 0% hit rate.

Each TUI section shows formatted totals and a per-model table. Models are
sorted by descending estimated cost, with unpriced models last, then by input
tokens and model name. Token counts use thousands separators, cache hit rates
use one decimal place, and costs use two decimal places at or above one cent
and four below it. Use the arrow keys, Page Up, Page Down, or the mouse wheel
when the panel content exceeds its viewport.

When a request is priced, the assistant block's closing footer includes its
estimated cost after generation speed and context usage. The bottom bar shows
the selected session total; clicking it opens `/usage`. Unpriced costs are
omitted from both locations.

Cost is always optional. Managed models use the selected models.dev catalog
price tier for each request's reported input-token context size. Model-specific entries under `[pricing.models."provider/model"]` supply
prices for custom models or override catalog prices, as described in the
[configuration reference](../reference/configuration.md#pricingmodelsprovidermodel).
The precedence is config override, catalog, then no cost. An aggregate remains
unpriced when any used model lacks a required rate or when a provider omits a
usage split needed to apply a distinct cache or reasoning price.

New usage events stamp the request price selected when the request completes.
Footers and rollups preserve those stamps, so later pricing changes do not
rewrite already stamped costs. An explicit unpriced stamp also remains
unpriced after configuration or catalog changes. Legacy events without the
field are priced from
the current configuration or catalog when a rollup is requested; mixed
rollups therefore combine durable historical prices with best-effort current
pricing for legacy records.

To evaluate prompt caching, compare equivalent workloads before and after
enabling `[prompt_caching]`. Track `cache_hit_rate` after the first turn and
`estimated_cost_usd` over the whole session. A healthy multi-turn workload
normally reports cache writes on the first request and cache reads on later
requests; compare totals only after enough repeated-prefix turns to amortize the
initial write. Use `enabled = false` as the baseline, where the strategy emits no
cache markers and provider-reported cache reads and writes should remain zero.
