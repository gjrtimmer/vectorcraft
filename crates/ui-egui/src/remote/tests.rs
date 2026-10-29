//! Remote storage in the app: the menus with and without it, opening, placing and saving through
//! a host that records what it is asked.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use serde_json::json;
use vectorcraft_engine::Session;

use super::{Mode, Purpose, RemoteEntry, RemoteEvent, RemoteInbox, RemoteMount, RemoteStorage, join, save_path, valid_name};
use crate::place::PlaceInbox;
use crate::{Services, VectorcraftApp, menus};

/// What the host was asked: fetches (path, purpose) and writes (path, bytes).
#[derive(Default)]
struct Log {
    fetched: Vec<(String, Purpose)>,
    written: Vec<(String, Vec<u8>)>,
    downloads: Vec<String>,
}

fn mounts() -> Vec<RemoteMount> {
    vec![
        RemoteMount { name: "Server".into(), path: "/rw".into(), writable: true },
        RemoteMount { name: "Archive".into(), path: "/ro".into(), writable: false },
    ]
}

fn host(log: &Rc<RefCell<Log>>) -> (RemoteStorage, RemoteInbox) {
    let inbox = RemoteInbox::default();
    let on = |p: &str| p == "/rw" || p.starts_with("/rw/") || p == "/ro" || p.starts_with("/ro/");
    let (fetch_log, write_log) = (log.clone(), log.clone());
    let remote = RemoteStorage {
        mounts: mounts(),
        is_remote: Box::new(on),
        list: Box::new(|dir| match dir {
            "/rw" => Ok(vec![
                RemoteEntry { name: "b.svg".into(), is_dir: false, size: Some(9) },
                RemoteEntry { name: "docs".into(), is_dir: true, size: None },
                RemoteEntry { name: "taken.vectorcraft".into(), is_dir: false, size: Some(9) },
            ]),
            "/loading" => Err(io::Error::new(io::ErrorKind::WouldBlock, "Loading…")),
            _ => Ok(vec![]),
        }),
        fetch: Box::new(move |p, purpose| fetch_log.borrow_mut().fetched.push((p.to_string(), purpose))),
        write: Box::new(move |p, b| {
            if p.starts_with("/ro/") {
                return Err("Archive is read-only".into());
            }
            write_log.borrow_mut().written.push((p.to_string(), b.to_vec()));
            Ok(())
        }),
        inbox: inbox.clone(),
    };
    (remote, inbox)
}

fn app_with(remote: Option<RemoteStorage>, log: &Rc<RefCell<Log>>) -> VectorcraftApp {
    let dl = log.clone();
    let services = Services {
        download: Some(Box::new(move |name: &str, _: &[u8]| {
            dl.borrow_mut().downloads.push(name.to_string());
            Ok(())
        })),
        place_inbox: Some(PlaceInbox::default()),
        remote,
        ..Default::default()
    };
    VectorcraftApp::new(Session::new(), services)
}

fn remote_app() -> (VectorcraftApp, RemoteInbox, Rc<RefCell<Log>>) {
    let log = Rc::new(RefCell::new(Log::default()));
    let (remote, inbox) = host(&log);
    (app_with(Some(remote), &log), inbox, log)
}

/// The File menu's labels as shown.
fn file_menu(app: &VectorcraftApp) -> Vec<String> {
    menus::menu_entries(app).into_iter().filter(|e| e.path == ["File"] && e.command.is_some()).map(|e| e.label).collect()
}

fn native_bytes() -> Vec<u8> {
    let bytes = Rc::new(RefCell::new(vec![]));
    let sink = bytes.clone();
    let mut app = VectorcraftApp::new(
        Session::new(),
        Services {
            write: Some(Box::new(move |_: &str, b: &[u8]| {
                *sink.borrow_mut() = b.to_vec();
                Ok(())
            })),
            ..Default::default()
        },
    );
    app.run("file.new", json!({"width": 100, "height": 100})).unwrap();
    app.run("shape.rectangle", json!({"x": 10, "y": 10, "width": 30, "height": 20})).unwrap();
    app.run("file.saveAs", json!({"path": "/tmp/a.vectorcraft"})).unwrap();
    bytes.borrow().clone()
}

#[test]
fn without_remote_storage_the_file_menu_is_as_it_was() {
    let log = Rc::new(RefCell::new(Log::default()));
    let app = app_with(None, &log);
    let labels = file_menu(&app);
    assert!(labels.iter().any(|l| l == "Open…") && labels.iter().any(|l| l == "Place…"), "{labels:?}");
    assert!(!labels.iter().any(|l| l.contains("Remote") || l.contains("Local")), "{labels:?}");
    for id in ["file.openRemote", "file.placeRemote", "file.saveAsRemote"] {
        assert!(!menus::enabled(&app, id), "{id}");
    }
}

