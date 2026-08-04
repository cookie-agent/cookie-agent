use std::path::PathBuf;

fn main() -> Result<(), cookie_agent_protocol::BindingExportError> {
    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("generated"));
    cookie_agent_protocol::export_json_schema_set(&root.join("json-schema"))?;
    cookie_agent_protocol::export_typescript_binding_set(&root.join("typescript"))?;
    Ok(())
}
