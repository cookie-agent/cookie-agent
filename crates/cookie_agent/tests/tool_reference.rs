//! Renders the generated regions of `docs/site/reference/tools.md` from the
//! built-in tool specs: the local coding runtime's tools, the engine's base
//! layer, and the `global` runtime's memory tools, `list_runtimes`,
//! `list_agents`, and cron tools.
//!
//! The page is written by hand around the regions. A region starts with a
//! `<!-- generated: KEY -->` line and ends with `<!-- /generated -->`: the key
//! `tool-table` holds the table of every tool and its permission, and the key
//! `tool NAME` shows that tool exactly as the engine publishes it to the model,
//! right under a heading naming it. Every tool has exactly one region.
//! Regenerate them after changing a tool's name, description, or parameters:
//! `COOKIE_UPDATE_TOOL_DOCS=1 cargo test -p cookie_agent --test tool_reference`.

use std::fmt::Write as _;

use cookie_agent_coding_runtime::{BuiltinTools, bash::bash_tool_description_for};
use cookie_agent_engine::{
    agents_tool_spec, base_layer_tool_specs, builtin_tool_groups, cron_tool_specs,
    memory_tool_specs, runtimes_tool_spec,
};
use cookie_agent_protocol::SessionId;
use cookie_agent_runtime_api::{SessionToolContext, ToolProvider, ToolSpec, WorkspaceContext};
use serde_json::Value;

const DOC_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/site/reference/tools.md"
);
const REGION_START: &str = "<!-- generated: ";
const REGION_END: &str = "<!-- /generated -->";
const UPDATE_ENV: &str = "COOKIE_UPDATE_TOOL_DOCS";
/// Stands in for the session-specific `agent_type` enum of `delegate_subagent`.
const DELEGATE_TARGET_PLACEHOLDER: &str = "<subagent id>";

/// Every built-in tool, in the order the reference lists them.
fn documented_specs() -> Vec<ToolSpec> {
    let workspace = WorkspaceContext::new(&std::env::temp_dir()).expect("workspace");
    let mut specs = BuiltinTools::new(&workspace)
        .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
        .expect("built-in tool specs");
    specs.extend(base_layer_tool_specs(DELEGATE_TARGET_PLACEHOLDER));
    specs.extend(memory_tool_specs());
    specs.push(runtimes_tool_spec());
    specs.push(agents_tool_spec());
    specs.extend(cron_tool_specs());
    specs
}

/// When the engine publishes a tool, beyond a visible permission.
fn availability(name: &str) -> Option<&'static str> {
    Some(match name {
        "skill" => "Only when at least one skill is visible to the session.",
        "tool_search" => {
            "Only when the request defers a tool: on a model that loads deferred tools, \
             when an MCP or plugin tool, or a tool of a deferred built-in group, is offered. \
             It stays until the session's next compaction."
        }
        "delegate_subagent" => {
            "Only when the agent can delegate to at least one subagent at this depth. The \
             `agent_type` enum lists those subagents; this page shows a placeholder. \
             `capabilities` exists only in a root session with the `global` runtime attached: \
             optional, as shown, when the root has other attachments too, and required when it \
             has only `global`. Each runtime it names is checked as `delegate@<runtime>`."
        }
        "get_subagent_result" => {
            "Only when the agent can delegate to at least one subagent at this depth."
        }
        "goal_get" | "goal_set" | "goal_update" => {
            "Only in a root session. An ACP agent's root session has them only while the \
             user's goal is active or paused when the run starts."
        }
        "memory_search" | "memory_read" => {
            "Only when the session has the `global` runtime attached and the agent or the \
             session overlay has an `allow` or `ask` rule for `memory`. `memory_search` needs \
             no rule of its own: each result is checked against the `memory` rules for its \
             URL instead."
        }
        "memory_write" | "memory_edit" | "memory_delete" => {
            "Only when `memory_read` is sent too and some memory URL is both readable (an \
             `allow` or `ask` `memory` rule matches it) and writable (an `allow` or `ask` \
             `memory_write` rule matches it). A delegated run whose agent document has no \
             memory keys gets its own agent's memory, `agent://<agent>`, by default."
        }
        "list_runtimes" => {
            "Only when the session has the `global` runtime attached and the agent or the \
             session overlay has an `allow` or `ask` rule for `list_runtimes`, which `primary` \
             and `all` agents have by default."
        }
        "list_agents" => {
            "Only when the session has the `global` runtime attached and the run is offered \
             `delegate_subagent` or a cron tool. It has no permission of its own."
        }
        "cron_create" | "cron_update" | "cron_delete" | "cron_run" => {
            "Only when the session has the `global` runtime attached and the agent or the \
             session overlay has an `allow` or `ask` rule for `cron`, which a root run of any \
             agent but an internal one has by default; a delegated run has none unless its \
             agent document grants it."
        }
        "cron_list" => {
            "Only next to the other cron tools. It needs no rule of its own: it lists the jobs \
             whose target kind the `cron` rules allow or ask for."
        }
        "read_artifact" => {
            "Always, whatever the agent's permission rules: it reads only output retained by \
             the session's own tree."
        }
        _ => return None,
    })
}

