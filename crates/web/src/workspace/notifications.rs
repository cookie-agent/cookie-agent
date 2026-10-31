//! Push notifications from the page (push notifications spec section 9):
//! Settings › Notifications' calls (`notifications.*`,
//! client protocol 1.30), kept fresh by `notifications.changed`, and what
//! the page tells the daemon it shows (`client.presence`). The view is
//! `crate::views::notifications`.

use cookie_agent_protocol::{NotificationsSubscribeParams, NotificationsTestStatus, SessionId};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{
    browser,
    push::{PUSH_DAEMON_URL_KEY, PushContext, device_label, presence, same_key},
};

/// Where the page keeps this browser's subscription id and label for a
/// daemon URL.
fn subscription_key(daemon_url: &str) -> String {
    format!("cookie.push_subscription.{daemon_url}")
}

/// How long forgetting a daemon waits for it to drop this browser's
/// subscription.
const RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Stops the daemon at `daemon_url` notifying this browser: the daemon
/// drops this browser's subscription, over `client` when there is one, and
/// the browser unsubscribes when its subscription is that daemon's.
///
/// The daemon's id for the subscription is the one `notifications.subscribe`
/// answers: registering the browser's subscription again keeps its id, so
/// it is found even when the page kept none (or kept it under another URL
/// for the daemon). Without a connection only the browser unsubscribes; the
/// daemon drops the subscription when its next push finds it gone.
async fn unregister(
    daemon_url: &str,
    client: Option<cookie_agent_protocol::Client>,
) -> Result<(), cookie_agent_protocol::ClientError> {
    let subscription = browser::push_subscription().await;
    let mut outcome = Ok(());
    let mut theirs = browser::load(PUSH_DAEMON_URL_KEY).as_deref() == Some(daemon_url);
    if let Some(client) = client {
        let mut id = browser::load(&subscription_key(daemon_url));
        if let (Ok(daemon), Some(subscription)) =
            (client.get_notifications().await, subscription.clone())
            && subscription
                .key
                .as_deref()
                .is_some_and(|key| same_key(key, &daemon.public_key))
        {
            theirs = true;
            let (agent, touches) = browser::user_agent();
            if let Ok(registered) = client
                .subscribe_notifications(NotificationsSubscribeParams {
                    endpoint: subscription.endpoint,
                    p256dh: subscription.p256dh,
                    auth: subscription.auth,
                    label: device_label(&agent, touches),
                })
                .await
            {
                id = Some(registered.subscription.id);
            }
        }
        if let Some(id) = id
            && let Err(error) = client.unsubscribe_notifications(id).await
            && error.error_code() != Some("push_subscription_not_found")
        {
            outcome = Err(error);
        }
    }
    browser::forget(&subscription_key(daemon_url));
    if theirs {
        if subscription.is_some() {
            browser::push_unsubscribe().await;
        }
        browser::forget(PUSH_DAEMON_URL_KEY);
    }
    outcome
}

/// Forgetting the daemon at `daemon_url` stops its notifications here
/// ([`unregister`]), waiting at most [`RELEASE_TIMEOUT`] for the daemon.
pub(crate) async fn release_push(
    daemon_url: String,
    client: Option<cookie_agent_protocol::Client>,
) {
    let release = std::pin::pin!(unregister(&daemon_url, client));
    let timeout = std::pin::pin!(gloo_timers::future::sleep(RELEASE_TIMEOUT));
    if let futures_util::future::Either::Right(_) =
        futures_util::future::select(release, timeout).await
    {
        // The daemon did not answer in time: this browser stops anyway.
        browser::forget(&subscription_key(&daemon_url));
        if browser::load(PUSH_DAEMON_URL_KEY).as_deref() == Some(daemon_url.as_str()) {
            browser::push_unsubscribe().await;
            browser::forget(PUSH_DAEMON_URL_KEY);
        }
    }
}

