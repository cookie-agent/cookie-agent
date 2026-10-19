//! Plain `cookie` and a daemon already running on this machine
//! (`docs/site/specs/web-client.md` section 13): finding the daemon that
//! `[server]` describes, and choosing the runtime the TUI's sessions use,
//! which `cookie attach` shares.

// Without the TUI, a found daemon has no frontend to choose a runtime for.
#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use std::time::Duration;

use cookie_agent_protocol::{
    CapabilityName, Client, ClientError, ClientIdentity, MachinesListResult, RuntimeId,
    RuntimesListResult, ServerHello,
};
use zeroize::Zeroizing;

/// How long the TCP connect to the probed address may take.
pub(crate) const CONNECT_DEADLINE: Duration = Duration::from_millis(500);

/// How long the WebSocket upgrade and the protocol handshake may take once
/// the TCP connection is open.
pub(crate) const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(2);

/// What probing the daemon `[server]` describes found (section 13.1).
pub(crate) enum Probe {
    /// `[server]` has no token, so no daemon has run with this
    /// configuration; nothing was dialed.
    NoToken,
    /// Nothing listens: the connection was refused, or the connect deadline
    /// passed.
    NothingListens,
    /// Something answered at `url` but the handshake failed, for `reason`:
    /// a wrong token, another protocol major, a TLS listener, or not a
    /// cookie daemon.
    Unusable { url: String, reason: String },
    /// A daemon answered and the handshake succeeded.
    Connected(LocalDaemon),
}

/// A daemon found on this machine, connected and past the handshake.
pub(crate) struct LocalDaemon {
    pub(crate) client: Client,
    pub(crate) hello: ServerHello,
    pub(crate) url: String,
    pub(crate) token: Zeroizing<String>,
}

impl Probe {
    /// The notice plain `cookie` shows when it runs in-process after this
    /// probe: only when something answered that it could not use.
    pub(crate) fn notice(&self) -> Option<String> {
        match self {
            Self::NoToken | Self::NothingListens | Self::Connected(_) => None,
            Self::Unusable { url, reason } => Some(format!(
                "Running standalone: the daemon at {url} could not be used ({reason}). \
                 Run `cookie attach` to require it, or `cookie --standalone` to stop \
                 looking for it."
            )),
        }
    }
}

/// Probes `ws://<host>:<port>/ws` of `server` with its token, handshaking
/// as `identity`, within [`CONNECT_DEADLINE`] and [`HANDSHAKE_DEADLINE`].
/// `[server] url` is never read: plain `cookie` does not go to another
/// machine on its own.
pub(crate) async fn probe(
    server: &cookie_agent_config::ServerConfig,
    identity: ClientIdentity,
) -> Probe {
    probe_within(server, identity, CONNECT_DEADLINE, HANDSHAKE_DEADLINE).await
}

pub(crate) async fn probe_within(
    server: &cookie_agent_config::ServerConfig,
    identity: ClientIdentity,
    connect_deadline: Duration,
    handshake_deadline: Duration,
) -> Probe {
    let Some(token) = server.token.clone().map(Zeroizing::new) else {
        return Probe::NoToken;
    };
    let url = server.local_url();
    let address = (server.host.as_str(), server.port);
    let stream =
        match tokio::time::timeout(connect_deadline, tokio::net::TcpStream::connect(address)).await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) | Err(_) => return Probe::NothingListens,
        };
    let connected = tokio::time::timeout(handshake_deadline, async {
        let client = Client::connect_websocket_over(stream, &url, &token).await?;
        let hello = client.handshake_as(identity).await?;
        Ok::<_, ClientError>((client, hello))
    })
    .await;
    match connected {
        Ok(Ok((client, hello))) => Probe::Connected(LocalDaemon {
            client,
            hello,
            url,
            token,
        }),
        Ok(Err(error)) => Probe::Unusable {
            url,
            reason: failure_reason(&error),
        },
        Err(_) => Probe::Unusable {
            url,
            reason: format!("no handshake within {} ms", handshake_deadline.as_millis()),
        },
    }
}

