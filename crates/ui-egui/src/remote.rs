//! Remote storage in the web build: files on a server the page configures (WebDAV mounts, see
//! `docs/development.md` › Web build › Remote storage) opened, placed and saved from the File
//! menu. Off unless the host gives [`Services::remote`].
//!
//! The host does the I/O: it lists folders, fetches a file and uploads a save in the background,
//! and reports back through [`RemoteStorage::inbox`]. This module holds what the app does with it:
//! the commands (File → Open Remote…, Place from Remote…, Save As Remote…), the dialog's
//! [`Mode`], and the events that come back ([`drain`]).
//!
//! A document opened from a mount remembers its remote path, so File → Save writes back to the
//! server ([`crate::io::write_to`]). A file placed from a mount is embedded: a link to a remote
//! path would not resolve when the document is opened again.

use std::io;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use vectorcraft_engine::cmd::fileio::{self, SAVE_FORMATS, SaveMode};

use crate::VectorcraftApp;
use crate::place::PlaceArrival;

/// One server folder mounted under a virtual path (`/data`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteMount {
    pub name: String,
    /// The virtual path of the mount's folder, without a trailing slash.
    pub path: String,
    /// Saves are accepted (a read-only mount refuses them with a message).
    pub writable: bool,
}

/// One row of a folder listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
}

/// What a fetched file is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Open,
    Place,
}

/// What the host reports back.
#[derive(Debug)]
pub enum RemoteEvent {
    /// A file asked for with [`RemoteStorage::fetch`]: its bytes, or why it could not be read.
    Fetched { path: String, purpose: Purpose, result: Result<Vec<u8>, String> },
    /// An upload that [`RemoteStorage::write`] started did not reach the server.
    SaveFailed { path: String, message: String },
}

pub type RemoteInbox = Arc<Mutex<Vec<RemoteEvent>>>;
/// [`RemoteStorage::list`].
pub type ListFn = Box<dyn FnMut(&str) -> io::Result<Vec<RemoteEntry>>>;
/// [`RemoteStorage::fetch`].
pub type FetchFn = Box<dyn FnMut(&str, Purpose)>;
/// [`RemoteStorage::write`].
pub type WriteFn = Box<dyn FnMut(&str, &[u8]) -> Result<(), String>>;

/// The host's remote storage (web: same-origin WebDAV).
pub struct RemoteStorage {
    pub mounts: Vec<RemoteMount>,
    /// Is a virtual path on one of the mounts?
    pub is_remote: Box<dyn Fn(&str) -> bool>,
    /// The entries of a folder on a mount. `WouldBlock` while the listing is still loading: it is
    /// asked for again on the next frames.
    pub list: ListFn,
    /// Start fetching a file; the answer is a [`RemoteEvent::Fetched`].
    pub fetch: FetchFn,
    /// Save bytes to a path on a mount. Returns at once; an upload that fails later is a
    /// [`RemoteEvent::SaveFailed`]. An error for a path that cannot be written (a read-only mount).
    pub write: WriteFn,
    pub inbox: RemoteInbox,
}

/// What the remote dialog chooses ([`crate::dialogs::remote`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Open,
    Place,
    Save,
}

impl Mode {
    /// The id the dialog and the commands use for this mode.
    pub fn id(self) -> &'static str {
        match self {
            Mode::Open => "open",
            Mode::Place => "place",
            Mode::Save => "save",
        }
    }

    /// The mode for a dialog id (anything unknown is Open).
    pub fn of(id: &str) -> Mode {
        match id {
            "place" => Mode::Place,
            "save" => Mode::Save,
            _ => Mode::Open,
        }
    }
}

/// Is remote storage configured?
pub fn available(app: &VectorcraftApp) -> bool {
    app.services.remote.is_some()
}

/// The UI commands File → Open Remote…, Place from Remote… and Save As Remote…; `None` for any
/// other id. With `{path}` the file is fetched or saved at once, without it the dialog asks.
pub fn command(app: &mut VectorcraftApp, id: &str, p: &Value) -> Option<Result<Value, String>> {
    let mode = match id {
        "file.openRemote" => Mode::Open,
        "file.placeRemote" => Mode::Place,
        "file.saveAsRemote" => Mode::Save,
        _ => return None,
    };
    Some(run(app, mode, p.get("path").and_then(Value::as_str)))
}

/// The File commands: open the dialog, or act on `path` at once.
fn run(app: &mut VectorcraftApp, mode: Mode, path: Option<&str>) -> Result<Value, String> {
    if !available(app) {
        return Err("remote storage is not configured".into());
    }
    if mode != Mode::Open && app.session.active().is_none() {
        return Err("no document".into());
    }
    match path {
        None => {
            crate::dialogs::remote::open(app, mode);
            Ok(json!({"dialog": crate::dialogs::remote::KIND}))
        }
        Some(path) => accept(app, mode, path),
    }
}

