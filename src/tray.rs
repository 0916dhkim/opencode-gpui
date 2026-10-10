//! Sending while a session runs (steer / queue) and the tray of waiting
//! prompts above the composer.
//!
//! Server behavior (2.0.8, verified on the harness): a steered prompt joins
//! the running turn at its next step; a queued one starts a new turn once
//! the turn ends. `POST /interrupt` without `resume` parks every waiting
//! item. On a parked session, switching an item to steer wakes the session,
//! which then delivers every parked steer at once and afterwards runs the
//! parked queue one turn at a time; a new prompt wakes it the same way (it
//! joins the parked steers). `POST /interrupt?resume=true` wakes it for the
//! parked steers only: queued items stay parked, so the tray never uses it.

use std::collections::HashSet;

use crate::{
    api::{InboxRequest, Settled},
    model::TrayItem,
    protocol::Delivery,
};

/// What a send does, from the composer's buttons or Enter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendMode {
    /// Idle session: a plain prompt (server default delivery).
    Send,
    /// Running session: into the current run at its next step.
    Steer,
    /// Running session: a new turn once the run ends.
    Queue,
}

impl SendMode {
    /// The prompt body's `delivery`: only a queued prompt carries one.
    pub fn delivery(self) -> Option<Delivery> {
        (self == Self::Queue).then_some(Delivery::Queue)
    }
}

/// Enter without Shift (Shift+Enter is a newline). Idle sessions send, with
/// or without Ctrl; while running, Enter steers and Ctrl+Enter queues.
pub fn enter_mode(busy: bool, ctrl: bool) -> SendMode {
    match (busy, ctrl) {
        (false, _) => SendMode::Send,
        (true, false) => SendMode::Steer,
        (true, true) => SendMode::Queue,
    }
}

fn is_queue(delivery: Delivery) -> bool {
    delivery == Delivery::Queue
}

/// The other mode, for a row's switch button.
pub fn switched(delivery: Delivery) -> Delivery {
    if is_queue(delivery) {
        Delivery::Steer
    } else {
        Delivery::Queue
    }
}

/// A tray row's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAction {
    Switch,
    Cancel,
}

/// Whether a row shows its switch button. While paused a queued row has
/// none: switching it to steer would wake the session and run everything.
/// A steered row can still be queued (that wakes nothing).
pub fn shows_switch(delivery: Delivery, paused: bool) -> bool {
    !paused || !is_queue(delivery)
}

/// The request for a row action, or `None` while the row cannot act (its
/// prompt is still being sent, or a request for it is in flight) or the
/// action is not offered.
pub fn row_request(row: &TrayRow, action: RowAction, paused: bool) -> Option<InboxRequest> {
    if row.sending || row.in_flight {
        return None;
    }
    match action {
        RowAction::Switch if shows_switch(row.delivery, paused) => {
            Some(InboxRequest::SetDelivery(switched(row.delivery)))
        }
        RowAction::Switch => None,
        RowAction::Cancel => Some(InboxRequest::Cancel),
    }
}

/// Resume on a paused tray: the item to act on and the request that wakes
/// the session for every parked message ([`InboxRequest::Resume`]). With a
/// parked steer, that steer is bounced (queue, then steer again); with only
/// queued items, the first one is steered, so it runs as the next turn and
/// the rest follow. `None` while any row is busy (a send or another tray
/// request, including an earlier Resume, is in flight).
pub fn resume_request(rows: &[TrayRow]) -> Option<(String, InboxRequest)> {
    if rows.is_empty() || rows.iter().any(|row| row.sending || row.in_flight) {
        return None;
    }
    let target = rows
        .iter()
        .find(|row| !is_queue(row.delivery))
        .or_else(|| rows.first())?;
    let delivery = if is_queue(target.delivery) {
        Delivery::Queue
    } else {
        Delivery::Steer
    };
    Some((target.id.clone(), InboxRequest::Resume(delivery)))
}

