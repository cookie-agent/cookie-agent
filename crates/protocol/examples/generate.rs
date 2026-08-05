use std::path::PathBuf;

fn main() -> Result<(), cookie_agent_protocol::BindingExportError> {
    let mut arguments = std::env::args_os().skip(1);
    let root = match arguments.next() {
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("generated"),
        Some(flag) if flag == "--output" => {
            arguments.next().map(PathBuf::from).ok_or_else(|| {
                cookie_agent_protocol::BindingExportError::Arguments(
                    "--output requires exactly one directory".into(),
                )
            })?
        }
        Some(argument) => {
            return Err(cookie_agent_protocol::BindingExportError::Arguments(
                format!(
                    "unknown argument {}; use --output <directory>",
                    PathBuf::from(argument).display()
                ),
            ));
        }
    };
    if arguments.next().is_some() {
        return Err(cookie_agent_protocol::BindingExportError::Arguments(
            "only one --output <directory> pair is accepted".into(),
        ));
    }
    cookie_agent_protocol::export_json_schema_set(&root.join("json-schema"))?;
    cookie_agent_protocol::export_typescript_binding_set(&root.join("typescript"))?;
    Ok(())
}
