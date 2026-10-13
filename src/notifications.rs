//! Desktop notifications for completed top-level turns. All portal I/O stays
//! off the GPUI thread; actions are sent back to the client over a channel.

use std::{
    hash::{Hash, Hasher},
    time::Duration,
};

use ashpd::desktop::notification::{DisplayHint, Notification, NotificationProxy};
use async_channel::{Receiver, Sender};
use futures_util::{StreamExt, future::Either, future::select, pin_mut};

#[derive(Clone, Debug)]
pub struct DesktopNotifications {
    commands: Sender<NotificationCommand>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum NotificationCommand {
    Show { id: String, title: String },
    Withdraw(String),
}

pub fn notification_id(server: &str, session: &str) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    server.hash(&mut hash);
    session.hash(&mut hash);
    format!("opencode-gpui:{:016x}", hash.finish())
}

impl DesktopNotifications {
    /// An in-memory sink for headless tests; the live app uses `start`.
    pub fn channel() -> (Self, Receiver<NotificationCommand>) {
        let (commands, receiver) = async_channel::bounded(128);
        (Self { commands }, receiver)
    }

    pub fn start() -> (Self, Receiver<String>) {
        let (notifications, receiver) = Self::channel();
        let (actions, action_receiver) = async_channel::unbounded();
        if let Err(error) = std::thread::Builder::new()
            .name("opencode-notifications".into())
            .spawn(move || {
                let mut pending = None;
                while !receiver.is_closed() {
                    if let Err(error) = futures_lite::future::block_on(run(
                        receiver.clone(),
                        actions.clone(),
                        &mut pending,
                    )) {
                        log::warn!("desktop notifications unavailable; retrying: {error}");
                    }
                    if !receiver.is_closed() {
                        std::thread::sleep(Duration::from_secs(2));
                    }
                }
            })
        {
            log::warn!("desktop notifications worker unavailable: {error}");
        }
        (notifications, action_receiver)
    }

    pub fn show(&self, id: String, title: String) {
        if let Err(error) = self
            .commands
            .try_send(NotificationCommand::Show { id, title })
        {
            log::warn!("could not queue desktop notification: {error}");
        }
    }

    pub fn withdraw(&self, id: String) {
        if let Err(error) = self.commands.try_send(NotificationCommand::Withdraw(id)) {
            log::debug!("could not queue notification withdrawal: {error}");
        }
    }
}

