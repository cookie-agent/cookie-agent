//! `cookie`'s arguments: every subcommand and flag, and where each one
//! takes its workspace and daemon from.

use super::*;

#[test]
fn error_details_is_a_flag_of_every_command() {
    assert!(
        !Cli::try_parse_from(["cookie", "config", "status"])
            .unwrap()
            .error_details
    );
    for arguments in [
        &["cookie", "--error-details", "config", "status"][..],
        &["cookie", "config", "status", "--error-details"][..],
    ] {
        assert!(
            Cli::try_parse_from(arguments).unwrap().error_details,
            "{arguments:?}"
        );
    }
}

#[test]
fn cli_commands_and_removed_flags_are_exact() {
    assert_eq!(Cli::try_parse_from(["cookie"]).unwrap().command, None);
    assert!(!Cli::try_parse_from(["cookie"]).unwrap().standalone);
    let standalone = Cli::try_parse_from(["cookie", "--standalone"]).unwrap();
    assert!(standalone.standalone);
    assert_eq!(standalone.command, None);
    // Only plain `cookie` takes it; `main_result` refuses it before a
    // subcommand.
    assert!(Cli::try_parse_from(["cookie", "run", "--standalone", "hello"]).is_err());
    let daemon = |port| Command::Daemon {
        command: None,
        port,
        listen: None,
        tls_cert: None,
        tls_key: None,
    };
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon"]).unwrap().command,
        Some(daemon(None))
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon", "--port", "0"])
            .unwrap()
            .command,
        Some(daemon(Some(0)))
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "daemon",
            "--listen",
            "0.0.0.0:7419",
            "--tls-cert",
            "cert.pem",
            "--tls-key",
            "key.pem"
        ])
        .unwrap()
        .command,
        Some(Command::Daemon {
            command: None,
            port: None,
            listen: Some("0.0.0.0:7419".parse().unwrap()),
            tls_cert: Some("cert.pem".into()),
            tls_key: Some("key.pem".into()),
        })
    );
    let restart = |now, url: Option<&str>, token: Option<&str>| Command::Daemon {
        command: Some(daemon_restart::DaemonCommand::Restart {
            now,
            url: url.map(str::to_owned),
            token: token.map(str::to_owned),
        }),
        port: None,
        listen: None,
        tls_cert: None,
        tls_key: None,
    };
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon", "restart"])
            .unwrap()
            .command,
        Some(restart(false, None, None))
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "daemon",
            "restart",
            "--now",
            "--url",
            "ws://127.0.0.1:7419/ws",
            "--token",
            "secret"
        ])
        .unwrap()
        .command,
        Some(restart(
            true,
            Some("ws://127.0.0.1:7419/ws"),
            Some("secret")
        ))
    );
    // Serving options belong to the daemon itself, not to its restart.
    assert!(Cli::try_parse_from(["cookie", "daemon", "--port", "0", "restart"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "daemon", "restart", "--port", "0"]).is_err());
    assert_eq!(
        Cli::try_parse_from(["cookie", "sessions", "upgrade"])
            .unwrap()
            .command,
        Some(Command::Sessions {
            command: SessionsCommand::Upgrade { all: false },
        })
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "sessions", "upgrade", "--all"])
            .unwrap()
            .command,
        Some(Command::Sessions {
            command: SessionsCommand::Upgrade { all: true },
        })
    );
    for arguments in [
        vec!["cookie", "sessions"],
        vec![
            "cookie",
            "daemon",
            "--port",
            "1",
            "--listen",
            "0.0.0.0:7419",
        ],
        vec!["cookie", "daemon", "--tls-cert", "cert.pem"],
        vec!["cookie", "daemon", "--tls-key", "key.pem"],
        vec!["cookie", "daemon", "--listen", "not-an-address"],
    ] {
        assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
    }
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "runtime",
            "serve",
            "--stdio",
            "--root",
            "~/my repo",
            "--root",
            "/srv"
        ])
        .unwrap()
        .command,
        Some(Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: true,
                daemon: None,
                id: None,
                token: None,
                roots: vec!["~/my repo".into(), "/srv".into()],
            }
        })
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "runtime",
            "serve",
            "--daemon",
            "wss://daemon.example.com:7419/ws",
            "--id",
            "labpc"
        ])
        .unwrap()
        .command,
        Some(Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: false,
                daemon: Some(Some("wss://daemon.example.com:7419/ws".into())),
                id: Some("labpc".into()),
                token: None,
                roots: Vec::new(),
            }
        })
    );
    // `--daemon` alone dials in to the configured daemon as this host.
    assert_eq!(
        Cli::try_parse_from(["cookie", "runtime", "serve", "--daemon"])
            .unwrap()
            .command,
        Some(Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: false,
                daemon: Some(None),
                id: None,
                token: None,
                roots: Vec::new(),
            }
        })
    );
    for arguments in [
        vec!["cookie", "runtime", "serve"],
        vec!["cookie", "runtime", "serve", "--root", "/srv"],
        vec!["cookie", "runtime", "serve", "--stdio", "--id", "labpc"],
        vec![
            "cookie",
            "runtime",
            "serve",
            "--stdio",
            "--daemon",
            "ws://127.0.0.1:7419/ws",
            "--id",
            "labpc",
        ],
        vec!["cookie", "runtime", "coding", "--stdio"],
        vec![
            "cookie",
            "runtime",
            "serve",
            "--stdio",
            "--workspace",
            "/srv",
        ],
    ] {
        assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
    }
    assert!(Cli::try_parse_from(["cookie", "--trust-workspace", "daemon"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "connect", "openai", "--api-key", "sentinel"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "mcp", "approve", "github"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "mcp", "reject", "github"]).is_err());
    assert_eq!(
        Cli::try_parse_from(["cookie", "mcp", "--token", TEST_TOKEN, "auth", "remote"])
            .unwrap()
            .command,
        Some(Command::Mcp {
            command: McpCommand::Auth {
                server: "remote".into()
            },
            url: None,
            token: Some(TEST_TOKEN.into()),
        })
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "run",
            "check the build",
            "--permission-mode",
            "ask",
            "--allowed-tools",
            "read,bash",
            "--max-turns",
            "7",
            "--timeout",
            "30",
            "--output",
            "json",
            "--agent",
            "primary",
            "--preset",
            "python",
            "--model",
            "custom.local/test",
            "--variant",
            "base",
            "--output-file",
            "result.jsonl",
            "--verbose",
        ])
        .unwrap()
        .command,
        Some(Command::Run {
            args: Box::new(run::RunArgs {
                positional_prompt: Some("check the build".into()),
                prompt: None,
                prompt_file: None,
                agent: Some("primary".parse().unwrap()),
                preset: Some("python".into()),
                model: Some("custom.local/test".parse().unwrap()),
                variant: Some("base".into()),
                permission_mode: run::PermissionModeArg::Ask,
                allowed_tools: vec![
                    run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::READ),
                    run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::BASH)
                ],
                skill: None,
                skill_args: None,
                max_turns: 7,
                timeout: 30,
                resume_session: None,
                data_dir: None,
                output: Some(run::OutputMode::Json),
                output_file: Some("result.jsonl".into()),
                verbose: true,
                json: false,
            }),
        })
    );
    for prompt in [
        ["cookie", "run", "-p", "named"],
        ["cookie", "run", "--prompt", "-"],
        ["cookie", "run", "-f", "prompt.txt"],
    ] {
        assert!(Cli::try_parse_from(prompt).is_ok());
    }
    let Command::Run { args } = Cli::try_parse_from([
        "cookie",
        "run",
        "-p",
        "repeat tools",
        "--allowed-tools",
        "read",
        "--allowed-tools",
        "bash,mcp",
        "--json",
    ])
    .unwrap()
    .command
    .unwrap() else {
        panic!("run command");
    };
    assert_eq!(
        args.allowed_tools,
        [
            run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::READ),
            run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::BASH),
            run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::MCP),
        ]
    );
    assert_eq!(args.output_mode(), run::OutputMode::Json);
    let Command::Run { args } = Cli::try_parse_from([
        "cookie",
        "run",
        "prompt",
        "--skill",
        "release-check",
        "--skill-args",
        "v1.2.0",
        "--allowed-tools",
        "skill:release-check",
    ])
    .unwrap()
    .command
    .unwrap() else {
        panic!("run command");
    };
    assert_eq!(args.skill.as_deref(), Some("release-check"));
    assert_eq!(args.skill_args.as_deref(), Some("v1.2.0"));
    assert_eq!(
        args.allowed_tools,
        [run::AllowedTool::Skill("release-check".into())]
    );
    assert!(Cli::try_parse_from(["cookie", "run", "prompt", "--skill-args", "orphan"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "positional", "-p", "named"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "-p", "named", "-f", "prompt.txt"]).is_err());
    assert!(
        Cli::try_parse_from(["cookie", "run", "prompt", "--json", "--output", "text"]).is_err()
    );
    let Command::Run { args } = Cli::try_parse_from([
        "cookie",
        "run",
        "prompt",
        "--output",
        "none",
        "--output-file",
        "result.txt",
    ])
    .unwrap()
    .command
    .unwrap() else {
        panic!("run command");
    };
    assert!(args.validate_cli().is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "prompt", "--max-turns", "0"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "prompt", "--timeout", "0"]).is_err());
}

