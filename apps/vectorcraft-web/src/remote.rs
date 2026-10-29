//! Remote storage in the browser: files on a same-origin WebDAV server appear as mounts
//! (`./vectorcraft-config.json`, see [`crate::remote_config`]). Listings are PROPFIND requests,
//! files are read with `fetch`, saves are uploaded by [`crate::remote_upload`]. The pure parts live
//! in [`crate::remote_config`], [`crate::remote_dav`] and [`crate::remote_write`]; this module is
//! the browser side, and hands the app a [`RemoteStorage`] (`Services::remote`).
//!
//! The app's callbacks are synchronous, so a listing starts on the first look and answers
//! `WouldBlock` ("Loading…") until it arrives; the dialog asks again on the next frames.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;

use vectorcraft_ui_egui::remote::{Purpose, RemoteEntry, RemoteEvent, RemoteInbox, RemoteMount, RemoteStorage};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::remote_config::{self, Config};
use crate::remote_dav::{self, Child};

/// A failed listing is not asked for again for this long (seconds).
const ERROR_RETRY_S: f64 = 5.0;
/// Largest file opened or placed from the server (bytes).
const MAX_FILE_BYTES: usize = 512 << 20;
/// Largest config file read.
const CONFIG_MAX: usize = 64 * 1024;

#[derive(Default)]
struct DirState {
    entries: Option<Vec<RemoteEntry>>,
    fetched: f64,
    /// Whether the listing was handed out since it was fetched (a TTL of 0 refetches after that).
    shown: bool,
    loading: bool,
    error: Option<(String, f64)>,
}

#[derive(Default)]
struct State {
    cfg: Option<Config>,
    origin: String,
    dirs: HashMap<String, DirState>,
    ctx: Option<egui::Context>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn now_s() -> f64 {
    js_sys::Date::now() / 1000.0
}

fn repaint() {
    if let Some(ctx) = STATE.with(|s| s.borrow().ctx.clone()) {
        ctx.request_repaint();
    }
}

pub(crate) fn js_err(e: wasm_bindgen::JsValue) -> String {
    format!("{e:?}")
}

pub(crate) async fn fetch(method: &str, url: &str, headers: &[(&str, &str)], body: Option<&[u8]>) -> Result<web_sys::Response, String> {
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
    let req = web_sys::Request::new_with_str_and_init(url, &init).map_err(js_err)?;
    let win = web_sys::window().ok_or("no window")?;
    let resp = JsFuture::from(win.fetch_with_request(&req)).await.map_err(js_err)?;
    resp.dyn_into::<web_sys::Response>().map_err(|_| "fetch did not return a Response".to_string())
}

pub(crate) fn header(r: &web_sys::Response, name: &str) -> Option<String> {
    r.headers().get(name).ok().flatten()
}

/// The body, refused before reading when `Content-Length` exceeds `cap` and after when it does.
async fn read_body(r: &web_sys::Response, cap: usize) -> Result<Vec<u8>, String> {
    if header(r, "content-length").and_then(|h| remote_dav::parse_content_length(&h)).is_some_and(|n| n > cap as u64) {
        return Err("the file is too large".into());
    }
    let buf = JsFuture::from(r.array_buffer().map_err(js_err)?).await.map_err(js_err)?;
    let a = js_sys::Uint8Array::new(&buf);
    if a.length() as usize > cap {
        return Err("the file is too large".into());
    }
    Ok(a.to_vec())
}

pub(crate) fn cfg_and_origin() -> Result<(Config, String), String> {
    STATE.with(|s| {
        let s = s.borrow();
        s.cfg.clone().map(|c| (c, s.origin.clone())).ok_or_else(|| "remote storage is not configured".to_string())
    })
}

/// Read `./vectorcraft-config.json`; remote storage stays off when it is missing or invalid.
pub async fn load_config() {
    let Some(loc) = web_sys::window().map(|w| w.location()) else { return };
    let (origin, path) = (loc.origin().unwrap_or_default(), loc.pathname().unwrap_or_default());
    let base_dir = path.get(..=path.rfind('/').unwrap_or(0)).unwrap_or("/").to_string();
    let bytes = match fetch("GET", "vectorcraft-config.json", &[], None).await {
        Ok(r) if r.status() == 200 => read_body(&r, CONFIG_MAX).await,
        Ok(r) => Err(format!("status {}", r.status())),
        Err(e) => Err(e),
    };
    match bytes.and_then(|b| remote_config::parse(&b, &origin, &base_dir)) {
        Ok(cfg) => {
            log::info!("remote storage: {} mount(s)", cfg.mounts.len());
            STATE.with(|s| {
                let mut s = s.borrow_mut();
                s.cfg = Some(cfg);
                s.origin = origin;
            });
        }
        Err(e) => log::info!("remote storage off: {e}"),
    }
}

/// PROPFIND `dir` (Depth 1) and map the answer to virtual paths.
async fn propfind(dir: &str) -> Result<Vec<Child>, String> {
    let (cfg, origin) = cfg_and_origin()?;
    let mount = cfg.mount_for(dir).ok_or_else(|| format!("{dir}: not on a remote mount"))?.clone();
    let url = cfg.url_for(&format!("{}/", dir.trim_end_matches('/')))?;
    let r = fetch("PROPFIND", &url, &[("Depth", "1"), ("Content-Type", "application/xml")], Some(remote_dav::PROPFIND_BODY.as_bytes())).await?;
    if r.status() != 207 {
        return Err(format!("{dir}: the server answered {} to a listing", r.status()));
    }
    let entries = remote_dav::parse_multistatus(&read_body(&r, remote_dav::MAX_XML_BYTES).await?)?;
    Ok(remote_dav::children_of(&cfg, &mount, dir, &origin, entries))
}

fn record_dir(dir: &str, res: Result<Vec<Child>, String>) {
    let now = now_s();
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.dirs.entry(dir.to_string()).or_default();
        st.loading = false;
        match res {
            Ok(children) => {
                st.error = None;
                st.fetched = now;
                st.shown = false;
                st.entries =
                    Some(children.iter().map(|c| RemoteEntry { name: c.name.clone(), is_dir: c.entry.is_dir, size: c.entry.size }).collect());
            }
            Err(e) => st.error = Some((e, now)),
        }
    });
}