/// What a tray request's answer means for the client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settlement {
    /// Accepted; the inbox events move the row.
    Done,
    /// The item was no longer waiting in that mode (409/404: delivered,
    /// cancelled or switched meanwhile): resolved, reload the inbox.
    Reconcile,
    /// Any other failure: show it and keep the row.
    Failed(String),
}

pub fn settlement(request: InboxRequest, result: Result<Settled, String>) -> Settlement {
    match result {
        Ok(Settled::Done) => Settlement::Done,
        Ok(Settled::AlreadyResolved) => Settlement::Reconcile,
        Err(error) => Settlement::Failed(match request {
            InboxRequest::SetDelivery(_) => {
                format!("Could not switch the waiting message: {error}")
            }
            InboxRequest::Cancel => format!("Could not cancel the waiting message: {error}"),
            InboxRequest::Resume(_) => format!("Could not resume the waiting messages: {error}"),
        }),
    }
}

/// "3 waiting" while the session runs, "Paused · 3 waiting" once it is idle
/// with items left (after Stop, or when a queued item waits behind nothing).
pub fn header_text(count: usize, paused: bool) -> String {
    if paused {
        format!("Paused · {count} waiting")
    } else {
        format!("{count} waiting")
    }
}

/// Resume's tooltip: what runs, in which order.
pub fn resume_tooltip(steers: usize, queued: usize) -> String {
    let total = steers + queued;
    let all = if total == 1 {
        "Runs the waiting message".to_owned()
    } else {
        format!("Runs all {total} waiting messages")
    };
    match (steers, queued) {
        (_, 0) if total == 1 => format!("{all} in the next turn."),
        (_, 0) => format!("{all} together in the next turn."),
        (0, 1) => format!("{all} as the next turn."),
        (0, _) => format!("{all}, each as its own turn."),
        (steers, queued) => format!(
            "{all}: the steered {} first, then {} as its own turn.",
            if steers == 1 { "one" } else { "ones" },
            if queued == 1 {
                "the queued one"
            } else {
                "each queued one"
            },
        ),
    }
}

/// The line above the composer while the tray is paused and the draft has
/// input: sending wakes the session, so the parked messages run too.
/// `paused_count` is 0 unless the tray is paused.
pub fn resume_warning(paused_count: usize, has_input: bool) -> Option<String> {
    if paused_count == 0 || !has_input {
        return None;
    }
    Some(if paused_count == 1 {
        "Sending also resumes the paused message — your message joins the next turn.".to_owned()
    } else {
        format!(
            "Sending also resumes the {paused_count} paused messages — your message joins the next turn."
        )
    })
}

pub fn badge_text(delivery: Delivery) -> &'static str {
    if is_queue(delivery) {
        "⏸ QUEUE"
    } else {
        "↪ STEER"
    }
}

pub fn switch_label(delivery: Delivery) -> &'static str {
    if is_queue(switched(delivery)) {
        "→ Queue"
    } else {
        "→ Steer"
    }
}

pub fn switch_tooltip(delivery: Delivery, paused: bool) -> &'static str {
    match (is_queue(switched(delivery)), paused) {
        (true, false) => "Queue it instead: sent as a new turn once this run finishes",
        (true, true) => "Queue it instead: it runs as its own turn after the steered messages",
        (false, _) => "Steer it instead: the agent reads it at its next step",
    }
}

/// One line for a tray row: the first non-blank line of the text, plus
/// "· N attachments".
pub fn summary(text: &str, attachments: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let more = text.lines().filter(|line| !line.trim().is_empty()).count() > 1;
    let mut summary = if more {
        format!("{line} …")
    } else {
        line.to_owned()
    };
    if attachments > 0 {
        let files = format!(
            "{attachments} attachment{}",
            if attachments == 1 { "" } else { "s" }
        );
        summary = if summary.is_empty() {
            files
        } else {
            format!("{summary} · {files}")
        };
    }
    summary
}

