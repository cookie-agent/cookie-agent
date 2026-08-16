# Usage and Cost

Cookie agent records normalized provider usage after every committed model
turn and completed internal-agent model request. Records include inclusive input and output token totals and cache read or
write counts when the provider reports them. Anthropic cache creation maps to
cache writes and cache reads map directly; OpenAI cached input maps to cache
reads. Providers that omit a field leave it unknown rather than estimating it.

The event log is the source of truth. Session rollups and their per-model rows
are rebuilt on restart and obey revert/fork visibility. Agent rollups use the
agent that owned each model turn, while the global rollup covers every session
in the current project.

Open `/usage` in the TUI to compare the selected session with global totals.
Programmatic clients can call `session.usage`, `agent.usage`, and
`usage.global`. Cache hit rate is cache-read tokens divided by inclusive input
tokens. It is unavailable unless every included request reports both fields;
an explicitly reported zero remains a 0% hit rate.

Cost is always optional. Managed models use the selected models.dev catalog
price tier for each request's reported input-token context size. Model-specific entries under `[pricing.models."provider/model"]` supply
prices for custom models or override catalog prices, as described in the
[configuration reference](../reference/configuration.md#pricingmodelsprovidermodel).
The precedence is config override, catalog, then no cost. An aggregate remains
unpriced when any used model lacks a required rate or when a provider omits a
usage split needed to apply a distinct cache or reasoning price.