/// The dialog's OK, or a command with a path: fetch the file at `path` for opening or placing, or
/// save the active document there (a name without a known save extension gets `.vectorcraft`).
pub(crate) fn accept(app: &mut VectorcraftApp, mode: Mode, path: &str) -> Result<Value, String> {
    let remote = app.services.remote.as_mut().ok_or("remote storage is not configured")?;
    if !(remote.is_remote)(path) {
        return Err(format!("{path} is not on remote storage"));
    }
    let name = fileio::file_name(path);
    match mode {
        Mode::Open | Mode::Place => {
            let purpose = if mode == Mode::Open { Purpose::Open } else { Purpose::Place };
            (remote.fetch)(path, purpose);
            app.status(format!("Fetching {name}…"));
            Ok(json!({"pending": "fetch", "path": path}))
        }
        Mode::Save => {
            let path = save_path(path)?;
            crate::io::save(app, SaveMode::SaveAs, &json!({ "path": path }), true)
        }
    }
}

/// Open the file at `path` when it is on a remote mount: start fetching it (the document opens when
/// the bytes arrive, see [`drain`]) → `{pending: "fetch"}`. `None` for any other path. A second
/// open of a path whose fetch is still running does nothing.
pub(crate) fn open_path(app: &mut VectorcraftApp, path: &str) -> Option<Result<Value, String>> {
    let remote = app.services.remote.as_ref()?;
    if !(remote.is_remote)(path) {
        return None;
    }
    if app.remote_opening.iter().any(|p| p == path) {
        return Some(Ok(json!({"pending": "fetch", "path": path})));
    }
    app.remote_opening.push(path.to_string());
    let r = accept(app, Mode::Open, path);
    if r.is_err() {
        app.remote_opening.retain(|p| p != path);
    }
    Some(r)
}

/// `path` with the extension of a save format: kept when it names one, else `.vectorcraft`.
pub(crate) fn save_path(path: &str) -> Result<String, String> {
    let native = fileio::format("vectorcraft").ok_or("no native format")?;
    let format = fileio::format_for_name(path).filter(|f| SAVE_FORMATS.contains(&f.id)).unwrap_or(native);
    Ok(crate::io::with_save_extension(path, format, SaveMode::SaveAs))
}

/// A file name that stays in its folder: not empty, not `.` or `..`, no separator or control character.
pub fn valid_name(name: &str) -> bool {
    let name = name.trim();
    !(name.is_empty() || name == "." || name == ".." || name.chars().any(|c| c == '/' || c == '\\' || c.is_control()))
}

/// `name` in `dir`, as the save dialog makes it.
pub fn join(dir: &str, name: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), name.trim())
}

/// The name the save dialog starts with: the document's, in the format it was read or saved in
/// when that is a save format, else the native one.
pub fn suggested_name(app: &VectorcraftApp) -> String {
    let Some(st) = app.session.active() else { return "Untitled.vectorcraft".into() };
    let title = st.path.clone().unwrap_or_else(|| st.doc.title.clone());
    let ext = fileio::format(st.format).filter(|f| SAVE_FORMATS.contains(&f.id)).map_or("vectorcraft", |f| f.extensions[0]);
    format!("{}.{ext}", fileio::file_stem(&title))
}

/// Take what the host reported and act on it: open or place fetched files, and tell the user
/// about a save that did not reach the server (the document is unsaved again).
pub fn drain(app: &mut VectorcraftApp) {
    let events = app.services.remote.as_ref().map(|r| std::mem::take(&mut *r.inbox.lock().unwrap_or_else(|e| e.into_inner()))).unwrap_or_default();
    for event in events {
        match event {
            RemoteEvent::Fetched { path, purpose, result } => fetched(app, path, purpose, result),
            RemoteEvent::SaveFailed { path, message } => save_failed(app, &path, &message),
        }
    }
}

/// A fetched file: open it, place it, or report why it could not be read.
fn fetched(app: &mut VectorcraftApp, path: String, purpose: Purpose, result: Result<Vec<u8>, String>) {
    let name = fileio::file_name(&path);
    if purpose == Purpose::Open {
        app.remote_opening.retain(|p| *p != path);
    }
    match (purpose, result) {
        (Purpose::Open, Err(e)) => crate::io::report_open_error(app, &name, &e),
        (Purpose::Place, Err(e)) => app.status(format!("Couldn't place {name}: {e}")),
        // The document remembers where it came from: Save writes back there.
        (Purpose::Open, Ok(bytes)) => {
            if let Err(e) = crate::io::open_bytes(app, &name, &bytes, Some(path)) {
                crate::io::report_open_error(app, &name, &e);
            }
        }
        // Embedded, as a picked local file is: the arrival goes to the Place dialog.
        (Purpose::Place, Ok(bytes)) => match app.services.place_inbox.as_ref() {
            Some(queue) => queue.lock().unwrap_or_else(|e| e.into_inner()).push(PlaceArrival { name, bytes, drop: None }),
            None => app.status("Couldn't place: no Place dialog"),
        },
    }
}

/// An upload that did not reach the server: its documents are unsaved again and the user is told.
fn save_failed(app: &mut VectorcraftApp, path: &str, message: &str) {
    let uids: Vec<u64> = app.session.documents().iter().filter(|d| d.path.as_deref() == Some(path)).map(|d| d.uid).collect();
    for uid in uids {
        if let Some(st) = app.session.document_mut(uid) {
            st.mark_unsaved();
        }
    }
    app.status(message.to_string());
    crate::io::report_write_error(app, false, path, message);
}

#[cfg(test)]
mod tests;