/// What the browser says about push now.
async fn read_push_context() -> PushContext {
    let (agent, touches) = browser::user_agent();
    let supported = browser::push_supported();
    PushContext {
        supported,
        secure: browser::is_secure_context(),
        ios: crate::install::is_ios(&agent, touches),
        standalone: browser::is_standalone(),
        permission: browser::notification_permission(),
        subscribed_key: if supported {
            browser::push_subscription()
                .await
                .and_then(|subscription| subscription.key)
        } else {
            None
        },
    }
}

impl Workspace {
    /// Reads `notifications.get` and what the browser says, when the
    /// daemon has push notifications.
    pub fn poll_notifications(self) {
        if !self.has_push.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        // A change's read and an older one overlap: only the newest lands.
        let read = self.begin_read(|reads| &mut reads.notifications);
        spawn_local(async move {
            let result = client
                .get_notifications()
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            let context = read_push_context().await;
            if self.read_is_current(|reads| &mut reads.notifications, read) {
                self.notifications.set(Some(result));
                self.push_context.set(context);
            }
        });
    }

    /// The daemon's push key, once `notifications.get` answered.
    pub fn push_key(self) -> Option<String> {
        self.notifications.with(|notifications| {
            notifications
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .map(|result| result.public_key.clone())
        })
    }

    /// The id of this browser's subscription at `daemon_url`, as the page
    /// kept it.
    pub fn own_subscription(self, daemon_url: &str) -> Option<String> {
        browser::load(&subscription_key(daemon_url))
    }

    /// **Turn on notifications** (and **Switch to this daemon**): asks for
    /// permission, subscribes this browser with the daemon's key, and
    /// registers it with the daemon at `daemon_url`. Call it from a click:
    /// browsers ask for permission only then.
    pub fn turn_on_notifications(self, daemon_url: String) {
        let Some(key) = self.push_key() else {
            return;
        };
        self.push_error.set(None);
        self.push_busy.set(true);
        spawn_local(async move {
            let outcome = async {
                if browser::request_notification_permission().await != "granted" {
                    return Err("Notifications were not allowed.".to_owned());
                }
                let subscription = browser::push_subscribe(&key).await?;
                let (agent, touches) = browser::user_agent();
                self.register_subscription(&daemon_url, subscription, device_label(&agent, touches))
                    .await
            }
            .await;
            self.push_busy.set(false);
            if let Err(error) = outcome {
                self.push_error.set(Some(error));
            }
            self.poll_notifications();
        });
    }

    /// Registers this browser's `subscription` with the daemon as `label`,
    /// and keeps its id and the daemon it belongs to.
    async fn register_subscription(
        self,
        daemon_url: &str,
        subscription: browser::BrowserPushSubscription,
        label: String,
    ) -> Result<(), String> {
        let client = self
            .client()
            .ok_or_else(|| "The page is reconnecting.".to_owned())?;
        let registered = client
            .subscribe_notifications(NotificationsSubscribeParams {
                endpoint: subscription.endpoint,
                p256dh: subscription.p256dh,
                auth: subscription.auth,
                label,
            })
            .await
            .map_err(|error| crate::errors::describe_full(&error))?;
        browser::store(&subscription_key(daemon_url), &registered.subscription.id);
        browser::store(PUSH_DAEMON_URL_KEY, daemon_url);
        Ok(())
    }

