//! The Machines & runtimes view's rules, independent of the browser: the
//! gesture that drags a runtime to another machine (a mouse drags at
//! once, a finger after a long press), the machine a drop moves it to,
//! the `[runtimes.<id>]` template an ssh form writes, the command a
//! runtime dials in with, and the guides the view links to.

use cookie_agent_client_state::runtimes::MachineRequest;
use cookie_agent_protocol::{ConfigChange, MachineName, RuntimeId, RuntimeLifetime};

use crate::layout::PRESS_SLOP;

/// How far a mouse moves with its button down before it drags, in CSS
/// pixels: less is a click.
pub const DRAG_SLOP: f64 = 4.0;

/// Where a press on a runtime's row stands.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    /// Down, not yet dragging: a mouse until it moves, a finger until its
    /// hold timer runs out.
    Pressed {
        x: f64,
        y: f64,
        touch: bool,
    },
    /// A finger held long enough: the row is lifted, and the page must not
    /// scroll under it.
    Lifted {
        x: f64,
        y: f64,
    },
    Dragging {
        x: f64,
        y: f64,
    },
}

/// The drag of a runtime's row, one at a time.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Drag {
    phase: Phase,
}

/// What a pointer's move did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragStep {
    Nothing,
    /// A finger moved before its long press: it scrolls, and the press is
    /// over.
    Cancelled,
    /// The drag began; the row follows the pointer from here.
    Started,
    /// The row is this far from where it was pressed.
    Moved {
        dx: f64,
        dy: f64,
    },
}

/// How a press ended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragEnd {
    /// A click, or a press already given up.
    Nothing,
    /// A long press let go where it was: the row offers its "Move to…".
    Held,
    /// A drag let go: the row moves to the machine under the pointer.
    Dropped,
}

impl Drag {
    /// A button or finger went down at `x`, `y`.
    pub fn press(&mut self, x: f64, y: f64, touch: bool) {
        self.phase = Phase::Pressed { x, y, touch };
    }

    /// The hold timer of a finger ran out; `true` when it lifts the row.
    pub fn elapse(&mut self) -> bool {
        if let Phase::Pressed { x, y, touch: true } = self.phase {
            self.phase = Phase::Lifted { x, y };
            return true;
        }
        false
    }

    /// The pointer moved to `x`, `y`.
    pub fn moved(&mut self, x: f64, y: f64) -> DragStep {
        let far = |from_x: f64, from_y: f64, slop: f64| (x - from_x).hypot(y - from_y) > slop;
        match self.phase {
            Phase::Idle => DragStep::Nothing,
            Phase::Pressed {
                x: from_x,
                y: from_y,
                touch: false,
            } if far(from_x, from_y, DRAG_SLOP) => {
                self.phase = Phase::Dragging {
                    x: from_x,
                    y: from_y,
                };
                DragStep::Started
            }
            Phase::Pressed {
                x: from_x,
                y: from_y,
                touch: true,
            } if far(from_x, from_y, PRESS_SLOP) => {
                self.phase = Phase::Idle;
                DragStep::Cancelled
            }
            Phase::Pressed { .. } => DragStep::Nothing,
            Phase::Lifted {
                x: from_x,
                y: from_y,
            } => {
                self.phase = Phase::Dragging {
                    x: from_x,
                    y: from_y,
                };
                DragStep::Started
            }
            Phase::Dragging {
                x: from_x,
                y: from_y,
            } => DragStep::Moved {
                dx: x - from_x,
                dy: y - from_y,
            },
        }
    }

    /// The button or finger came up.
    pub fn release(&mut self) -> DragEnd {
        let end = match self.phase {
            Phase::Idle | Phase::Pressed { .. } => DragEnd::Nothing,
            Phase::Lifted { .. } => DragEnd::Held,
            Phase::Dragging { .. } => DragEnd::Dropped,
        };
        self.phase = Phase::Idle;
        end
    }

    /// The browser took the gesture over.
    pub fn cancel(&mut self) {
        self.phase = Phase::Idle;
    }

    /// Whether a finger's move must not scroll the page: the row is lifted
    /// or dragged.
    pub fn holds_scroll(&self) -> bool {
        matches!(self.phase, Phase::Lifted { .. } | Phase::Dragging { .. })
    }
}