#[test]
fn with_remote_storage_open_and_place_say_local_and_the_remote_entries_follow() {
    let (mut app, ..) = remote_app();
    app.run("file.new", json!({})).unwrap();
    let labels = file_menu(&app);
    let at = |l: &str| labels.iter().position(|x| x == l).unwrap_or_else(|| panic!("{l} in {labels:?}"));
    assert_eq!(at("Open Remote…"), at("Open Local…") + 1);
    assert_eq!(at("Place from Remote…"), at("Place Local…") + 1);
    assert_eq!(at("Save As Remote…"), at("Save As…") + 1);
    assert!(!labels.iter().any(|l| l == "Open…" || l == "Place…"), "{labels:?}");
    for id in ["file.openRemote", "file.placeRemote", "file.saveAsRemote"] {
        assert!(menus::enabled(&app, id), "{id}");
    }
}

#[test]
fn the_commands_fail_cleanly_without_remote_storage() {
    let log = Rc::new(RefCell::new(Log::default()));
    let mut app = app_with(None, &log);
    app.run("file.new", json!({})).unwrap();
    for id in ["file.openRemote", "file.placeRemote", "file.saveAsRemote"] {
        assert_eq!(app.run(id, json!({"path": "/rw/a.vectorcraft"})).unwrap_err(), "remote storage is not configured", "{id}");
    }
}

#[test]
fn a_fetched_file_opens_as_a_document_that_remembers_its_path_and_saves_back() {
    let (mut app, inbox, log) = remote_app();
    app.run("file.openRemote", json!({"path": "/rw/docs/a.vectorcraft"})).unwrap();
    assert_eq!(log.borrow().fetched, [("/rw/docs/a.vectorcraft".to_string(), Purpose::Open)]);
    inbox.lock().unwrap().push(RemoteEvent::Fetched { path: "/rw/docs/a.vectorcraft".into(), purpose: Purpose::Open, result: Ok(native_bytes()) });
    crate::remote::drain(&mut app);
    assert_eq!(app.session.active().and_then(|d| d.path.clone()).as_deref(), Some("/rw/docs/a.vectorcraft"));
    app.run("shape.rectangle", json!({"x": 1, "y": 1, "width": 5, "height": 5})).unwrap();
    app.run("file.save", json!({})).unwrap();
    let log = log.borrow();
    assert_eq!(log.written.len(), 1, "Save goes to the server");
    assert_eq!(log.written[0].0, "/rw/docs/a.vectorcraft");
    assert!(log.downloads.is_empty(), "not to a download");
}

#[test]
fn a_file_that_cannot_be_fetched_is_reported() {
    let (mut app, inbox, _) = remote_app();
    inbox.lock().unwrap().push(RemoteEvent::Fetched {
        path: "/rw/gone.vectorcraft".into(),
        purpose: Purpose::Open,
        result: Err("the server answered 404".into()),
    });
    crate::remote::drain(&mut app);
    assert!(app.session.active().is_none());
    assert!(app.ui.status.contains("gone.vectorcraft"), "{}", app.ui.status);
}

#[test]
fn a_save_the_server_refused_leaves_the_document_unsaved() {
    let (mut app, inbox, _) = remote_app();
    inbox.lock().unwrap().push(RemoteEvent::Fetched { path: "/rw/a.vectorcraft".into(), purpose: Purpose::Open, result: Ok(native_bytes()) });
    crate::remote::drain(&mut app);
    assert!(!app.session.active().unwrap().is_dirty());
    inbox
        .lock()
        .unwrap()
        .push(RemoteEvent::SaveFailed { path: "/rw/a.vectorcraft".into(), message: "Document changed on the server; use Save As".into() });
    crate::remote::drain(&mut app);
    assert!(app.session.active().unwrap().is_dirty(), "still unsaved");
    assert!(app.ui.status.contains("Save As"), "{}", app.ui.status);
}

#[test]
fn save_as_remote_writes_to_the_mount_and_a_read_only_mount_refuses() {
    let (mut app, _, log) = remote_app();
    app.run("file.new", json!({"width": 100, "height": 100})).unwrap();
    app.run("file.saveAsRemote", json!({"path": "/rw/new"})).unwrap();
    assert_eq!(log.borrow().written[0].0, "/rw/new.vectorcraft", "a name without a save extension gets the native one");
    assert_eq!(app.session.active().and_then(|d| d.path.clone()).as_deref(), Some("/rw/new.vectorcraft"));
    app.run("file.new", json!({"width": 100, "height": 100})).unwrap();
    assert!(app.run("file.saveAsRemote", json!({"path": "/ro/x.vectorcraft"})).is_err());
    assert_eq!(log.borrow().written.len(), 1);
    assert!(app.run("file.saveAsRemote", json!({"path": "/local/x.vectorcraft"})).is_err(), "not on a mount");
}