/// The table of every tool and its permission, linking to each tool's heading.
fn render_table(specs: &[ToolSpec]) -> String {
    let mut table = String::from("| Tool | Permission |\n|---|---|\n");
    for spec in specs {
        let permission = spec
            .permission_name
            .as_ref()
            .map_or_else(|| "*none*".to_owned(), |name| format!("`{name}`"));
        writeln!(table, "| [`{0}`](#{0}) | {permission} |", spec.name).unwrap();
    }
    table
}

/// The table of built-in tool groups: the name a group's deferred tools are
/// announced under, its tools, whether the deferral policy defers them, and
/// its summary.
fn render_groups() -> String {
    let mut groups = builtin_tool_groups(DELEGATE_TARGET_PLACEHOLDER);
    groups.push((
        cookie_agent_coding_runtime::BuiltinTools::GROUP,
        BuiltinTools::new(&WorkspaceContext::new(&std::env::temp_dir()).expect("workspace"))
            .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
            .expect("built-in tool specs")
            .into_iter()
            .map(|spec| spec.name)
            .collect(),
    ));
    groups.sort_by_key(|(group, _)| group.name);
    let mut table = String::from(
        "| Group | Tools | Deferred | Summary sent to the model |
|---|---|---|---|
",
    );
    for (group, tools) in groups {
        let tools = tools
            .iter()
            .map(|tool| format!("[`{tool}`](#{tool})"))
            .collect::<Vec<_>>()
            .join(", ");
        let deferred = if cookie_agent_config::DEFERRED_BUILTIN_GROUPS.contains(&group.name) {
            "yes"
        } else {
            "no"
        };
        writeln!(
            table,
            "| `{}` | {tools} | {deferred} | {} |",
            group.name, group.summary
        )
        .unwrap();
    }
    table
}

/// Replaces the body of every generated region of `page`, checking that each
/// tool has exactly one region, under a heading that names it.
fn splice(page: &str, specs: &[ToolSpec]) -> Result<String, String> {
    let mut spliced = String::with_capacity(page.len());
    let mut rest = page;
    let mut keys = Vec::new();
    while let Some(start) = rest.find(REGION_START) {
        let key_start = start + REGION_START.len();
        let key_end = rest[key_start..]
            .find(" -->")
            .map(|offset| key_start + offset)
            .ok_or("a region marker is not closed")?;
        let key = &rest[key_start..key_end];
        let body_start = key_end + " -->".len();
        let body_end = rest[body_start..]
            .find(REGION_END)
            .map(|offset| body_start + offset)
            .ok_or_else(|| format!("region `{key}` has no `{REGION_END}`"))?;
        let body = if key == "tool-table" {
            render_table(specs)
        } else if key == "tool-groups" {
            render_groups()
        } else if let Some(name) = key.strip_prefix("tool ") {
            let spec = specs
                .iter()
                .find(|spec| spec.name == name)
                .ok_or_else(|| format!("region `{key}` names no built-in tool"))?;
            let heading = rest[..start].trim_end().rsplit('\n').next().unwrap_or("");
            if !(heading.starts_with('#') && heading.ends_with(&format!("`{name}`"))) {
                return Err(format!(
                    "region `{key}` must follow a heading naming `{name}`"
                ));
            }
            let mut body = String::new();
            render_tool(&mut body, spec);
            body
        } else {
            return Err(format!("unknown region `{key}`"));
        };
        spliced.push_str(&rest[..body_start]);
        spliced.push_str("\n\n");
        spliced.push_str(body.trim());
        spliced.push_str("\n\n");
        spliced.push_str(REGION_END);
        rest = &rest[body_end + REGION_END.len()..];
        keys.push(key.to_owned());
    }
    spliced.push_str(rest);
    let expected = ["tool-table".to_owned(), "tool-groups".to_owned()]
        .into_iter()
        .chain(specs.iter().map(|spec| format!("tool {}", spec.name)));
    for key in expected {
        let count = keys.iter().filter(|seen| **seen == key).count();
        if count != 1 {
            return Err(format!("region `{key}` appears {count} times, not once"));
        }
    }
    Ok(spliced)
}