/// The first line of `error` without the layers' prefixes: a refused
/// upgrade carries the response body on the lines after it.
fn failure_reason(error: &ClientError) -> String {
    let message = error.to_string();
    let mut line = message.lines().next().unwrap_or_default().trim();
    while let Some(rest) = ["websocket error: ", "transport error: "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
    {
        line = rest;
    }
    line.to_owned()
}

/// The runtime the TUI's sessions use for its directory (section 13.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeChoice {
    /// The daemon runs on this machine: its `local` runtime.
    Local,
    /// A runtime of the daemon runs on this machine: this capability of it,
    /// which takes a workspace.
    Existing {
        runtime_id: RuntimeId,
        capability: CapabilityName,
    },
    /// Nothing of the daemon runs here: this client dials in its own
    /// runtime.
    DialIn,
}

/// Whether the daemon `hello` describes runs on the machine
/// `local_machine` names; a missing ID on either side counts as the same
/// machine.
pub(crate) fn same_machine(hello: &ServerHello, local_machine: Option<&str>) -> bool {
    match (hello.machine_id.as_deref(), local_machine) {
        (Some(daemon), Some(local)) => daemon == local,
        _ => true,
    }
}

/// Chooses the runtime by the three steps of section 13.2, from reported
/// facts only, never a kind label:
///
/// 1. the daemon runs on this machine: `local`;
/// 2. else a connected runtime that `runtimes` places on a machine whose
///    `ids` include `local_machine` and that offers a capability taking a
///    workspace, the lowest runtime ID first, its first such capability;
/// 3. else a dial-in.
///
/// `own_runtime`, the ID this client dials in as, is no candidate in
/// step 2: under it runs another client of this machine, limited to that
/// client's directory and gone when it quits.
pub(crate) fn choose_runtime(
    hello: &ServerHello,
    local_machine: Option<&str>,
    runtimes: &RuntimesListResult,
    machines: &MachinesListResult,
    own_runtime: &str,
) -> RuntimeChoice {
    if same_machine(hello, local_machine) {
        return RuntimeChoice::Local;
    }
    let Some(local_machine) = local_machine else {
        return RuntimeChoice::Local;
    };
    let here = machines
        .machines
        .iter()
        .filter(|machine| machine.ids.iter().any(|id| id == local_machine))
        .map(|machine| &machine.name)
        .collect::<Vec<_>>();
    runtimes
        .runtimes
        .iter()
        .filter(|runtime| runtime.connected && runtime.runtime_id.as_str() != own_runtime)
        .filter(|runtime| {
            runtime
                .machine
                .as_ref()
                .is_some_and(|machine| here.contains(&machine))
        })
        .filter_map(|runtime| {
            runtime
                .capabilities
                .iter()
                .find(|capability| capability.takes_workspace)
                .map(|capability| (&runtime.runtime_id, &capability.name))
        })
        .min_by(|(left, _), (right, _)| left.cmp(right))
        .map_or(RuntimeChoice::DialIn, |(runtime_id, capability)| {
            RuntimeChoice::Existing {
                runtime_id: runtime_id.clone(),
                capability: capability.clone(),
            }
        })
}

/// [`choose_runtime`] against the daemon `client` reaches, reading
/// `runtimes.list` and `machines.list` only when the daemon is on another
/// machine. A daemon that cannot list them gets a dial-in.
pub(crate) async fn runtime_for(
    client: &Client,
    hello: &ServerHello,
    local_machine: Option<&str>,
    own_runtime: &str,
) -> RuntimeChoice {
    if same_machine(hello, local_machine) {
        return RuntimeChoice::Local;
    }
    match tokio::join!(client.list_runtimes(), client.list_machines()) {
        (Ok(runtimes), Ok(machines)) => {
            choose_runtime(hello, local_machine, &runtimes, &machines, own_runtime)
        }
        _ => RuntimeChoice::DialIn,
    }
}

/// The notice that the daemon runs another version than this binary, once
/// per start; the protocol versions alone decide whether the two connect.
pub(crate) fn version_notice(daemon_version: &str, own_version: &str) -> Option<String> {
    (daemon_version != own_version).then(|| {
        format!(
            "The daemon runs {daemon_version}, this is {own_version}; restart the daemon to \
             upgrade."
        )
    })
}

