//! Operator alerts over Telegram. Best effort and bounded: a hung Telegram
//! API never stalls the maintenance pass, and a failed delivery is logged.

use std::time::Duration;

use futures_util::future::{Either, select};
use serde_json::json;
use worker::{AbortController, Delay, Fetch, Headers, Method, Request, RequestInit};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Telegram {
    bot_token: String,
    chat_id: String,
}

impl Telegram {
    /// Only when both are set.
    pub fn new(bot_token: Option<String>, chat_id: Option<String>) -> Option<Self> {
        match (bot_token, chat_id) {
            (Some(bot_token), Some(chat_id)) => Some(Self { bot_token, chat_id }),
            _ => None,
        }
    }

    pub async fn send(&self, message: &str) -> bool {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.bot_token);
        let payload = json!({"chat_id": self.chat_id, "text": message}).to_string();
        let attempt = async {
            let headers = Headers::new();
            headers.set("content-type", "application/json").ok()?;
            let mut init = RequestInit::new();
            init.with_method(Method::Post).with_headers(headers).with_body(Some(payload.into()));
            let request = Request::new_with_init(&url, &init).ok()?;
            let controller = AbortController::default();
            let signal = controller.signal();
            let fetch = Fetch::Request(request);
            let fetched = Box::pin(fetch.send_with_signal(&signal));
            let timer = Box::pin(Delay::from(TIMEOUT));
            match select(fetched, timer).await {
                Either::Left((Ok(r), _)) if r.status_code() < 300 => Some(()),
                Either::Left(_) => None,
                Either::Right(_) => {
                    controller.abort();
                    None
                }
            }
        };
        let ok = attempt.await.is_some();
        if !ok {
            worker::console_warn!("telegram alert delivery failed");
        }
        ok
    }
}