/// A prompt sent while the session runs, shown in the tray before the
/// server echoes it (`session.inbox.enqueued`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingEntry {
    pub id: String,
    pub delivery: Delivery,
    pub summary: String,
    /// The POST has been answered; the echo is still on its way.
    pub accepted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayRow {
    pub id: String,
    pub delivery: Delivery,
    pub summary: String,
    /// Not in the server's inbox yet (its POST is in flight).
    pub sending: bool,
    /// A switch/cancel/resume request for it is in flight.
    pub in_flight: bool,
}

/// The tray: the session's waiting prompts, oldest first, then a prompt
/// being sent into the run. `hidden` is a prompt still shown as the
/// optimistic transcript row (sent while idle).
pub fn tray_rows(
    items: &[TrayItem],
    hidden: Option<&str>,
    pending: Option<&PendingEntry>,
    in_flight: &HashSet<String>,
) -> Vec<TrayRow> {
    let mut rows: Vec<TrayRow> = items
        .iter()
        .filter(|item| Some(item.id.as_str()) != hidden)
        .map(|item| TrayRow {
            id: item.id.clone(),
            delivery: item.delivery,
            summary: summary(&item.text, item.attachments),
            sending: item.optimistic,
            in_flight: item.optimistic || in_flight.contains(&item.id),
        })
        .collect();
    if let Some(pending) = pending
        && !items.iter().any(|item| item.id == pending.id)
    {
        rows.push(TrayRow {
            id: pending.id.clone(),
            delivery: pending.delivery,
            summary: pending.summary.clone(),
            sending: !pending.accepted,
            in_flight: true,
        });
    }
    rows
}

/// Rows that run together, under a label saying when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayGroup {
    pub label: String,
    pub rows: Vec<TrayRow>,
}

/// A group's label from its turn number.
///
/// Turns are numbered from the one that takes the steered messages:
/// - while running, that is the current run, turn 1 ("THIS RUN · AT ITS
///   NEXT STEP"); the k-th queued message is turn k + 1, the first of them
///   "AFTER THIS RUN · TURN 2";
/// - while paused, it is the next turn, turn 1 ("NEXT TURN"). Without
///   parked steers the first queued message is that next turn; otherwise the
///   k-th queued message is turn k + 1. Later turns read "TURN n".
pub fn turn_label(turn: usize, steers: bool, running: bool) -> String {
    match (running, steers, turn) {
        (true, true, _) => "THIS RUN · AT ITS NEXT STEP".to_owned(),
        (true, false, 2) => "AFTER THIS RUN · TURN 2".to_owned(),
        (false, _, 1) => "NEXT TURN".to_owned(),
        _ => format!("TURN {turn}"),
    }
}