/// How the TUI names the daemon at the client URL `url`: its host and port,
/// with `wss://` kept in front.
pub(crate) fn daemon_address(url: &str) -> String {
    let address = url.strip_suffix("/ws").unwrap_or(url);
    address.strip_prefix("ws://").unwrap_or(address).to_owned()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{
        EventOrigin, MachineInfo, MachineName, RuntimeCapabilityInfo, RuntimeKind, RuntimeLifetime,
        RuntimeListing, SemVer,
    };

    use super::*;

    const HERE: &str = "machine-here";
    const THERE: &str = "machine-there";
    const OWN: &str = "client_laptop";

    fn hello(machine_id: Option<&str>) -> ServerHello {
        ServerHello {
            protocol_version: SemVer::new(1, 0, 0),
            engine_version: "0.3.0".into(),
            origin: EventOrigin::new("user").unwrap(),
            machine_id: machine_id.map(str::to_owned),
            features: Vec::new(),
        }
    }

    fn machine(name: &str, ids: &[&str]) -> MachineInfo {
        MachineInfo {
            name: MachineName::new(name).unwrap(),
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            pinned: Vec::new(),
            connected: Vec::new(),
            platform: None,
            memories: 0,
        }
    }

    fn capability(name: &str, kind: &str, takes_workspace: bool) -> RuntimeCapabilityInfo {
        RuntimeCapabilityInfo {
            name: CapabilityName::new(name).unwrap(),
            kind: RuntimeKind::new(kind).unwrap(),
            takes_workspace,
            description: String::new(),
        }
    }

    fn runtime(
        id: &str,
        machine: Option<&str>,
        connected: bool,
        capabilities: Vec<RuntimeCapabilityInfo>,
    ) -> RuntimeListing {
        RuntimeListing {
            runtime_id: RuntimeId::new(id).unwrap(),
            lifetime: RuntimeLifetime::DialIn,
            connected,
            capabilities,
            platform: None,
            machine: machine.map(|name| MachineName::new(name).unwrap()),
            message: None,
        }
    }

    fn listings(
        runtimes: Vec<RuntimeListing>,
        machines: Vec<MachineInfo>,
    ) -> (RuntimesListResult, MachinesListResult) {
        (
            RuntimesListResult {
                runtimes,
                instances: Vec::new(),
            },
            MachinesListResult { machines },
        )
    }

    fn choose(
        hello: &ServerHello,
        local: Option<&str>,
        (runtimes, machines): &(RuntimesListResult, MachinesListResult),
    ) -> RuntimeChoice {
        choose_runtime(hello, local, runtimes, machines, OWN)
    }

    fn existing(runtime_id: &str, capability: &str) -> RuntimeChoice {
        RuntimeChoice::Existing {
            runtime_id: RuntimeId::new(runtime_id).unwrap(),
            capability: CapabilityName::new(capability).unwrap(),
        }
    }

    /// The daemon's own machine and a laptop with a persistent runtime of a
    /// made-up kind and a workspace capability that is not named `coding`.
    fn laptop_with_runtime() -> (RuntimesListResult, MachinesListResult) {
        listings(
            vec![
                runtime(
                    "server",
                    Some("server"),
                    true,
                    vec![capability("coding", "coding", true)],
                ),
                runtime(
                    "laptop",
                    Some("laptop"),
                    true,
                    vec![
                        capability("search", "search", false),
                        capability("shell", "custom-shell", true),
                    ],
                ),
            ],
            vec![machine("laptop", &[HERE]), machine("server", &[THERE])],
        )
    }

    #[test]
    fn only_an_answer_the_client_cannot_use_leaves_a_notice() {
        assert_eq!(Probe::NoToken.notice(), None);
        assert_eq!(Probe::NothingListens.notice(), None);
        let notice = Probe::Unusable {
            url: "ws://127.0.0.1:7419/ws".into(),
            reason: "HTTP error: 401 Unauthorized".into(),
        }
        .notice()
        .expect("notice");
        for part in [
            "ws://127.0.0.1:7419/ws",
            "HTTP error: 401 Unauthorized",
            "`cookie attach`",
            "`cookie --standalone`",
        ] {
            assert!(notice.contains(part), "{notice}");
        }
    }

    #[tokio::test]
    async fn a_connected_probe_leaves_no_notice() {
        let (client_stream, _server_stream) = cookie_agent_protocol::in_process_pair(1);
        let probe = Probe::Connected(LocalDaemon {
            client: Client::connect_stream(client_stream),
            hello: hello(Some(HERE)),
            url: "ws://127.0.0.1:7419/ws".into(),
            token: Zeroizing::new(String::new()),
        });
        assert_eq!(probe.notice(), None);
    }

    #[test]
    fn a_daemon_on_this_machine_serves_with_local() {
        let listed = laptop_with_runtime();
        assert_eq!(
            choose(&hello(Some(HERE)), Some(HERE), &listed),
            RuntimeChoice::Local
        );
        // A side that cannot tell its machine counts as this machine.
        assert_eq!(
            choose(&hello(None), Some(HERE), &listed),
            RuntimeChoice::Local
        );
        assert_eq!(
            choose(&hello(Some(THERE)), None, &listed),
            RuntimeChoice::Local
        );
        assert_eq!(choose(&hello(None), None, &listed), RuntimeChoice::Local);
        assert!(same_machine(&hello(Some(HERE)), Some(HERE)));
        assert!(!same_machine(&hello(Some(THERE)), Some(HERE)));
    }

    #[test]
    fn a_daemon_runtime_on_this_machine_serves_with_its_workspace_capability() {
        assert_eq!(
            choose(&hello(Some(THERE)), Some(HERE), &laptop_with_runtime()),
            existing("laptop", "shell")
        );
    }

    #[test]
    fn several_runtimes_on_this_machine_choose_the_lowest_id() {
        let listed = listings(
            vec![
                runtime(
                    "zeta",
                    Some("laptop"),
                    true,
                    vec![capability("coding", "coding", true)],
                ),
                runtime(
                    "alpha",
                    Some("desk"),
                    true,
                    vec![capability("code", "x", true)],
                ),
                runtime(
                    "beta",
                    Some("laptop"),
                    true,
                    vec![capability("coding", "coding", true)],
                ),
            ],
            // One machine of the registry may stand for several IDs.
            vec![
                machine("desk", &["other", HERE]),
                machine("laptop", &[HERE]),
            ],
        );
        assert_eq!(
            choose(&hello(Some(THERE)), Some(HERE), &listed),
            existing("alpha", "code")
        );
    }

    #[test]
    fn runtimes_that_cannot_serve_the_directory_here_are_passed_over() {
        let listed = listings(
            vec![
                // Not connected.
                runtime(
                    "absent",
                    Some("laptop"),
                    false,
                    vec![capability("coding", "coding", true)],
                ),
                // Another machine.
                runtime(
                    "remote",
                    Some("server"),
                    true,
                    vec![capability("coding", "coding", true)],
                ),
                // No machine the daemon can tell.
                runtime(
                    "unplaced",
                    None,
                    true,
                    vec![capability("coding", "coding", true)],
                ),
                // No capability that takes a workspace, whatever its kind says.
                runtime(
                    "tools",
                    Some("laptop"),
                    true,
                    vec![capability("coding", "coding", false)],
                ),
                // The ID this client dials in as: another client's runtime.
                runtime(
                    OWN,
                    Some("laptop"),
                    true,
                    vec![capability("coding", "coding", true)],
                ),
            ],
            vec![machine("laptop", &[HERE]), machine("server", &[THERE])],
        );
        assert_eq!(
            choose(&hello(Some(THERE)), Some(HERE), &listed),
            RuntimeChoice::DialIn
        );
    }

    #[test]
    fn nothing_on_this_machine_dials_in() {
        let listed = listings(
            vec![runtime(
                "server",
                Some("server"),
                true,
                vec![capability("coding", "coding", true)],
            )],
            vec![machine("server", &[THERE])],
        );
        assert_eq!(
            choose(&hello(Some(THERE)), Some(HERE), &listed),
            RuntimeChoice::DialIn
        );
        assert_eq!(
            choose(
                &hello(Some(THERE)),
                Some(HERE),
                &listings(Vec::new(), Vec::new())
            ),
            RuntimeChoice::DialIn
        );
    }

    #[test]
    fn a_daemon_of_another_version_is_noted() {
        assert_eq!(version_notice("0.6.0", "0.6.0"), None);
        assert_eq!(
            version_notice("0.5.0", "0.6.0").as_deref(),
            Some("The daemon runs 0.5.0, this is 0.6.0; restart the daemon to upgrade.")
        );
    }

    #[test]
    fn the_daemon_address_drops_the_plain_scheme_and_path() {
        assert_eq!(daemon_address("ws://127.0.0.1:7419/ws"), "127.0.0.1:7419");
        assert_eq!(daemon_address("wss://labpc:7419/ws"), "wss://labpc:7419");
    }

    #[test]
    fn a_refused_upgrade_reason_is_its_first_line() {
        let error = ClientError::WebSocket(
            "transport error: HTTP error: 401 Unauthorized\nResponse body:\nunauthorized".into(),
        );
        assert_eq!(failure_reason(&error), "HTTP error: 401 Unauthorized");
    }
}
