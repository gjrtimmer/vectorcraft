//! `fetch` for remote storage: same origin only, never cached, and with a time limit. A request
//! that runs out of time is aborted and fails with a clear message instead of hanging.

use std::cell::Cell;
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use wasm_bindgen_futures::JsFuture;

use crate::remote_dav;
use crate::remote_util::{timeout_for, timeout_message};

pub(crate) fn js_err(e: wasm_bindgen::JsValue) -> String {
    format!("{e:?}")
}

/// The timer that aborts a request; cleared when the answer (body included) is done with.
struct Deadline {
    win: web_sys::Window,
    handle: i32,
    fired: Rc<Cell<bool>>,
    _cb: Closure<dyn FnMut()>,
}

impl Deadline {
    fn arm(ctl: &web_sys::AbortController, limit: Duration) -> Result<Deadline, String> {
        let win = web_sys::window().ok_or("no window")?;
        let fired = Rc::new(Cell::new(false));
        let (flag, ctl) = (fired.clone(), ctl.clone());
        let cb = Closure::<dyn FnMut()>::new(move || {
            flag.set(true);
            ctl.abort();
        });
        let ms = i32::try_from(limit.as_millis()).unwrap_or(i32::MAX);
        let handle = win.set_timeout_with_callback_and_timeout_and_arguments_0(cb.as_ref().unchecked_ref(), ms).map_err(js_err)?;
        Ok(Deadline { win, handle, fired, _cb: cb })
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.win.clear_timeout_with_handle(self.handle);
    }
}

/// An answer whose body read is still under the request's time limit.
pub(crate) struct Resp {
    r: web_sys::Response,
    deadline: Deadline,
    method: String,
    limit: Duration,
}

impl Deref for Resp {
    type Target = web_sys::Response;
    fn deref(&self) -> &web_sys::Response {
        &self.r
    }
}

/// `e` as an error message: the time limit's when that is what aborted the request.
fn failure(fired: &Cell<bool>, method: &str, limit: Duration, e: wasm_bindgen::JsValue) -> String {
    if fired.get() { timeout_message(method, limit) } else { js_err(e) }
}

/// A `fetch` that stays on this origin and always asks the server (the service worker leaves
/// `no-cache` requests alone). Limited to [`timeout_for`] the method.
pub(crate) async fn fetch(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<Resp, String> {
    fetch_within(method, url, headers, body, timeout_for(method)).await
}

pub(crate) async fn fetch_within(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>, limit: Duration) -> Result<Resp, String> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    // Same origin only: a redirect elsewhere fails instead of leaking the request.
    init.set_mode(web_sys::RequestMode::SameOrigin);
    init.set_cache(web_sys::RequestCache::NoCache);
    let h = web_sys::Headers::new().map_err(js_err)?;
    for (k, v) in headers {
        h.set(k, v).map_err(js_err)?;
    }
    init.set_headers(&h);
    if let Some(b) = body {
        init.set_body(&js_sys::Uint8Array::from(b));
    }
    let ctl = web_sys::AbortController::new().map_err(js_err)?;
    init.set_signal(Some(&ctl.signal()));
    let req = web_sys::Request::new_with_str_and_init(url, &init).map_err(js_err)?;
    let win = web_sys::window().ok_or("no window")?;
    let deadline = Deadline::arm(&ctl, limit)?;
    let sent = JsFuture::from(win.fetch_with_request(&req)).await;
    let resp = match sent {
        Ok(v) => v,
        Err(e) => return Err(failure(&deadline.fired, method, limit, e)),
    };
    let r = resp.dyn_into::<web_sys::Response>().map_err(|_| "fetch did not return a Response".to_string())?;
    Ok(Resp { r, deadline, method: method.to_string(), limit })
}

pub(crate) fn header(r: &web_sys::Response, name: &str) -> Option<String> {
    r.headers().get(name).ok().flatten()
}

/// The body, refused before reading when `Content-Length` exceeds `cap` and after when it does.
/// Reading it is still bound by the request's time limit.
pub(crate) async fn read_body(r: &Resp, cap: usize) -> Result<Vec<u8>, String> {
    if header(r, "content-length").and_then(|h| remote_dav::parse_content_length(&h)).is_some_and(|n| n > cap as u64) {
        return Err("the file is too large".into());
    }
    let fail = |e| failure(&r.deadline.fired, &r.method, r.limit, e);
    let promise = r.array_buffer().map_err(fail)?;
    let buf = JsFuture::from(promise).await.map_err(fail)?;
    let a = js_sys::Uint8Array::new(&buf);
    if a.length() as usize > cap {
        return Err("the file is too large".into());
    }
    Ok(a.to_vec())
}