/// The rows in run order: every steered message in one group (they run
/// together), then each queued message as its own group, keeping the tray's
/// order within each kind. See [`turn_label`] for the numbering.
pub fn tray_groups(rows: &[TrayRow], running: bool) -> Vec<TrayGroup> {
    let (queued, steers): (Vec<&TrayRow>, Vec<&TrayRow>) =
        rows.iter().partition(|row| is_queue(row.delivery));
    let mut groups = Vec::new();
    if !steers.is_empty() {
        groups.push(TrayGroup {
            label: turn_label(1, true, running),
            rows: steers.into_iter().cloned().collect(),
        });
    }
    // Turn 1 is the current run while running, or the steers' next turn.
    let first = if running || !groups.is_empty() { 2 } else { 1 };
    for (index, row) in queued.into_iter().enumerate() {
        groups.push(TrayGroup {
            label: turn_label(first + index, false, running),
            rows: vec![row.clone()],
        });
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, delivery: Delivery, text: &str, attachments: usize) -> TrayItem {
        TrayItem {
            id: id.into(),
            delivery,
            text: text.into(),
            attachments,
            optimistic: false,
        }
    }

    fn row(id: &str, delivery: Delivery) -> TrayRow {
        TrayRow {
            id: id.into(),
            delivery,
            summary: id.into(),
            sending: false,
            in_flight: false,
        }
    }

    fn view(groups: &[TrayGroup]) -> Vec<(String, Vec<&str>)> {
        groups
            .iter()
            .map(|group| {
                (
                    group.label.clone(),
                    group.rows.iter().map(|row| row.id.as_str()).collect(),
                )
            })
            .collect()
    }

    fn owned(expected: &[(&str, &[&'static str])]) -> Vec<(String, Vec<&'static str>)> {
        expected
            .iter()
            .map(|(label, ids)| ((*label).to_owned(), ids.to_vec()))
            .collect()
    }

    #[test]
    fn enter_sends_when_idle_and_steers_or_queues_while_running() {
        assert_eq!(enter_mode(false, false), SendMode::Send);
        assert_eq!(
            enter_mode(false, true),
            SendMode::Send,
            "Ctrl+Enter = Enter"
        );
        assert_eq!(enter_mode(true, false), SendMode::Steer);
        assert_eq!(enter_mode(true, true), SendMode::Queue);
        assert_eq!(SendMode::Send.delivery(), None);
        assert_eq!(SendMode::Steer.delivery(), None, "steer omits delivery");
        assert_eq!(SendMode::Queue.delivery(), Some(Delivery::Queue));
    }

    #[test]
    fn groups_run_steers_together_then_each_queued_message() {
        // Server order interleaves kinds; the tray shows run order.
        let rows = [
            row("q1", Delivery::Queue),
            row("s1", Delivery::Steer),
            row("q2", Delivery::Queue),
            row("s2", Delivery::Steer),
            row("q3", Delivery::Queue),
        ];
        assert_eq!(
            view(&tray_groups(&rows, true)),
            owned(&[
                ("THIS RUN · AT ITS NEXT STEP", &["s1", "s2"]),
                ("AFTER THIS RUN · TURN 2", &["q1"]),
                ("TURN 3", &["q2"]),
                ("TURN 4", &["q3"]),
            ])
        );
        assert_eq!(
            view(&tray_groups(&rows, false)),
            owned(&[
                ("NEXT TURN", &["s1", "s2"]),
                ("TURN 2", &["q1"]),
                ("TURN 3", &["q2"]),
                ("TURN 4", &["q3"]),
            ])
        );
    }

    #[test]
    fn without_steers_the_first_queued_message_is_the_next_turn() {
        let rows = [row("q1", Delivery::Queue), row("q2", Delivery::Queue)];
        assert_eq!(
            view(&tray_groups(&rows, true)),
            owned(&[("AFTER THIS RUN · TURN 2", &["q1"]), ("TURN 3", &["q2"])])
        );
        assert_eq!(
            view(&tray_groups(&rows, false)),
            owned(&[("NEXT TURN", &["q1"]), ("TURN 2", &["q2"])])
        );
        let steers = [row("s1", Delivery::Steer)];
        assert_eq!(
            view(&tray_groups(&steers, true)),
            owned(&[("THIS RUN · AT ITS NEXT STEP", &["s1"])])
        );
        assert!(tray_groups(&[], false).is_empty());
    }

    #[test]
    fn turn_labels_follow_the_numbering() {
        assert_eq!(turn_label(1, true, true), "THIS RUN · AT ITS NEXT STEP");
        assert_eq!(turn_label(2, false, true), "AFTER THIS RUN · TURN 2");
        assert_eq!(turn_label(3, false, true), "TURN 3");
        assert_eq!(turn_label(1, true, false), "NEXT TURN");
        assert_eq!(turn_label(1, false, false), "NEXT TURN");
        assert_eq!(turn_label(2, false, false), "TURN 2");
    }

    #[test]
    fn paused_rows_offer_cancel_and_only_steered_rows_can_be_queued() {
        let steer = row("s", Delivery::Steer);
        let queue = row("q", Delivery::Queue);
        // Running: switch both ways, and cancel.
        assert!(shows_switch(Delivery::Steer, false) && shows_switch(Delivery::Queue, false));
        assert_eq!(
            row_request(&steer, RowAction::Switch, false),
            Some(InboxRequest::SetDelivery(Delivery::Queue))
        );
        assert_eq!(
            row_request(&queue, RowAction::Switch, false),
            Some(InboxRequest::SetDelivery(Delivery::Steer))
        );
        // Paused: "→ Steer" would wake the session, so it is not offered.
        assert!(shows_switch(Delivery::Steer, true));
        assert!(!shows_switch(Delivery::Queue, true));
        assert_eq!(
            row_request(&steer, RowAction::Switch, true),
            Some(InboxRequest::SetDelivery(Delivery::Queue))
        );
        assert_eq!(row_request(&queue, RowAction::Switch, true), None);
        for paused in [false, true] {
            assert_eq!(
                row_request(&queue, RowAction::Cancel, paused),
                Some(InboxRequest::Cancel)
            );
        }
        // Busy rows cannot act.
        let busy = TrayRow {
            in_flight: true,
            ..steer.clone()
        };
        assert_eq!(row_request(&busy, RowAction::Cancel, false), None);
        let sending = TrayRow {
            sending: true,
            in_flight: true,
            ..steer
        };
        assert_eq!(row_request(&sending, RowAction::Switch, false), None);
    }

    #[test]
    fn resume_bounces_a_parked_steer_or_steers_the_first_queued_message() {
        let rows = [
            row("q1", Delivery::Queue),
            row("s1", Delivery::Steer),
            row("s2", Delivery::Steer),
        ];
        assert_eq!(
            resume_request(&rows),
            Some(("s1".to_owned(), InboxRequest::Resume(Delivery::Steer)))
        );
        let queued = [row("q1", Delivery::Queue), row("q2", Delivery::Queue)];
        assert_eq!(
            resume_request(&queued),
            Some(("q1".to_owned(), InboxRequest::Resume(Delivery::Queue)))
        );
        assert_eq!(resume_request(&[]), None);
        // Disabled while a request (e.g. this Resume) is in flight.
        let mut busy = rows.to_vec();
        busy[0].in_flight = true;
        assert_eq!(resume_request(&busy), None);
    }

    #[test]
    fn conflicts_reconcile_and_other_errors_keep_the_row() {
        let steer = InboxRequest::SetDelivery(Delivery::Steer);
        assert_eq!(settlement(steer, Ok(Settled::Done)), Settlement::Done);
        assert_eq!(
            settlement(InboxRequest::Cancel, Ok(Settled::AlreadyResolved)),
            Settlement::Reconcile
        );
        assert_eq!(
            settlement(steer, Ok(Settled::AlreadyResolved)),
            Settlement::Reconcile
        );
        assert_eq!(
            settlement(
                InboxRequest::Resume(Delivery::Steer),
                Ok(Settled::AlreadyResolved)
            ),
            Settlement::Reconcile
        );
        assert_eq!(
            settlement(
                InboxRequest::Resume(Delivery::Queue),
                Err("server returned 500".into())
            ),
            Settlement::Failed("Could not resume the waiting messages: server returned 500".into())
        );
    }

    #[test]
    fn headers_and_tooltips_say_what_runs() {
        assert_eq!(header_text(2, false), "2 waiting");
        assert_eq!(header_text(3, true), "Paused · 3 waiting");
        assert_eq!(
            resume_tooltip(1, 2),
            "Runs all 3 waiting messages: the steered one first, then each queued one as its own turn."
        );
        assert_eq!(
            resume_tooltip(2, 1),
            "Runs all 3 waiting messages: the steered ones first, then the queued one as its own turn."
        );
        assert_eq!(
            resume_tooltip(1, 0),
            "Runs the waiting message in the next turn."
        );
        assert_eq!(
            resume_tooltip(2, 0),
            "Runs all 2 waiting messages together in the next turn."
        );
        assert_eq!(
            resume_tooltip(0, 1),
            "Runs the waiting message as the next turn."
        );
        assert_eq!(
            resume_tooltip(0, 3),
            "Runs all 3 waiting messages, each as its own turn."
        );
    }

    #[test]
    fn the_composer_warns_only_when_paused_with_input() {
        assert_eq!(resume_warning(0, true), None, "not paused");
        assert_eq!(resume_warning(3, false), None, "nothing typed");
        assert_eq!(
            resume_warning(3, true).as_deref(),
            Some("Sending also resumes the 3 paused messages — your message joins the next turn.")
        );
        assert_eq!(
            resume_warning(1, true).as_deref(),
            Some("Sending also resumes the paused message — your message joins the next turn.")
        );
    }

    #[test]
    fn labels_follow_the_mockup() {
        assert_eq!(badge_text(Delivery::Steer), "↪ STEER");
        assert_eq!(badge_text(Delivery::Queue), "⏸ QUEUE");
        assert_eq!(switch_label(Delivery::Steer), "→ Queue");
        assert_eq!(switch_label(Delivery::Queue), "→ Steer");
    }

    #[test]
    fn summaries_are_one_line_with_an_attachment_count() {
        assert_eq!(summary("  hello  ", 0), "hello");
        assert_eq!(summary("\nfirst\n\nsecond", 0), "first …");
        assert_eq!(summary("look", 1), "look · 1 attachment");
        assert_eq!(summary("look", 3), "look · 3 attachments");
        assert_eq!(summary("", 2), "2 attachments");
    }

    #[test]
    fn rows_list_items_oldest_first_then_the_prompt_being_sent() {
        let items = [
            item("msg_s", Delivery::Steer, "and keep the cap", 0),
            item("msg_q", Delivery::Queue, "then the changelog", 1),
            item("msg_idle", Delivery::Steer, "sent while idle", 0),
        ];
        let in_flight = HashSet::from(["msg_q".to_owned()]);
        let pending = PendingEntry {
            id: "msg_new".into(),
            delivery: Delivery::Queue,
            summary: "later".into(),
            accepted: false,
        };
        let rows = tray_rows(&items, Some("msg_idle"), Some(&pending), &in_flight);
        let view: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.id.as_str(),
                    row.summary.as_str(),
                    row.sending,
                    row.in_flight,
                )
            })
            .collect();
        assert_eq!(
            view,
            [
                ("msg_s", "and keep the cap", false, false),
                ("msg_q", "then the changelog · 1 attachment", false, true),
                ("msg_new", "later", true, true),
            ]
        );

        // Once echoed, the server's item replaces the pending entry.
        let echoed = [item("msg_new", Delivery::Queue, "later", 0)];
        let rows = tray_rows(&echoed, None, Some(&pending), &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].sending && !rows[0].in_flight);

        // Accepted but not echoed yet: no longer "sending", still inert.
        let accepted = PendingEntry {
            accepted: true,
            ..pending
        };
        let rows = tray_rows(&[], None, Some(&accepted), &HashSet::new());
        assert!(!rows[0].sending && rows[0].in_flight);
    }

    #[test]
    fn optimistic_tray_row_cannot_act_until_the_server_confirms_it() {
        let mut item = item("msg_new", Delivery::Queue, "later", 0);
        item.optimistic = true;
        let rows = tray_rows(&[item.clone()], None, None, &HashSet::new());
        assert!(rows[0].sending && rows[0].in_flight);
        assert_eq!(row_request(&rows[0], RowAction::Cancel, false), None);
        item.optimistic = false;
        let rows = tray_rows(&[item], None, None, &HashSet::new());
        assert!(!rows[0].sending && !rows[0].in_flight);
        assert_eq!(
            row_request(&rows[0], RowAction::Cancel, false),
            Some(InboxRequest::Cancel)
        );
    }
}
