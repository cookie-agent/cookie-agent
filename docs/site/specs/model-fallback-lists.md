# Fallback lists for `default_model`, `small_model`, and `utility_model`

Status: **implemented** (2026-11-01, client protocol 1.69.0). The user's decisions
are in section 8.

## 1. Problem

`default_model`, `small_model`, and `utility_model` each hold one model and
an optional variant (`ConfiguredModel`, `crates/config/src/runtime.rs`). An
agent document's `models` is already a fallback chain, but a variable in it
expands to exactly one entry
(`AgentRegistry::resolve`, `crates/engine/src/runtime_snapshot/agents.rs`).
So a user who wants "Opus, else GPT-6, else DeepSeek" everywhere has to
write that chain into every agent document, and cannot change it in one
place.

## 2. Configuration

Each of the three fields takes one of:

```toml
default_model = "anthropic/claude-opus-5-5"                                  # one model, its default variant
default_model = { model = "anthropic/claude-opus-5-5", variant = "high" }    # one model and a variant
default_model = [                                                            # a fallback list
  { model = "anthropic/claude-opus-5-5", variant = "high" },
  "openai/gpt-6",
  "deepseek/deepseek-v4",
]
small_model   = ["${default_model}"]                                         # another field (D2)
utility_model = [{ model = "${small_model}", variant = "low" }, "deepseek/deepseek-flash"]
```

- The decoded type is a nonempty list of entries; an empty list is a
  configuration error. A one-entry list is written back in the single
  form, so existing files and onboarding's output do not change shape.
- An entry is a model key or one of `${default_model}`, `${small_model}`,
  `${utility_model}` (D2), each optionally with a `variant`.
  `${parent_model}` is refused here: it only means something inside an
  agent's chain.
- Config layers merge per field as now (the higher layer's value replaces
  the whole list); references are resolved after the merge.

## 3. Expansion

A field's value expands to an ordered list of model selections:

1. A model key entry is that model with its variant (the entry's, else the
   model's default variant).
2. A reference entry is the referenced field's expansion, in place, with
   the entry's `variant` applied by section 4.
3. An unset referenced field contributes nothing and is reported in the
   configuration diagnostics as now ("`small_model` is not set").
4. A model already in the list is dropped (first wins), as agent chains do
   today.

`${default_model}` in an agent document's `models` splices the expanded
list at that point, and the existing first-wins pass over the agent's chain
removes overlaps with the agent's other entries. Runs then walk the chain
as they do now (`fallback_chain`, `selected_suffix_start`), so falling back
through the list needs nothing new at run time. ACP agents keep skipping
`${...}` entries.

## 4. Variant overrides on a reference (D1)

An entry `{ model = "${X}", variant = "V" }` applies `V` to each model of
`X`'s expansion that

- has no variant assigned by its own entry (or by an inner reference's
  override), and
- has a variant named `V`.

Every other model keeps its configured variant, or its default one.
`variant = "base"` is a variant every model has, so it applies to every
model without an assigned variant. Whether a model has `V` depends on the
compiled catalog, so overrides are applied in the engine's resolver, not at
config load.

## 5. References between the fields (D2)

References are allowed and a **cycle** is a configuration error, with no
hop limit (the user first asked for a hop limit of 3):

- With three fields, the longest chain of references that has no cycle is
  two hops (`utility_model` → `small_model` → `default_model`), so a limit
  of 3 never applies; the only thing that can make expansion run away is
  a cycle.
- A cycle check is as little code as a hop counter (a stack of the fields
  being expanded, a few lines) and gives a better error:
  "`default_model` → `small_model` → `default_model`: model fields refer to
  each other in a cycle".
- The structural check (cycle, `${parent_model}`, empty list) runs at
  config load, so `config.apply` and `config.reload` refuse a cycle before
  anything is written or published.

The cost of D2 overall is small: a recursive expansion with that stack,
and a reference form in the onboarding inputs (section 6).

## 6. Onboarding (D3)

Onboarding can set each field to a fallback list, and the optional fields
to a reference.

### 6.1 Protocol (additive, client protocol 1.69.0)

- `OnboardingModelEntry`: `{ model, variant? }` or `{ field, variant? }`,
  where `field` is `default_model`, `small_model`, or `utility_model`.
- `OnboardingStatus` gains `default_model_list`, `small_model_list`, and
  `utility_model_list`: each field's entries as configured (references not
  expanded), absent when the field is unset. The existing
  `default_model`/`small_model`/`utility_model` keep reporting one model:
  the first model of the field's expansion, so older clients keep working.
- `OnboardingRunParams` gains the same three `*_list` fields as
  `Vec<OnboardingModelEntry>`. Giving both `default_model` and
  `default_model_list` is refused with `invalid_params`, as are an empty
  list and an entry without exactly one of `model` and `field`; a cycle is
  refused as `config.apply` refuses it, writing nothing.
- `OnboardingRequirement::DefaultModel` is missing when no model of
  `default_model`'s expansion is available (today: when its one model is
  not).

### 6.2 Clients

- **TUI model steps**: Enter (choose) sets the list to one model and moves
  on, as before; "add fallback" (`a`) appends the next model from the same
  picker and stays on the step; "remove last" (`x`) drops the last entry.
  The panel lists the chosen entries in fallback order beside what is
  configured.
- **Web model steps**: choosing a model adds it at the end of the list and
  choosing it again takes it out; each chosen row shows its place and its
  variant chips, and a line above the list reads the whole list in order.
- Neither client reorders entries in place: taking one out and adding it
  again moves it to the end.
- **`cookie onboard`**: `--default-model`, `--small-model`, and
  `--utility-model` may be repeated, in fallback order; a value is
  `provider/model[:variant]` or a field name (`default_model`), which is
  unambiguous because model keys always contain `/`. The interactive
  prompts ask for one model, then "Another fallback? (Enter to finish)".

## 7. Changes

- `cookie_agent_config`: the list type and its deserializer, the
  serializer's single form, the load-time structural checks, the
  `EngineConfig`/`RuntimeConfig` fields.
- Engine: `ModelVariables` expands recursively with section 4's override
  rule; `AgentRegistry::resolve` splices the expansion;
  `configured_selection` callers (onboarding's availability check, the
  test-only default agent) use the expansion.
- Protocol and schemas: section 6.1; a client protocol minor bump.
- Clients: section 6.2, and the `/config` editors in the TUI and web where
  they treat these fields as one model.
- Docs: `docs/site/extend/agents.md`, `docs/site/clients/cli.md`, and the
  configuration reference.

## 8. Decisions

- **D1** (user, 2026-11-01): a reference entry's `variant` applies only to
  models of the expansion that have no assigned variant and have a variant
  of that name.
- **D2** (user, 2026-11-01): the fields may refer to each other; a cycle
  is a configuration error and there is no hop limit (section 5).
- **D3** (user, 2026-11-01): onboarding can pick a fallback list.

## 9. Implementation notes

- A list naming the same model or field twice is refused when the file
  loads, as an agent's `models` refuses repeats compared as written.
- D1 also changes a single model: an entry such as
  `{ model: "${small_model}", variant: "max" }` used to replace the variant
  `small_model` configured; it now applies only when `small_model` gives
  its model no variant.
- An agent entry's `cache` beside a variable applies to each of the field's
  models; a shape for another provider family leaves that model without a
  structural cache strategy, as it would on a key of that model.