#[test]
fn place_from_remote_embeds_through_the_place_dialog() {
    let (mut app, inbox, log) = remote_app();
    app.run("file.new", json!({})).unwrap();
    app.run("file.placeRemote", json!({"path": "/rw/b.svg"})).unwrap();
    assert_eq!(log.borrow().fetched, [("/rw/b.svg".to_string(), Purpose::Place)]);
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><rect width="10" height="10"/></svg>"#.to_vec();
    inbox.lock().unwrap().push(RemoteEvent::Fetched { path: "/rw/b.svg".into(), purpose: Purpose::Place, result: Ok(svg) });
    crate::remote::drain(&mut app);
    crate::place::drain(&mut app);
    let d = app.ui.dialog.as_ref().expect("the Place dialog opens");
    assert_eq!(d.kind, crate::dialogs::place::KIND);
    assert_eq!(d.fields["files"][0]["name"], "b.svg");
    assert!(d.fields["files"][0].get("path").is_none(), "no path: embedded, never linked");
}

#[test]
fn the_dialog_starts_at_the_mounts_and_confirming_fetches_the_chosen_file() {
    let (mut app, _, log) = remote_app();
    app.run("file.openRemote", json!({})).unwrap();
    let d = app.ui.dialog.as_ref().unwrap();
    assert_eq!((d.kind.as_str(), d.str("dir").as_str(), d.str("mode").as_str()), ("remote", "", "open"));
    assert!(crate::dialogs::confirm(&mut app).is_err(), "no folder yet");
    for (k, v) in [("dir", "/rw"), ("name", "b.svg")] {
        app.ui.dialog.as_mut().unwrap().fields.insert(k.into(), json!(v));
    }
    crate::dialogs::confirm(&mut app).unwrap();
    assert!(app.ui.dialog.is_none());
    assert_eq!(log.borrow().fetched, [("/rw/b.svg".to_string(), Purpose::Open)]);
}

#[test]
fn the_save_dialog_suggests_a_name_and_refuses_read_only_mounts_and_other_files() {
    let (mut app, ..) = remote_app();
    app.run("file.new", json!({"width": 100, "height": 100})).unwrap();
    app.run("file.saveAsRemote", json!({})).unwrap();
    assert_eq!(app.ui.dialog.as_ref().unwrap().str("name"), "Untitled-1.vectorcraft");
    let set = |app: &mut VectorcraftApp, dir: &str, name: &str| {
        let d = app.ui.dialog.as_mut().unwrap();
        d.fields.insert("dir".into(), json!(dir));
        d.fields.insert("name".into(), json!(name));
    };
    set(&mut app, "/ro", "x.vectorcraft");
    assert!(crate::dialogs::confirm(&mut app).unwrap_err().contains("read-only"));
    set(&mut app, "/rw", "taken");
    assert!(crate::dialogs::confirm(&mut app).unwrap_err().contains("already there"), "the listing has taken.vectorcraft");
    assert!(app.ui.dialog.is_some(), "the dialog stays open");
    set(&mut app, "/rw", "../escape");
    assert!(crate::dialogs::confirm(&mut app).is_err());
}

#[test]
fn the_dialog_draws_the_mounts_the_listing_and_a_listing_that_is_loading() {
    let (mut app, ..) = remote_app();
    app.run("file.openRemote", json!({})).unwrap();
    let ctx = egui::Context::default();
    crate::theme::install_fonts(&ctx);
    for dir in ["", "/rw", "/loading", "/rw/empty"] {
        app.ui.dialog.as_mut().unwrap().fields.insert("dir".into(), json!(dir));
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| crate::dialogs::show(&mut app, ui.ctx()));
        out.textures_delta.clear();
        assert!(app.ui.dialog.is_some(), "{dir}");
    }
}

#[test]
fn names_and_paths() {
    assert!(valid_name("a.vectorcraft") && valid_name(" a b "));
    for bad in ["", " ", ".", "..", "a/b", "a\\b", "a\nb"] {
        assert!(!valid_name(bad), "{bad:?}");
    }
    assert_eq!(join("/rw/", " x.svg "), "/rw/x.svg");
    assert_eq!(save_path("/rw/x").unwrap(), "/rw/x.vectorcraft");
    assert_eq!(save_path("/rw/x.svg").unwrap(), "/rw/x.svg");
    assert_eq!(save_path("/rw/x.png").unwrap(), "/rw/x.vectorcraft", "not a save format");
    assert_eq!(Mode::of("save"), Mode::Save);
    assert_eq!(Mode::of("junk"), Mode::Open);
}
