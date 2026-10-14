//! Session-scoped background request bookkeeping, independent of GPUI.
use std::collections::HashMap;

#[derive(Default)]
pub struct Requests {
    next_id: u64,
    pending: HashMap<String, u64>,
    errors: HashMap<String, String>,
}

impl Requests {
    pub fn pending(&self, session: &str) -> bool {
        self.pending.contains_key(session)
    }

    pub fn error(&self, session: &str) -> Option<&str> {
        self.errors.get(session).map(String::as_str)
    }

    pub fn begin(&mut self, session: &str) -> Option<u64> {
        if session.is_empty() || self.pending(session) {
            return None;
        }
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.insert(session.to_owned(), self.next_id);
        self.errors.remove(session);
        Some(self.next_id)
    }

    /// Late responses after closing a tab, retrying, or changing servers do
    /// not settle another request. A 204 is not a tool-state transition.
    pub fn finish(&mut self, session: &str, request_id: u64, result: Result<(), String>) -> bool {
        if self.pending.get(session) != Some(&request_id) {
            return false;
        }
        self.pending.remove(session);
        if let Err(error) = result {
            self.errors.insert(session.to_owned(), error);
        }
        true
    }

    pub fn remove(&mut self, session: &str) {
        self.pending.remove(session);
        self.errors.remove(session);
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.errors.clear();
        // Keep the counter monotonic across connection changes.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_session_scoped_and_duplicate_safe() {
        let mut requests = Requests::default();
        assert_eq!(requests.begin(""), None);
        let a = requests.begin("ses_a").unwrap();
        assert_eq!(requests.begin("ses_a"), None);
        let b = requests.begin("ses_b").unwrap();
        assert!(requests.finish("ses_a", a, Ok(())));
        assert!(!requests.pending("ses_a"));
        assert!(requests.pending("ses_b"));
        assert!(requests.finish("ses_b", b, Err("unavailable".into())));
        assert_eq!(requests.error("ses_a"), None);
        assert_eq!(requests.error("ses_b"), Some("unavailable"));
        requests.begin("ses_b").unwrap();
        assert_eq!(
            requests.error("ses_b"),
            None,
            "retry clears the prior error"
        );
    }

    #[test]
    fn stale_replies_do_not_settle_retries_or_new_connections() {
        let mut requests = Requests::default();
        let old = requests.begin("ses_a").unwrap();
        requests.remove("ses_a");
        let retry = requests.begin("ses_a").unwrap();
        assert!(!requests.finish("ses_a", old, Err("old failure".into())));
        assert!(requests.pending("ses_a"));
        requests.clear();
        let current = requests.begin("ses_a").unwrap();
        assert!(current > retry);
        assert!(!requests.finish("ses_a", retry, Ok(())));
        assert!(requests.finish("ses_a", current, Ok(())));
        assert_eq!(requests.error("ses_a"), None);
    }
}