/// The change a drop of `runtime`, which is on machine `from` (or on
/// none), onto the machine named `target` makes: none when it lands on no
/// machine or on its own.
pub fn drop_request(
    runtime: &RuntimeId,
    from: Option<&MachineName>,
    target: Option<&str>,
) -> Option<MachineRequest> {
    let target = target?;
    if from.is_some_and(|from| from.as_str() == target) {
        return None;
    }
    Some(MachineRequest::Assign {
        runtime: runtime.clone(),
        machine: MachineName::new(target).ok()?,
    })
}

/// The change "Move to…" makes for `runtime` with the machine name typed
/// (a new name creates the machine), or why the name is not one.
pub fn move_request(runtime: &RuntimeId, typed: &str) -> Result<MachineRequest, String> {
    let typed = typed.trim();
    let machine = MachineName::new(typed).map_err(|_| {
        format!(
            "`{typed}` is not a machine name: a lowercase letter, then up to 31 lowercase \
             letters, digits, `_`, or `-`"
        )
    })?;
    Ok(MachineRequest::Assign {
        runtime: runtime.clone(),
        machine,
    })
}

/// How long a runtime's process may take to start, in the template the
/// ssh form writes: ssh adds a connection to the start.
pub const SSH_STARTUP_TIMEOUT_MS: u64 = 20_000;

/// A `[runtimes.<id>]` template that runs `cookie runtime serve --stdio`
/// on another machine over ssh, as the runtimes guide's example does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshTemplate {
    pub id: RuntimeId,
    /// What ssh connects to: a host, `user@host`, or a `Host` of the ssh
    /// configuration.
    pub host: String,
    /// The one directory `--root` limits the runtime's workspaces to.
    pub root: Option<String>,
}

impl SshTemplate {
    /// The template of the form's fields, or why they cannot make one.
    pub fn new(id: &str, host: &str, root: &str) -> Result<Self, String> {
        let id = id.trim();
        let id = RuntimeId::new(id).map_err(|_| {
            format!(
                "`{id}` is not a runtime ID: a lowercase letter, then up to 31 lowercase \
                 letters, digits, or `_`"
            )
        })?;
        if id.is_reserved() {
            return Err(format!("`{id}` is a built-in runtime's ID"));
        }
        let host = host.trim();
        if host.is_empty() {
            return Err("name the host ssh connects to".into());
        }
        // The host is one ssh argument: an option there would change what
        // ssh runs.
        if host.starts_with('-') || !host.chars().all(is_plain) {
            return Err(format!(
                "`{host}` is not a host: letters, digits, and `.`, `-`, `_`, `@`, or `:`"
            ));
        }
        let root = root.trim();
        // ssh joins the arguments into one string for the remote shell, so
        // a root it would split or expand is refused rather than quoted.
        if !root.is_empty()
            && !root
                .chars()
                .all(|char| is_plain(char) || "/~+,".contains(char))
        {
            return Err(format!(
                "`{root}` is not a directory the template can pass through ssh: letters, digits, \
                 and `/`, `~`, `.`, `-`, `_`"
            ));
        }
        Ok(Self {
            id,
            host: host.to_owned(),
            root: (!root.is_empty()).then(|| root.to_owned()),
        })
    }

    /// The program and its arguments.
    pub fn command(&self) -> Vec<String> {
        let mut command = ["ssh", "-o", "BatchMode=yes", &self.host]
            .into_iter()
            .chain(["cookie", "runtime", "serve", "--stdio"])
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if let Some(root) = &self.root {
            command.extend(["--root".to_owned(), root.clone()]);
        }
        command
    }

    /// The `config.apply` change that writes the template into
    /// `config.toml`.
    pub fn change(&self) -> ConfigChange {
        ConfigChange::Set {
            key: vec!["runtimes".into(), self.id.to_string()],
            value: serde_json::json!({
                "command": self.command(),
                "startup_timeout_ms": SSH_STARTUP_TIMEOUT_MS,
            }),
        }
    }

