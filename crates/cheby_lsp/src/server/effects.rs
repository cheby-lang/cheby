//! Effects: messages the main loop sends on behalf of a step, applied in
//! order (plan §1.5). Only the main loop writes to the connection, so the
//! request queue stays consistent.

use lsp_server::{Message, Notification};
use lsp_types::MessageType;
use serde_json::{Value, json};

use super::{Outgoing, Server};

/// One message to send.
#[derive(Debug)]
pub(crate) enum Effect {
    /// A notification to the client.
    Notify { method: &'static str, params: Value },
    /// A request to the client whose answer is not used.
    Request { method: &'static str, params: Value },
}

impl Effect {
    /// A `window/logMessage`.
    pub(crate) fn log(level: MessageType, message: impl Into<String>) -> Self {
        Self::Notify {
            method: "window/logMessage",
            params: json!({ "type": level, "message": message.into() }),
        }
    }

    /// A `window/showMessage`.
    pub(crate) fn show(level: MessageType, message: impl Into<String>) -> Self {
        Self::Notify {
            method: "window/showMessage",
            params: json!({ "type": level, "message": message.into() }),
        }
    }
}

impl Server {
    /// Sends `effect`. A connection that is gone ends the main loop.
    pub(crate) fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Notify { method, params } => {
                self.send(Notification::new(method.to_owned(), params).into());
            }
            Effect::Request { method, params } => {
                self.request(method, params, Outgoing::Ignore);
            }
        }
    }

    /// Sends a request to the client, remembering what its answer is for.
    pub(crate) fn request(&mut self, method: &str, params: Value, purpose: Outgoing) {
        let request = self
            .queue
            .outgoing
            .register(method.to_owned(), params, purpose);
        self.send(Message::Request(request));
    }

    /// Sends one message. When the client has stopped reading, the loop
    /// ends at its next turn, as at the end of input (A §3.7).
    pub(crate) fn send(&mut self, message: Message) {
        if self.connection.sender.send(message).is_err() {
            self.closed = true;
        }
    }
}
