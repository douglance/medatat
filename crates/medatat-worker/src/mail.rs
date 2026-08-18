//! Outbound mail, behind a trait.
//!
//! The `send_email` binding is the only implementation today; the seam exists so swapping
//! it for Postmark or SES is a one-file change (`docs/11-CRATE-GUIDE.md`).
//!
//! **The message body carries the code and nothing else.** No case identifier, no MRN, no
//! field value — nothing that would put clinical data into an unencrypted transport.

use crate::error::LogicResult;

/// A rendered message. Deliberately plain text: an HTML part is one more place for a
/// template variable to leak something it should not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailContent {
    pub subject: String,
    pub text: String,
}

/// The one message this system sends.
pub fn code_email(code: &str) -> EmailContent {
    EmailContent {
        subject: format!("{code} is your medatat sign-in code"),
        text: format!(
            "{code}\n\nThis code expires in 10 minutes and can be used once.\nIf you did not request it, ignore this message.\n"
        ),
    }
}

#[allow(async_fn_in_trait)]
pub trait Mailer {
    async fn send_code(&self, to: &str, code: &str) -> LogicResult<()>;
}

/// Captures what would have been sent. Used by tests; never compiled into a route.
#[derive(Debug, Default)]
pub struct MockMailer {
    sent: std::cell::RefCell<Vec<(String, EmailContent)>>,
}

impl MockMailer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn sent(&self) -> Vec<(String, EmailContent)> {
        self.sent.borrow().clone()
    }

    pub fn last_code(&self) -> Option<String> {
        self.sent
            .borrow()
            .last()
            .map(|(_, c)| c.text.lines().next().unwrap_or_default().to_string())
    }

    pub fn count(&self) -> usize {
        self.sent.borrow().len()
    }
}

impl Mailer for MockMailer {
    async fn send_code(&self, to: &str, code: &str) -> LogicResult<()> {
        self.sent
            .borrow_mut()
            .push((to.to_string(), code_email(code)));
        Ok(())
    }
}

/// The `send_email` binding declared as `[[send_email]] name = "EMAIL"`.
#[cfg(target_arch = "wasm32")]
pub struct BindingMailer {
    binding: worker::SendEmail,
    from: String,
    from_name: String,
}

#[cfg(target_arch = "wasm32")]
impl BindingMailer {
    pub fn new(
        binding: worker::SendEmail,
        from: impl Into<String>,
        from_name: impl Into<String>,
    ) -> Self {
        Self {
            binding,
            from: from.into(),
            from_name: from_name.into(),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl Mailer for BindingMailer {
    async fn send_code(&self, to: &str, code: &str) -> LogicResult<()> {
        use crate::error::LogicError;

        let content = code_email(code);
        let from = worker::EmailAddress::new(&self.from_name, &self.from);
        let message = worker::SendEmailBuilder::builder_with_email_address_and_str(
            &from,
            to,
            &content.subject,
        )
        .text(&content.text)
        .build();

        self.binding
            .send_with_builder(&message)
            .await
            .map(|_| ())
            .map_err(|e| LogicError::Internal(format!("send_email failed: {e:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::auth::{Account, RequestOutcome, handle_auth_request, hash_code};
    use chrono::{DateTime, Utc};
    use medatat_core::wire::{Role, UserInfo};

    /// The mailer future is always immediately ready, so a noop-waker poll is a complete
    /// executor for these tests and avoids pulling a runtime into a WASM crate.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        const VTABLE: RawWakerVTable = RawWakerVTable::new(
            |_| RawWaker::new(std::ptr::null(), &VTABLE),
            |_| {},
            |_| {},
            |_| {},
        );
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(v) = future.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-08-17T12:00:00Z".parse::<DateTime<Utc>>().unwrap()
    }

    #[test]
    fn mock_mailer_captures_the_sent_code() {
        let mailer = MockMailer::new();
        block_on(mailer.send_code("abstractor@example.com", "418902")).unwrap();

        assert_eq!(mailer.count(), 1);
        let (to, content) = &mailer.sent()[0];
        assert_eq!(to, "abstractor@example.com");
        assert!(content.subject.contains("418902"));
        assert!(content.text.contains("418902"));
        assert_eq!(mailer.last_code().as_deref(), Some("418902"));
    }

    #[test]
    fn the_issued_code_is_the_code_that_gets_mailed() {
        let account = Account {
            user: UserInfo {
                user_id: "u-1".into(),
                email: "abstractor@example.com".into(),
                display_name: "Abstractor".into(),
                role: Role::Abstractor,
            },
            is_active: true,
        };
        let RequestOutcome::Issue { code, record, .. } =
            handle_auth_request(Some(&account), now()).unwrap()
        else {
            panic!("expected Issue");
        };

        let mailer = MockMailer::new();
        block_on(mailer.send_code(&account.user.email, &code)).unwrap();

        let captured = mailer.last_code().expect("a code was mailed");
        assert_eq!(captured, code);
        assert_eq!(
            hash_code(&captured),
            record.code_hash,
            "the mailed code must be the one whose hash was stored"
        );
    }

    #[test]
    fn r1_the_email_body_carries_the_code_and_nothing_else() {
        let content = code_email("418902");
        let body = content.text.to_ascii_lowercase();
        for forbidden in ["case", "mrn", "patient", "http://", "https://", "@"] {
            assert!(
                !body.contains(forbidden),
                "sign-in email must not mention {forbidden:?}: {}",
                content.text
            );
        }
        assert_eq!(content.text.lines().next(), Some("418902"));
    }
}