    /// The template as `config.toml` will have it, for the form to show.
    pub fn toml(&self) -> String {
        let command = self
            .command()
            .iter()
            .map(|argument| {
                format!(
                    "\"{}\"",
                    argument.replace('\\', "\\\\").replace('"', "\\\"")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[runtimes.{}]\ncommand = [{command}]\nstartup_timeout_ms = {SSH_STARTUP_TIMEOUT_MS}\n",
            self.id
        )
    }
}

fn is_plain(char: char) -> bool {
    char.is_ascii_alphanumeric() || "._-@:".contains(char)
}

/// Whether `config_text` (a `config.toml`) has a template for `id`
/// already: its `[runtimes.<id>]` table, or a key under it.
pub fn has_template(config_text: &str, id: &RuntimeId) -> bool {
    let table = format!("[runtimes.{id}]");
    let nested = format!("[runtimes.{id}.");
    config_text.lines().map(str::trim).any(|line| {
        line == table || line.starts_with(&nested) || line.starts_with(&format!("{table} "))
    })
}

/// What a runtime with `id` (the host's name when empty) runs on its own
/// machine to dial in to the daemon at `daemon_url`, with `token`, the one
/// this page connected with, or a placeholder when the page keeps none. A
/// loopback URL is the page's own view of the daemon, which another machine
/// cannot reach, so its host becomes a placeholder.
pub fn dial_in_command(daemon_url: Option<&str>, id: &str, token: Option<&str>) -> String {
    let url = daemon_url
        .and_then(|url| url::Url::parse(url).ok())
        .map_or_else(
            || "ws://<daemon host>:7419/ws".to_owned(),
            |mut url| {
                let loopback = match url.host() {
                    Some(url::Host::Domain(domain)) => domain == "localhost",
                    Some(url::Host::Ipv4(address)) => address.is_loopback(),
                    Some(url::Host::Ipv6(address)) => address.is_loopback(),
                    None => true,
                };
                if loopback {
                    let port = url.port_or_known_default().unwrap_or(7419);
                    let path = url.path().to_owned();
                    return format!("{}://<daemon host>:{port}{path}", url.scheme());
                }
                url.set_fragment(None);
                url.set_query(None);
                url.to_string()
            },
        );
    let id = id.trim();
    let id = if id.is_empty() {
        String::new()
    } else {
        format!(" --id {id}")
    };
    let token = token.map_or_else(|| TOKEN_PLACEHOLDER.to_owned(), str::to_owned);
    format!("cookie runtime serve --daemon {url}{id} --token \"{token}\"")
}

/// What stands for the token when the page keeps none.
const TOKEN_PLACEHOLDER: &str = "<the daemon's token>";

/// `command` with `token` hidden, for showing it on screen: copying takes the
/// command itself.
pub fn masked_token(command: &str, token: Option<&str>) -> String {
    match token {
        Some(token) if !token.is_empty() => command.replace(token, &"•".repeat(12)),
        _ => command.to_owned(),
    }
}

/// Where the guides live: the documentation of the released version.
pub const DOCS: &str = "https://cookie-agent.github.io/doc/latest/";

/// A guide the view links to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Guide {
    pub title: &'static str,
    /// Under [`DOCS`].
    pub page: &'static str,
    pub note: &'static str,
}

impl Guide {
    pub fn url(self) -> String {
        format!("{DOCS}{}", self.page)
    }
}

/// The guides of the "Add a runtime" panel.
pub const GUIDES: [Guide; 5] = [
    Guide {
        title: "Runtimes and machines",
        page: "concepts/runtimes/",
        note: "what a runtime is, and how it belongs to a machine",
    },
    Guide {
        title: "A repository on another machine over ssh",
        page: "runtimes/templates/#example-a-repository-on-another-machine-over-ssh",
        note: "the template, installing cookie there, and attaching it",
    },
    Guide {
        title: "Runtimes that dial in",
        page: "runtimes/dial-in/",
        note: "the listener, the token, IDs, and reconnecting",
    },
    Guide {
        title: "Remote access",
        page: "daemon/remote-access/",
        note: "listening on another address, tunnels, and TLS",
    },
    Guide {
        title: "Running as a service",
        page: "runtimes/service/",
        note: "keeping a dial-in runtime connected",
    },
];

/// How a runtime came to the daemon, as its row says it.
pub const fn lifetime_label(lifetime: RuntimeLifetime) -> &'static str {
    match lifetime {
        RuntimeLifetime::Builtin => "built in",
        RuntimeLifetime::DialIn => "dials in",
        RuntimeLifetime::Spawned => "started by the daemon",
    }
}