    /// Registers this browser's subscription again on a new connection to
    /// `daemon_url`, when it was made with the daemon's key: an endpoint
    /// the browser rotated, or a daemon that lost its file, catches up.
    pub(crate) fn resubscribe_push(self, daemon_url: String) {
        if !self.has_push.get_untracked() || !browser::push_supported() {
            return;
        }
        if browser::notification_permission() != "granted" {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let Ok(daemon) = client.get_notifications().await else {
                return;
            };
            let Some(subscription) = browser::push_subscription().await else {
                return;
            };
            if !subscription
                .key
                .as_deref()
                .is_some_and(|key| same_key(key, &daemon.public_key))
            {
                return;
            }
            let own = self.own_subscription(&daemon_url);
            let label = own
                .and_then(|id| daemon.subscriptions.iter().find(|shown| shown.id == id))
                .map(|shown| shown.label.clone())
                .unwrap_or_else(|| {
                    let (agent, touches) = browser::user_agent();
                    device_label(&agent, touches)
                });
            let _ = self
                .register_subscription(&daemon_url, subscription, label)
                .await;
        });
    }

    /// Renames this browser's subscription: registering it again keeps its
    /// id.
    pub fn rename_subscription(self, daemon_url: String, label: String) {
        let label = label.trim().to_owned();
        if label.is_empty() {
            return;
        }
        self.push_error.set(None);
        spawn_local(async move {
            let outcome = match browser::push_subscription().await {
                Some(subscription) => {
                    self.register_subscription(&daemon_url, subscription, label)
                        .await
                }
                None => Err("This browser has no subscription.".to_owned()),
            };
            if let Err(error) = outcome {
                self.push_error.set(Some(error));
            }
            self.poll_notifications();
        });
    }

    /// **Turn off**: removes this browser's subscription from the daemon at
    /// `daemon_url` and unsubscribes the browser, as forgetting the daemon
    /// does.
    pub fn turn_off_notifications(self, daemon_url: String) {
        self.push_error.set(None);
        self.push_busy.set(true);
        let client = self.client();
        spawn_local(async move {
            if let Err(error) = unregister(&daemon_url, client).await {
                self.push_error
                    .set(Some(crate::errors::describe_full(&error)));
            }
            self.push_busy.set(false);
            self.poll_notifications();
        });
    }

    /// Removes subscription `id` from the daemon: another device's, or one
    /// this browser no longer has.
    pub fn remove_subscription(self, id: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.push_error.set(None);
        spawn_local(async move {
            if let Err(error) = client.unsubscribe_notifications(id).await {
                self.push_error
                    .set(Some(crate::errors::describe_full(&error)));
            }
            self.poll_notifications();
        });
    }

    /// **Send test**: a test push to subscription `id`, and what came of it.
    pub fn test_subscription(self, id: String) {
        let Some(client) = self.client() else {
            return;
        };
        self.push_error.set(None);
        self.push_test.set(None);
        spawn_local(async move {
            let said = match client.test_notifications(id).await {
                Ok(result) => match result.status {
                    NotificationsTestStatus::Sent => {
                        "Sent: it should show on that device in a moment.".to_owned()
                    }
                    NotificationsTestStatus::Gone => {
                        "That subscription no longer exists; the daemon removed it.".to_owned()
                    }
                    _ => format!(
                        "The push service refused it: {}",
                        result.detail.unwrap_or_default()
                    ),
                },
                Err(error) => crate::errors::describe_full(&error),
            };
            self.push_test.set(Some(said));
            self.poll_notifications();
        });
    }

    /// Tells the daemon what the page shows the user now, when that
    /// changed: the viewed session while the page is visible, focused, and
    /// used lately. A shown tree's notifications are closed.
    pub fn report_presence(self) {
        if !self.has_push.get_untracked() {
            return;
        }
        let idle = browser::now_ms() - self.last_input.get_value();
        let viewed = if self.new_session.get_untracked() {
            None
        } else {
            self.selected.get_untracked()
        };
        let shown = presence(viewed, browser::page_attended(), true, idle);
        if self.presence_sent.get_value() == Some(shown) {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        self.presence_sent.set_value(Some(shown));
        if let Some(session) = shown {
            let tag = format!("session:{}", self.root_of(session));
            spawn_local(async move { browser::close_notifications(&tag).await });
        }
        spawn_local(async move {
            let _ = client.set_presence(shown).await;
        });
    }

    /// Notes input in the page: the user is there.
    pub fn note_input(self) {
        let was_idle =
            browser::now_ms() - self.last_input.get_value() >= crate::push::IDLE_AFTER_MS;
        self.last_input.set_value(browser::now_ms());
        if was_idle {
            self.report_presence();
        }
    }

    /// Opens session `id` from a notification, by its id as text.
    pub fn open_notified_session(self, id: &str) -> bool {
        match id.parse::<SessionId>() {
            Ok(session_id) => {
                self.open_session(session_id);
                true
            }
            Err(_) => false,
        }
    }
}
