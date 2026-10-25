//! Browser login, the way Vesktop does it: Discord's own login page in a web view.
//!
//! Password, MFA, captcha and "new location" e-mail checks all run on
//! discord.com itself, so none of them has to be reimplemented here and the
//! captcha is solved on the origin it was issued for. Once the user lands in
//! the web app we read the token it keeps in `localStorage` and hand it to the
//! native client. The web view is then dropped, taking its WebKit process with it.
//!
//! The session is ephemeral: nothing from the page (cookies, storage) survives
//! the login. The token is the only thing kept, in `config`.
//!
//! WARNING: reading the token relies on how Discord's web client stores it,
//! which is undocumented and may change. `TOKEN_SCRIPT` tries the known
//! places in order; if all fail the user can still paste a token.

use std::{cell::Cell, rc::Rc, time::Duration};

use gtk::{gio, glib};
use webkit6::prelude::*;

const LOGIN_URL: &str = "https://discord.com/login";

/// How often to look for the token once the user is past the login form.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);

/// Evaluates to the raw token string, or "" when it is not there (yet).
///
/// Discord removes `window.localStorage`, so the storage of a fresh iframe is
/// used instead. The second branch scans the webpack module cache, which
/// still works if the token ever moves out of `localStorage`.
const TOKEN_SCRIPT: &str = r#"(() => {
  try {
    const frame = document.createElement("iframe");
    document.body.appendChild(frame);
    const raw = frame.contentWindow.localStorage.getItem("token");
    frame.remove();
    if (raw) return raw;
  } catch (_) {}
  try {
    let cache;
    window.webpackChunkdiscord_app.push([[Symbol()], {}, (r) => { cache = r.c; }]);
    for (const id in cache) {
      const getToken = cache[id]?.exports?.default?.getToken;
      if (typeof getToken === "function") {
        const token = getToken.call(cache[id].exports.default);
        if (token) return JSON.stringify(token);
      }
    }
  } catch (_) {}
  return "";
})()"#;

/// Creates the login web view. `on_token` runs once, on the main thread, with
/// the token. Dropping the returned widget (or reaching the token) stops polling.
pub fn new(on_token: impl Fn(String) + 'static) -> webkit6::WebView {
    let on_token: Rc<dyn Fn(String)> = Rc::new(on_token);
    let session = webkit6::NetworkSession::new_ephemeral();
    let web_view = webkit6::WebView::builder().network_session(&session).build();
    web_view.set_hexpand(true);
    web_view.set_vexpand(true);
    // A crashed web process leaves a blank, dead view; start over instead.
    web_view.connect_web_process_terminated(|view, reason| {
        if reason != webkit6::WebProcessTerminationReason::TerminatedByApi {
            view.load_uri(LOGIN_URL);
        }
    });
    web_view.load_uri(LOGIN_URL);

    let done = Rc::new(Cell::new(false));
    let weak = web_view.downgrade();
    glib::timeout_add_local(POLL_INTERVAL, move || {
        let Some(web_view) = weak.upgrade() else { return glib::ControlFlow::Break };
        if done.get() {
            return glib::ControlFlow::Break;
        }
        let (done, on_token) = (done.clone(), on_token.clone());
        web_view.evaluate_javascript(TOKEN_SCRIPT, None, None, gio::Cancellable::NONE, move |result| {
            let value = match result {
                Ok(value) => value,
                Err(_) => return,
            };
            if !value.is_string() || done.get() {
                return;
            }
            if let Some(token) = parse_token(&value.to_str()) {
                done.set(true);
                on_token(token);
            }
        });
        glib::ControlFlow::Continue
    });
    web_view
}

/// Parses `TOKEN_SCRIPT`'s output: usually a JSON string, e.g. `"mfa.abc"`.
pub fn parse_token(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let token = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_owned());
    (!token.is_empty()).then_some(token)
}

/// Ends the web view's process on purpose. Letting the view be destroyed
/// while its page is still alive makes WebKit abort the process on some
/// setups, which looks like a crash to the desktop even though login is done.
pub fn shut_down(web_view: &webkit6::WebView) {
    web_view.terminate_web_process();
}