/// The icon of a runtime by how it came to the daemon; a runtime the
/// registry does not list (pinned, never seen) has a generic one.
pub fn lifetime_icon(lifetime: Option<RuntimeLifetime>) -> icondata::Icon {
    match lifetime {
        Some(RuntimeLifetime::Builtin) => icondata::FaCubeSolid,
        Some(RuntimeLifetime::DialIn) => icondata::FaPlugSolid,
        Some(RuntimeLifetime::Spawned) => icondata::FaTerminalSolid,
        None => icondata::FaCircleQuestionRegular,
    }
}

/// The icon of a machine by the operating system its runtimes report.
pub fn platform_icon(platform: Option<&str>) -> icondata::Icon {
    match platform {
        Some("linux") => icondata::FaLinuxBrands,
        Some("macos" | "ios") => icondata::FaAppleBrands,
        Some("windows") => icondata::FaWindowsBrands,
        _ => icondata::FaDesktopSolid,
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_client_state::runtimes::MachineRequest;
    use cookie_agent_protocol::{ConfigChange, MachineName, RuntimeId};

    use super::*;

    #[test]
    fn a_mouse_drags_once_it_moves_and_a_drop_ends_it() {
        let mut drag = Drag::default();
        drag.press(10.0, 10.0, false);
        assert!(!drag.elapse(), "a mouse has no long press");
        assert_eq!(
            drag.moved(11.0, 11.0),
            DragStep::Nothing,
            "a tremble is a click"
        );
        assert_eq!(drag.moved(30.0, 10.0), DragStep::Started);
        assert_eq!(
            drag.moved(40.0, 60.0),
            DragStep::Moved { dx: 30.0, dy: 50.0 }
        );
        assert!(drag.holds_scroll());
        assert_eq!(drag.release(), DragEnd::Dropped);
        assert_eq!(
            drag.moved(50.0, 50.0),
            DragStep::Nothing,
            "released, nothing follows"
        );

        drag.press(0.0, 0.0, false);
        assert_eq!(drag.release(), DragEnd::Nothing, "a click without a move");
    }

    #[test]
    fn a_finger_drags_after_a_long_press_and_scrolls_before_it() {
        let mut drag = Drag::default();
        drag.press(100.0, 100.0, true);
        assert!(
            !drag.holds_scroll(),
            "the page scrolls until the press is long"
        );
        assert_eq!(
            drag.moved(100.0, 100.0 + PRESS_SLOP + 1.0),
            DragStep::Cancelled
        );
        assert!(!drag.elapse(), "the scroll ended the press");
        assert_eq!(drag.release(), DragEnd::Nothing);

        drag.press(100.0, 100.0, true);
        assert_eq!(
            drag.moved(103.0, 102.0),
            DragStep::Nothing,
            "a tremble keeps the press"
        );
        assert!(drag.elapse());
        assert!(drag.holds_scroll(), "the lifted row keeps the page still");
        assert_eq!(drag.moved(101.0, 101.0), DragStep::Started);
        assert_eq!(
            drag.moved(100.0, 160.0),
            DragStep::Moved { dx: 0.0, dy: 60.0 }
        );
        assert_eq!(drag.release(), DragEnd::Dropped);

        drag.press(0.0, 0.0, true);
        assert!(drag.elapse());
        assert_eq!(
            drag.release(),
            DragEnd::Held,
            "held and let go offers Move to…"
        );

        drag.press(0.0, 0.0, true);
        drag.cancel();
        assert!(!drag.elapse());
    }

    #[test]
    fn a_drop_moves_the_runtime_to_another_machine_only() {
        let runtime = RuntimeId::new("box").unwrap();
        let laptop = MachineName::new("laptop").unwrap();
        assert_eq!(drop_request(&runtime, Some(&laptop), Some("laptop")), None);
        assert_eq!(drop_request(&runtime, Some(&laptop), None), None);
        assert_eq!(
            drop_request(&runtime, Some(&laptop), Some("labpc")),
            Some(MachineRequest::Assign {
                runtime: runtime.clone(),
                machine: MachineName::new("labpc").unwrap(),
            })
        );
        assert!(
            drop_request(&runtime, None, Some("laptop")).is_some(),
            "a runtime on no machine can be pinned to one"
        );
    }

    #[test]
    fn move_to_takes_a_new_machine_name_or_says_why_not() {
        let runtime = RuntimeId::new("box").unwrap();
        assert_eq!(
            move_request(&runtime, " lab-2 "),
            Ok(MachineRequest::Assign {
                runtime: runtime.clone(),
                machine: MachineName::new("lab-2").unwrap(),
            })
        );
        assert!(
            move_request(&runtime, "Lab PC")
                .unwrap_err()
                .contains("not a machine name")
        );
    }

    #[test]
    fn the_ssh_form_writes_the_guides_template() {
        let template = SshTemplate::new(" labpc ", "me@labpc.lan", "").unwrap();
        assert_eq!(
            template.toml(),
            "[runtimes.labpc]\ncommand = [\"ssh\", \"-o\", \"BatchMode=yes\", \"me@labpc.lan\", \
             \"cookie\", \"runtime\", \"serve\", \"--stdio\"]\nstartup_timeout_ms = 20000\n"
        );
        let ConfigChange::Set { key, value } = template.change() else {
            panic!("not a set");
        };
        assert_eq!(key, ["runtimes", "labpc"]);
        assert_eq!(value["command"][3], "me@labpc.lan");
        assert_eq!(value["startup_timeout_ms"], SSH_STARTUP_TIMEOUT_MS);
        let rooted = SshTemplate::new("labpc", "labpc", "~/src").unwrap();
        assert_eq!(rooted.command()[8..], ["--root", "~/src"]);
    }

    #[test]
    fn the_ssh_form_refuses_what_ssh_or_the_daemon_would_not_take() {
        assert!(SshTemplate::new("Lab PC", "labpc", "").is_err());
        assert!(SshTemplate::new("local", "labpc", "").is_err());
        assert!(SshTemplate::new("global", "labpc", "").is_err());
        assert!(SshTemplate::new("labpc", "", "").is_err());
        assert!(SshTemplate::new("labpc", "-oProxyCommand=x", "").is_err());
        assert!(SshTemplate::new("labpc", "lab pc", "").is_err());
        assert!(SshTemplate::new("labpc", "labpc", "/home/me/my repo").is_err());
        assert!(SshTemplate::new("labpc", "labpc", "$HOME").is_err());
    }

    #[test]
    fn an_existing_template_is_found_in_config_toml() {
        let id = RuntimeId::new("labpc").unwrap();
        assert!(has_template(
            "[server]\n\n[runtimes.labpc]\ncommand = []\n",
            &id
        ));
        assert!(has_template("[runtimes.labpc.mcp.servers.x]\n", &id));
        assert!(!has_template("[runtimes.labpc2]\n", &id));
        assert!(!has_template(
            "# [runtimes.lab]\n",
            &RuntimeId::new("lab").unwrap()
        ));
    }

    #[test]
    fn the_dial_in_command_names_the_daemon_and_carries_the_page_s_token() {
        assert_eq!(
            dial_in_command(Some("ws://127.0.0.1:7420/ws"), "", None),
            "cookie runtime serve --daemon ws://<daemon host>:7420/ws --token \"<the daemon's token>\""
        );
        assert_eq!(
            dial_in_command(Some("wss://box.tail.ts.net/ws"), "labpc", Some("s3cr3t")),
            "cookie runtime serve --daemon wss://box.tail.ts.net/ws --id labpc --token \"s3cr3t\""
        );
        assert!(dial_in_command(None, "", None).contains("ws://<daemon host>:7419/ws"));
    }

    #[test]
    fn the_shown_command_hides_the_token_that_copying_keeps() {
        let command = dial_in_command(Some("wss://box.tail.ts.net/ws"), "", Some("s3cr3t"));
        let shown = masked_token(&command, Some("s3cr3t"));
        assert!(!shown.contains("s3cr3t"), "{shown}");
        assert!(shown.contains("--token \"••••••••••••\""), "{shown}");
        // Without a token there is nothing to hide: the placeholder shows.
        let placeholder = dial_in_command(Some("wss://box.tail.ts.net/ws"), "", None);
        assert_eq!(masked_token(&placeholder, None), placeholder);
    }

    #[test]
    fn the_guides_link_into_the_released_documentation() {
        for guide in GUIDES {
            assert!(guide.url().starts_with(DOCS), "{}", guide.title);
            assert!(!guide.page.starts_with('/'), "{}", guide.title);
        }
    }
}
