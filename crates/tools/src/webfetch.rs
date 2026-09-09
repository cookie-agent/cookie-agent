use std::time::Duration;

use async_trait::async_trait;
use cookie_agent_engine::{
    PreparedExecutor, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};
use cookie_agent_protocol::{
    ApprovalResourceSource, PermissionAction, PersistedToolResult, PreparedBindingLifetime,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{parse_args, prepared_operation, prepared_resource, safe_title, schema, tool_error};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_CAP: usize = 16 * 1024 * 1024;

#[derive(Debug, Default)]
pub struct WebfetchTool;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebfetchArgs {
    url: String,
    #[serde(default)]
    raw: bool,
}

impl WebfetchArgs {
    fn validate(&self) -> Result<(), ToolError> {
        if !self.url.starts_with("http://") && !self.url.starts_with("https://") {
            return Err(ToolError::InvalidUrl(
                "URL must start with http:// or https://".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ToolProvider for WebfetchTool {
    fn provider_id(&self) -> &'static str {
        "builtin.webfetch"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
output: Default::default(),
            name: "webfetch".into(),
            permission_name: "webfetch".into(),
            description: "Fetch an HTTP or HTTPS URL. Output has final_url, status_code, content_type, and truncated header lines, a blank line, then the text body. Returns HTML as plaintext unless raw is true; other text passes through. Use read with the artifact URI in a truncation hint to page retained output, including the body.".into(),
            parameters: schema::<WebfetchArgs>(),
            concurrency: cookie_agent_engine::ToolConcurrency::Parallel,
            result_truncation: Default::default(),
        }])
    }

    fn get_permission_name(name: &str) -> Result<&'static str, ToolError> {
        if name != "webfetch" {
            return Err(tool_error("webfetch provider received another tool"));
        }
        Ok("webfetch")
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
        let permission = Self::get_permission_name(name)?;
        let args: WebfetchArgs = parse_args(name, arguments.clone())?;
        args.validate()?;
        Ok((permission, Some(args.url)))
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        let (_, resource) = self.get_permission_resource(name, arguments)?;
        Ok(resource.expect("webfetch has a URL resource"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Self::get_permission_name(&call.name)?;
        let args: WebfetchArgs = parse_args(&call.name, call.arguments)?;
        args.validate()?;
        let resource = prepared_resource(
            PermissionAction::Webfetch,
            "url",
            args.url.as_bytes(),
            args.url.as_bytes(),
            PreparedBindingLifetime::RestartStable,
            ApprovalResourceSource::PrimaryOperation,
        )?;
        let operation = prepared_operation(
            "webfetch",
            &args,
            vec![(PermissionAction::Webfetch, "get")],
            vec![resource],
            b"webfetch:v1",
        )?;
        let normalized = serde_json::to_value(&args).map_err(tool_error)?;
        let label = args.url.clone();
        PreparedTool::new(operation, normalized, None, Box::new(args))?
            .with_policy_labels(vec![label])
    }
}

fn request_error(error: reqwest::Error) -> ToolError {
    if error.is_timeout() {
        ToolError::Timeout(error.to_string())
    } else if error.is_redirect() {
        ToolError::RedirectError(error.to_string())
    } else if error.is_builder() {
        ToolError::InvalidUrl(error.to_string())
    } else {
        ToolError::TransportError(error.to_string())
    }
}

impl WebfetchArgs {
    async fn fetch(&self) -> Result<PersistedToolResult, ToolError> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ToolError::TransportError(error.to_string()))?;
        let mut response = client.get(&self.url).send().await.map_err(request_error)?;
        let final_url = response.url().to_string();
        let status_code = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let mime = content_type
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let html = matches!(mime.as_str(), "text/html" | "application/xhtml+xml");
        if !(mime.starts_with("text/")
            || matches!(
                mime.as_str(),
                "application/json"
                    | "application/xml"
                    | "application/javascript"
                    | "application/x-www-form-urlencoded"
            )
            || mime.ends_with("+json")
            || mime.ends_with("+xml"))
        {
            return Err(ToolError::UnsupportedContentType(content_type));
        }
        let mut body = Vec::new();
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await.map_err(request_error)? {
            let remaining = DOWNLOAD_CAP - body.len();
            body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            if chunk.len() > remaining {
                truncated = true;
                break;
            }
        }
        let text = if html && !self.raw {
            html2text::from_read(body.as_slice(), 80).map_err(tool_error)?
        } else {
            String::from_utf8_lossy(&body).into_owned()
        };
        let metadata = serde_json::json!({
            "url": self.url,
            "final_url": final_url,
            "status_code": status_code,
            "content_type": content_type,
            "truncated": truncated,
        });
        Ok(PersistedToolResult {
            display: None,
            retained_output: None,
            title: safe_title(&self.url),
            output: format!(
                "final_url: {final_url}\nstatus_code: {status_code}\ncontent_type: {content_type}\ntruncated: {truncated}\n\n{text}"
            ),
            metadata,
            truncation: None,
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        })
    }
}

#[async_trait]
impl PreparedExecutor for WebfetchArgs {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<cookie_agent_engine::ToolCompletion, ToolError> {
        let result: Result<PersistedToolResult, ToolError> =
            async move { self.fetch().await }.await;
        result.map(cookie_agent_engine::ToolCompletion::single)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, BufRead, BufReader, Read, Write},
        net::{SocketAddr, TcpListener, TcpStream},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
    };

    use cookie_agent_protocol::{PermissionEffect, RunId, SessionId, ToolCallId};

    use super::*;

    struct HttpFixture {
        address: SocketAddr,
        stop: Arc<AtomicBool>,
        task: Option<JoinHandle<io::Result<()>>>,
    }

    impl HttpFixture {
        fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let task_stop = Arc::clone(&stop);
            let task = thread::spawn(move || {
                while !task_stop.load(Ordering::Acquire) {
                    let (stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    if task_stop.load(Ordering::Acquire) {
                        break;
                    }
                    // Early disconnects are normal when webfetch rejects or caps a body.
                    let _ = serve_connection(stream);
                }
                Ok(())
            });
            Self {
                address,
                stop,
                task: Some(task),
            }
        }

        fn url(&self) -> String {
            format!("http://{}", self.address)
        }
    }

    impl Drop for HttpFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            // Wake accept even if shutdown races with the loop's stop check.
            let _ = TcpStream::connect(self.address);
            if let Some(task) = self.task.take() {
                task.join()
                    .expect("join HTTP fixture")
                    .expect("HTTP fixture accept");
            }
        }
    }

    fn serve_connection(mut stream: TcpStream) -> io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new((&mut stream).take(16 * 1024));
        let mut request = String::new();
        reader.read_line(&mut request)?;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header)? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            if header == "\r\n" {
                break;
            }
        }
        let Some(path) = request.split_whitespace().nth(1) else {
            return Ok(());
        };
        let (status, headers, body) = match path {
            "/first" => ("302 Found", "Location: /second\r\n", Vec::new()),
            "/second" => ("301 Moved Permanently", "Location: /html\r\n", Vec::new()),
            "/loop" => ("302 Found", "Location: /loop\r\n", Vec::new()),
            "/html" => (
                "200 OK",
                "Content-Type: text/html; charset=utf-8\r\n",
                b"<h1>Hello</h1><p>World <b>wide</b> web.</p>".to_vec(),
            ),
            "/cap" => (
                "200 OK",
                "Content-Type: text/plain\r\n",
                vec![b'x'; DOWNLOAD_CAP + 1],
            ),
            "/exact" => (
                "200 OK",
                "Content-Type: text/plain\r\n",
                vec![b'x'; DOWNLOAD_CAP],
            ),
            "/binary" => ("200 OK", "Content-Type: image/png\r\n", vec![0, 255]),
            "/lines" => (
                "200 OK",
                "Content-Type: text/plain\r\n",
                b"first\nsecond\r\nlast".to_vec(),
            ),
            _ => (
                "404 Not Found",
                "Content-Type: application/json\r\n",
                b"{\"error\":\"missing\"}".to_vec(),
            ),
        };
        write!(
            stream,
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )?;
        stream.write_all(&body)
    }

    async fn fetch(url: String, raw: bool) -> Result<PersistedToolResult, ToolError> {
        let result = WebfetchArgs {
            url: url.clone(),
            raw,
        }
        .fetch()
        .await?;
        assert_eq!(result.metadata["url"], url);
        let header = format!(
            "final_url: {}\nstatus_code: {}\ncontent_type: {}\ntruncated: {}\n\n",
            result.metadata["final_url"].as_str().unwrap(),
            result.metadata["status_code"].as_u64().unwrap(),
            result.metadata["content_type"].as_str().unwrap(),
            result.metadata["truncated"].as_bool().unwrap(),
        );
        assert!(result.output.starts_with(&header));
        assert_eq!(result.metadata.as_object().unwrap().len(), 5);
        assert!(result.truncation.is_none());
        Ok(result)
    }

    fn body(result: &PersistedToolResult) -> &str {
        result.output.split_once("\n\n").unwrap().1
    }

    #[test]
    fn fixture_shutdown_wakes_blocking_accept() {
        drop(HttpFixture::start());
    }

    #[tokio::test]
    async fn fixture_keeps_serving_after_dropped_connections() {
        let server = HttpFixture::start();
        for request in ["", "GET /html HTTP/1.1\r\nHost: incomplete"] {
            let mut stream = TcpStream::connect(server.address).unwrap();
            stream.write_all(request.as_bytes()).unwrap();
        }
        let result = fetch(format!("{}/lines", server.url()), false)
            .await
            .unwrap();
        assert_eq!(body(&result), "first\nsecond\r\nlast");
    }

    #[test]
    fn webfetch_keeps_standard_bounded_retention() {
        let spec = WebfetchTool
            .tools_for_session(&SessionToolContext::new(SessionId::new_v7()))
            .unwrap()
            .remove(0);
        assert_eq!(
            spec.result_truncation,
            cookie_agent_engine::ToolResultTruncationPolicy::Bounded
        );
    }

    #[tokio::test]
    async fn redirects_preserve_initial_url_and_record_final_url() {
        let server = HttpFixture::start();
        let base = server.url();
        let result = fetch(format!("{base}/first"), false).await.unwrap();
        assert_eq!(result.metadata["url"], format!("{base}/first"));
        assert_eq!(result.metadata["final_url"], format!("{base}/html"));
        assert_eq!(result.metadata["status_code"], 200);
        assert_eq!(result.metadata["content_type"], "text/html; charset=utf-8");
        assert_eq!(result.metadata["truncated"], false);
        let error = fetch(format!("{base}/loop"), false).await.unwrap_err();
        assert!(matches!(error, ToolError::RedirectError(_)), "{error}");
        let result = fetch(format!("{base}/html"), true).await.unwrap();
        assert_eq!(result.metadata["status_code"], 200);
    }

    #[tokio::test]
    async fn html_raw_and_plaintext_and_http_errors() {
        let server = HttpFixture::start();
        let base = server.url();
        let rendered = fetch(format!("{base}/html"), false).await.unwrap();
        let text = body(&rendered);
        assert!(text.contains("Hello") && text.contains("World"));
        assert!(!text.contains("<h1>") && !text.contains("<b>"));
        let raw = fetch(format!("{base}/html"), true).await.unwrap();
        assert_eq!(body(&raw), "<h1>Hello</h1><p>World <b>wide</b> web.</p>");
        for raw in [false, true] {
            let json = fetch(format!("{base}/missing?q=1"), raw).await.unwrap();
            assert_eq!(json.metadata["status_code"], 404);
            assert_eq!(body(&json), "{\"error\":\"missing\"}");
            let lines = fetch(format!("{base}/lines"), raw).await.unwrap();
            assert_eq!(body(&lines), "first\nsecond\r\nlast");
            assert_eq!(lines.output.lines().count(), 8);
            assert!(
                matches!(fetch(format!("{base}/binary"), raw).await, Err(ToolError::UnsupportedContentType(content_type)) if content_type == "image/png")
            );
        }
    }

    #[tokio::test]
    async fn download_cap_returns_prefix_and_only_truncates_over_cap() {
        let server = HttpFixture::start();
        let base = server.url();
        for (path, truncated) in [("cap", true), ("exact", false)] {
            let result = fetch(format!("{base}/{path}"), false).await.unwrap();
            assert_eq!(result.metadata["truncated"], truncated);
            assert_eq!(body(&result), "x".repeat(DOWNLOAD_CAP));
        }
    }

    async fn prepare(arguments: serde_json::Value) -> Result<PreparedTool, ToolError> {
        WebfetchTool
            .prepare(
                ToolPreparationContext {
                    session: SessionId::new_v7(),
                    run: RunId::new_v7(),
                    cwd: "/tmp".into(),
                    workspace_root: "/tmp".into(),
                    turn_context: crate::test_turn_context(),
                },
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "webfetch".into(),
                    arguments,
                },
            )
            .await
    }

    #[tokio::test]
    async fn validates_scheme_and_strict_arguments_without_network() {
        for url in [
            "file:///tmp/a",
            "ftp://example.org",
            "HTTPS://example.org",
            " example.org",
        ] {
            assert!(matches!(
                prepare(serde_json::json!({"url": url})).await,
                Err(ToolError::InvalidUrl(_))
            ));
        }
        for args in [
            serde_json::json!({}),
            serde_json::json!({"url":"https://example.org", "raw":"true"}),
            serde_json::json!({"url":"https://example.org", "limit":1}),
        ] {
            assert!(prepare(args).await.is_err());
        }
        let prepared = prepare(serde_json::json!({"url":"http://"})).await.unwrap();
        assert_eq!(prepared.normalized_arguments()["raw"], false);
        assert!(matches!(
            fetch("http://".into(), false).await,
            Err(ToolError::InvalidUrl(_))
        ));
    }

    #[tokio::test]
    async fn one_initial_url_resource_includes_query_and_defaults_to_deny() {
        use cookie_agent_protocol::{
            AgentDocumentSource, AgentId, AgentMode, AgentSchemaVersion, AgentSnapshot,
            PermissionRule, Sha256Digest, WildcardPattern,
        };
        let server = HttpFixture::start();
        let url = format!("{}/html?token=given", server.url());
        let prepared = prepare(serde_json::json!({"url":url})).await.unwrap();
        assert_eq!(prepared.operation().resources().len(), 1);
        assert_eq!(prepared.policy_labels(), [Some(url.clone())]);
        assert_eq!(
            WebfetchTool
                .get_display_argument("webfetch", prepared.normalized_arguments())
                .unwrap(),
            url
        );
        let mut policy = AgentSnapshot {
            agent: AgentId::new("test").unwrap(),
            schema: AgentSchemaVersion::current(),
            mode: AgentMode::Primary,
            description: "Test".into(),
            document_source: AgentDocumentSource::Workspace,
            document_fingerprint: Sha256Digest::of_bytes(b"test"),
            composed_prompt: "test".into(),
            prompt_fingerprint: Sha256Digest::of_bytes(b"test"),
            max_output_tokens: 0,
            permissions: Vec::new(),
            delegation: None,
            fallback_chain: Vec::new(),
            selected_suffix_start: 0,
        };
        let pipeline = cookie_agent_engine::permissions::PermissionPipeline::default();
        let decide = |policy: &AgentSnapshot| {
            pipeline.decide_operation(
                policy,
                prepared.operation(),
                prepared.policy_labels(),
                std::path::Path::new("/tmp"),
            )
        };
        assert_eq!(decide(&policy).effect, PermissionEffect::Deny);
        policy.permissions.push(PermissionRule {
            action: PermissionAction::Webfetch,
            resource: WildcardPattern::new(url.split('?').next().unwrap()).unwrap(),
            effect: PermissionEffect::Allow,
        });
        assert_eq!(decide(&policy).effect, PermissionEffect::Deny);
        policy.permissions[0].resource = WildcardPattern::new(&url).unwrap();
        for effect in [
            PermissionEffect::Allow,
            PermissionEffect::Ask,
            PermissionEffect::Deny,
        ] {
            policy.permissions[0].effect = effect;
            let decision = decide(&policy);
            assert_eq!(decision.effect, effect);
            assert_eq!(decision.evaluations.len(), 1);
            assert_eq!(decision.evaluations[0].trace.normalized_resource, url);
        }
    }
}