fn render_tool(page: &mut String, spec: &ToolSpec) {
    let schema = &spec.parameters;
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();

    let signature = properties
        .iter()
        .map(|(name, property)| {
            let optional = if required.contains(&name.as_str()) {
                ""
            } else {
                "?"
            };
            format!("{name}{optional}: {}", type_label(schema, property))
        })
        .collect::<Vec<_>>()
        .join(", ");
    write!(page, "```text\n{}({signature})\n```\n", spec.name).unwrap();
    // The page must render identically on every platform, so tools whose
    // description depends on the host list each variant.
    if spec.name == "bash" {
        for (platform, description) in [
            ("Linux and macOS", bash_tool_description_for(false)),
            (
                "Windows, where commands run in Git Bash",
                bash_tool_description_for(true),
            ),
        ] {
            write!(
                page,
                "\nDescription sent to the model on {platform}:\n\n```text\n{description}\n```\n"
            )
            .unwrap();
        }
    } else {
        write!(
            page,
            "\nDescription sent to the model:\n\n```text\n{}\n```\n",
            spec.description
        )
        .unwrap();
    }
    if let Some(note) = availability(&spec.name) {
        write!(page, "\n{note}\n").unwrap();
    }
    if !properties.is_empty() {
        page.push_str("\n| Parameter | Type | Required | Description |\n|---|---|---|---|\n");
        for (name, property) in &properties {
            let resolved = resolve(schema, property);
            let mut description = resolved
                .get("description")
                .or_else(|| property.get("description"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if let Some(default) = property.get("default").or_else(|| resolved.get("default")) {
                if !description.is_empty() {
                    description.push(' ');
                }
                write!(description, "Default: `{default}`.").unwrap();
            }
            writeln!(
                page,
                "| `{name}` | `{}` | {} | {} |",
                type_label(schema, property).replace('|', "\\|"),
                if required.contains(&name.as_str()) {
                    "yes"
                } else {
                    "no"
                },
                description.replace('|', "\\|").replace('\n', " "),
            )
            .unwrap();
        }
    }
    let json = serde_json::to_string_pretty(schema).expect("schema serializes");
    page.push_str("\n??? note \"Parameters JSON Schema\"\n\n    ```json\n");
    for line in json.lines() {
        writeln!(page, "    {line}").unwrap();
    }
    page.push_str("    ```\n");
}

/// Follows a local `$ref` into `$defs` or `definitions`.
fn resolve<'a>(root: &'a Value, property: &'a Value) -> &'a Value {
    property
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
        .unwrap_or(property)
}

fn type_label(root: &Value, property: &Value) -> String {
    let property = resolve(root, property);
    if let Some(values) = property.get("enum").and_then(Value::as_array) {
        return values
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = property.get(key).and_then(Value::as_array) {
            let labels = variants
                .iter()
                .map(|variant| type_label(root, variant))
                .filter(|label| label != "null")
                .collect::<Vec<_>>();
            return labels.join(" | ");
        }
    }
    let types = match property.get("type") {
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .filter(|name| *name != "null")
            .collect(),
        _ => return "any".into(),
    };
    types
        .into_iter()
        .map(|name| match name {
            "array" => format!(
                "{}[]",
                property
                    .get("items")
                    .map_or_else(|| "any".into(), |items| type_label(root, items))
            ),
            other => other.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

#[test]
fn tool_reference_page_matches_published_specs() {
    let current = std::fs::read_to_string(DOC_PATH).expect("read the tool reference");
    let rendered = splice(&current, &documented_specs())
        .unwrap_or_else(|error| panic!("docs/site/reference/tools.md: {error}"));
    if std::env::var_os(UPDATE_ENV).is_some() {
        std::fs::write(DOC_PATH, &rendered).expect("write tool reference");
        return;
    }
    assert!(
        current == rendered,
        "docs/site/reference/tools.md is stale; regenerate with \
         {UPDATE_ENV}=1 cargo test -p cookie_agent --test tool_reference"
    );
}