fn start_listing(dir: String) {
    wasm_bindgen_futures::spawn_local(async move {
        let res = propfind(&dir).await;
        record_dir(&dir, res);
        repaint();
    });
}

/// The listing of a folder: cached, else started and answered `WouldBlock` until it arrives. An
/// expired one is shown while it refreshes.
fn list(dir: &str) -> io::Result<Vec<RemoteEntry>> {
    let dir = dir.trim_end_matches('/').to_string();
    if !is_remote(&dir) {
        return Err(io::Error::other(format!("{dir} is not on remote storage")));
    }
    let now = now_s();
    let (result, start) = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let ttl = s.cfg.as_ref().map_or(remote_config::DEFAULT_LIST_CACHE_S, |c| c.list_cache_s);
        let st = s.dirs.entry(dir.clone()).or_default();
        // A listing that was not shown yet is always fresh, so a TTL of 0 does not refetch in a loop.
        let fresh = st.entries.is_some() && (!st.shown || now - st.fetched < ttl);
        let blocked = st.loading || st.error.as_ref().is_some_and(|(_, at)| now - at < ERROR_RETRY_S);
        let start = !fresh && !blocked;
        if start {
            st.loading = true;
        }
        st.shown |= st.entries.is_some();
        let result = match (&st.entries, &st.error) {
            (Some(e), _) => Ok(e.clone()),
            (None, Some((msg, at))) if now - at < ERROR_RETRY_S => Err(io::Error::other(msg.clone())),
            _ => Err(io::Error::new(io::ErrorKind::WouldBlock, "Loading…")),
        };
        (result, start)
    });
    if start {
        start_listing(dir);
    }
    result
}

fn is_remote(path: &str) -> bool {
    STATE.with(|s| s.borrow().cfg.as_ref().is_some_and(|c| c.mount_for(path).is_some()))
}

/// Drop the cached listing of the folder of `path` (after it was written): the next look asks again.
pub(crate) fn forget(path: &str) {
    STATE.with(|s| {
        if let Some(p) = remote_config::parent(path) {
            s.borrow_mut().dirs.remove(p);
        }
    });
}

/// GET a whole file, remembering its ETag for the save that follows.
async fn get_file(path: &str) -> Result<Vec<u8>, String> {
    let (cfg, _) = cfg_and_origin()?;
    let r = fetch("GET", &cfg.url_for(path)?, &[], None).await?;
    if r.status() != 200 {
        return Err(format!("the server answered {}", r.status()));
    }
    crate::remote_upload::note_opened(path, header(&r, "etag"));
    read_body(&r, MAX_FILE_BYTES).await
}

fn start_fetch(path: &str, purpose: Purpose, inbox: &RemoteInbox) {
    let (path, inbox) = (path.to_string(), inbox.clone());
    wasm_bindgen_futures::spawn_local(async move {
        let result = get_file(&path).await;
        inbox.lock().unwrap_or_else(|e| e.into_inner()).push(RemoteEvent::Fetched { path, purpose, result });
        repaint();
    });
}

/// The app's remote storage; `None` when the page has no (valid) config. `ctx` is woken when
/// something arrives.
pub fn storage(ctx: &egui::Context, inbox: &RemoteInbox) -> Option<RemoteStorage> {
    let (cfg, _) = cfg_and_origin().ok()?;
    STATE.with(|s| s.borrow_mut().ctx = Some(ctx.clone()));
    let mounts = cfg.mounts.iter().map(|m| RemoteMount { name: m.name.clone(), path: m.path.clone(), writable: m.writable }).collect();
    let fetch_inbox = inbox.clone();
    let write_inbox = inbox.clone();
    Some(RemoteStorage {
        mounts,
        is_remote: Box::new(is_remote),
        list: Box::new(list),
        fetch: Box::new(move |path, purpose| start_fetch(path, purpose, &fetch_inbox)),
        write: Box::new(move |path, data| crate::remote_upload::write(path, data, &write_inbox)),
        inbox: inbox.clone(),
    })
}