#[test]
fn every_token_flag_takes_a_token_that_starts_with_a_hyphen() {
    // Tokens written before generated ones avoided a leading `-` stay valid.
    let token = format!("-e{}", &TEST_TOKEN[2..]);
    assert_eq!(
        Cli::try_parse_from(["cookie", "attach", "--token", &token])
            .unwrap()
            .command,
        Some(Command::Attach {
            url: None,
            token: Some(token.clone()),
        })
    );
    for arguments in [
        &["cookie", "web", "--token"][..],
        &["cookie", "connect", "--token"][..],
        &["cookie", "disconnect", "--token"][..],
        &["cookie", "mcp", "--token"][..],
        &["cookie", "config", "status", "--token"][..],
        &["cookie", "machine", "list", "--token"][..],
        &["cookie", "memory", "list", "--token"][..],
        &[
            "cookie",
            "runtime",
            "serve",
            "--daemon",
            "ws://127.0.0.1:7419/ws",
            "--id",
            "devbox",
            "--token",
        ][..],
    ] {
        let mut arguments = arguments.to_vec();
        arguments.push(&token);
        if arguments[1] == "mcp" {
            // `mcp`'s flags come before its subcommand.
            arguments.push("list");
        }
        assert!(
            Cli::try_parse_from(&arguments).is_ok(),
            "{arguments:?}: {:?}",
            Cli::try_parse_from(&arguments).err()
        );
    }
}