async fn run(
    receiver: Receiver<NotificationCommand>,
    actions: Sender<String>,
    pending: &mut Option<NotificationCommand>,
) -> ashpd::Result<()> {
    let proxy = NotificationProxy::new().await?;
    let stream = proxy.receive_action_invoked().await?;
    pin_mut!(stream);
    loop {
        let next = if let Some(command) = pending.take() {
            Some(command)
        } else {
            let command = receiver.recv();
            let action = stream.next();
            pin_mut!(command, action);
            match select(command, action).await {
                Either::Left((Ok(command), _)) => Some(command),
                Either::Left((Err(_), _)) => break,
                Either::Right((Some(action), _)) => {
                    if action.name() == "open-session" {
                        let _ = actions.send(action.id().to_owned()).await;
                    }
                    None
                }
                Either::Right((None, _)) => break,
            }
        };
        match next {
            Some(NotificationCommand::Show { id, title }) => {
                let mut notification = Notification::new(&title)
                    .body("Session completed")
                    .default_action("open-session");
                if proxy.version() >= 2 {
                    notification = notification.display_hint([DisplayHint::HideOnLockScreen]);
                }
                if let Err(error) = proxy.add_notification(&id, notification).await {
                    *pending = Some(NotificationCommand::Show { id, title });
                    return Err(error);
                }
            }
            Some(NotificationCommand::Withdraw(id)) => {
                if let Err(error) = proxy.remove_notification(&id).await {
                    log::debug!("could not withdraw session notification: {error}");
                }
            }
            None => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::Duration,
    };

    use super::{DesktopNotifications, NotificationCommand, notification_id};

    struct TestPortal {
        records: mpsc::Sender<NotificationCommand>,
        fail_first_add: AtomicBool,
    }

    #[zbus::interface(name = "org.freedesktop.portal.Notification")]
    impl TestPortal {
        #[zbus(property)]
        fn version(&self) -> u32 {
            2
        }

        fn add_notification(
            &self,
            id: &str,
            mut notification: HashMap<String, zbus::zvariant::OwnedValue>,
        ) -> zbus::fdo::Result<()> {
            if self.fail_first_add.swap(false, Ordering::SeqCst) {
                return Err(zbus::fdo::Error::Failed("transient portal error".into()));
            }
            let title = notification
                .remove("title")
                .and_then(|value| String::try_from(value).ok())
                .expect("notification title");
            assert!(notification.contains_key("body"));
            assert!(notification.contains_key("default-action"));
            self.records
                .send(NotificationCommand::Show {
                    id: id.into(),
                    title,
                })
                .unwrap();
            Ok(())
        }

        fn remove_notification(&self, id: &str) {
            self.records
                .send(NotificationCommand::Withdraw(id.into()))
                .unwrap();
        }
    }

    #[test]
    fn notification_ids_are_stable_and_scoped_to_server_and_session() {
        let first = notification_id("https://one.example", "ses_1");
        assert_eq!(first, notification_id("https://one.example", "ses_1"));
        assert_ne!(first, notification_id("https://two.example", "ses_1"));
        assert_ne!(first, notification_id("https://one.example", "ses_2"));
        assert!(!first.contains("https://"));
    }

    /// Run only on an isolated session bus:
    /// `dbus-run-session -- cargo test --lib -- --ignored notification_portal_roundtrip`
    #[test]
    #[ignore = "requires an isolated D-Bus session; never use the host desktop bus"]
    fn notification_portal_roundtrip() {
        let (records_tx, records_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (emit_tx, emit_rx) = async_channel::unbounded::<String>();
        let service = std::thread::spawn(move || {
            futures_lite::future::block_on(async move {
                let connection = zbus::connection::Builder::session()
                    .unwrap()
                    .name("org.freedesktop.portal.Desktop")
                    .unwrap()
                    .serve_at(
                        "/org/freedesktop/portal/desktop",
                        TestPortal {
                            records: records_tx,
                            fail_first_add: AtomicBool::new(true),
                        },
                    )
                    .unwrap()
                    .build()
                    .await
                    .unwrap();
                ready_tx.send(()).unwrap();
                while let Ok(id) = emit_rx.recv().await {
                    connection
                        .emit_signal(
                            None::<&str>,
                            "/org/freedesktop/portal/desktop",
                            "org.freedesktop.portal.Notification",
                            "ActionInvoked",
                            &(id, "open-session", Vec::<zbus::zvariant::OwnedValue>::new()),
                        )
                        .await
                        .unwrap();
                }
            });
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (notifier, actions) = DesktopNotifications::start();
        notifier.show("notice-1".into(), "OpenCode".into());
        assert_eq!(
            records_rx.recv_timeout(Duration::from_secs(8)).unwrap(),
            NotificationCommand::Show {
                id: "notice-1".into(),
                title: "OpenCode".into(),
            }
        );
        let (clicked_tx, clicked_rx) = mpsc::channel();
        std::thread::spawn(move || clicked_tx.send(actions.recv_blocking()).unwrap());
        emit_tx.send_blocking("notice-1".into()).unwrap();
        assert_eq!(
            clicked_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            "notice-1"
        );
        notifier.withdraw("notice-1".into());
        assert_eq!(
            records_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            NotificationCommand::Withdraw("notice-1".into())
        );
        notifier.show("notice-1".into(), "OpenCode".into());
        assert_eq!(
            records_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            NotificationCommand::Show {
                id: "notice-1".into(),
                title: "OpenCode".into(),
            }
        );
        drop(emit_tx);
        service.join().unwrap();
    }
}