#[test]
fn attach_connect_and_disconnect_are_cwd_independent() {
    for command in [
        Command::Attach {
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Web {
            url: None,
            token: Some(TEST_TOKEN.into()),
            open: false,
        },
        Command::Connect {
            provider_id: None,
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Disconnect {
            provider_id: None,
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Mcp {
            command: McpCommand::List,
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: true,
                daemon: None,
                id: None,
                token: None,
                roots: vec!["~/repo".into()],
            },
        },
    ] {
        assert!(
            local_workspace(&Some(command), || panic!("cwd inspected"))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn websocket_subcommands_take_the_daemon_from_flags_or_the_configuration() {
    for arguments in [
        vec!["cookie", "attach"],
        vec!["cookie", "attach", "--token", TEST_TOKEN],
        vec!["cookie", "connect"],
        vec!["cookie", "disconnect"],
        vec!["cookie", "mcp", "list"],
        vec!["cookie", "mcp", "--token", TEST_TOKEN, "list"],
    ] {
        assert!(
            Cli::try_parse_from(&arguments).is_ok(),
            "rejected {arguments:?}"
        );
    }
    let server = |url: Option<&str>, token: Option<&str>| cookie_agent_config::ServerConfig {
        url: url.map(str::to_owned),
        token: token.map(str::to_owned),
        ..Default::default()
    };
    // Flags win and leave the configuration unread.
    let explicit = resolve_daemon_target(
        Some("wss://daemon.example:7419/ws".into()),
        Some("from-flag".into()),
        || panic!("the configuration is not read when both are given"),
    )
    .unwrap();
    assert_eq!(explicit.url, "wss://daemon.example:7419/ws");
    assert_eq!(explicit.token.as_str(), "from-flag");
    // Each missing one comes from `[server]`: its url, else the local daemon.
    let local = resolve_daemon_target(None, None, || Ok(server(None, Some(TEST_TOKEN)))).unwrap();
    assert_eq!(local.url, "ws://127.0.0.1:7419/ws");
    assert_eq!(local.token.as_str(), TEST_TOKEN);
    let remote = resolve_daemon_target(None, Some("from-flag".into()), || {
        Ok(server(Some("ws://laptop:7419/ws"), Some(TEST_TOKEN)))
    })
    .unwrap();
    assert_eq!(remote.url, "ws://laptop:7419/ws");
    assert_eq!(remote.token.as_str(), "from-flag");
    let error = resolve_daemon_target(None, None, || Ok(server(None, None)))
        .err()
        .expect("no token anywhere");
    assert!(error.to_string().contains("[server]"), "{error}");
    assert!(
        resolve_daemon_target(Some("ws://laptop/other".into()), Some("x".into()), || {
            panic!("unread")
        })
        .is_err()
    );
}

#[test]
fn websocket_endpoints_name_the_exact_client_path() {
    for url in [
        "ws://127.0.0.1:7419/ws",
        "wss://localhost:7419/ws",
        "ws://[::1]:7419/ws",
        "wss://daemon.example.com:7419/ws",
    ] {
        validate_websocket_url(url).unwrap();
    }
    for url in [
        "http://127.0.0.1:7419/ws",
        "ws://user:pass@127.0.0.1:7419/ws",
        "ws://127.0.0.1:7419/other",
        "ws://127.0.0.1:7419/ws?id=x",
    ] {
        assert!(validate_websocket_url(url).is_err(), "{url}");
    }
    assert_eq!(
        cookie_agent_protocol_transport::runtime_dial_in_url(
            "wss://daemon.example.com:7419/ws",
            "labpc"
        )
        .unwrap(),
        "wss://daemon.example.com:7419/runtime?id=labpc"
    );
}
