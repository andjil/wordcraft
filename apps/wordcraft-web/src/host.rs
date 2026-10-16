//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! WordCraft in an `<iframe>`. The xrm fork's port of gridcraft-web's `host.rs`: the same shape and
//! the same messages, with the `wordcraft:` prefix. Messages are plain objects with a `type`:
//!
//! - `wordcraft:ready` (to the parent): `{ version, protocol }`, the app is listening; send a file now.
//! - `wordcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or `Uint8Array`.
//!   It opens like a document picked with File › Open or dropped on the window.
//! - `wordcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file WordCraft would otherwise download: Save and Save As (the document) and the exports
//!   (PDF, PNG). The parent stores it. A document save stays unconfirmed — the document still
//!   counts as unsaved, and a "Save changes?" prompt waits — until the parent answers
//!   `wordcraft:saved` `{ name }` or `wordcraft:save-failed` `{ name, error }`, or
//!   [`ACK_TIMEOUT_MS`] passes (then it counts as failed).
//! - `wordcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `wordcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//! - `wordcraft:command` (from the parent): `{ id, command, params }` runs one of WordCraft's own
//!   commands (`file.save`, `home.bold`, …) or a bridge command (`host.save`, `host.state`); the
//!   answer is `wordcraft:result` `{ id, ok, result }` or `{ id, ok: false, error }`.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts on a blank document instead of the start screen, and the parent guards leaving the page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_ui_egui::async_saves::{AsyncSaves, SaveOutcome};
use wordcraft_ui_egui::{Inbox, Services, WordApp};

/// The protocol version announced in `wordcraft:ready`.
const PROTOCOL: u32 = 1;

/// How long a document save waits for the parent's `wordcraft:saved` before it counts as failed.
const ACK_TIMEOUT_MS: f64 = 20_000.0;

/// A document save handed to the parent and not yet confirmed.
struct PendingSave {
    name: String,
    ticket: u64,
    /// `Date.now()` when it was handed over.
    started: f64,
}

/// A message from the parent that the frame handles (the app lives on the UI thread).
enum Inbound {
    /// `wordcraft:command` `{ id, command, params }`: run a command, answer with `wordcraft:result`.
    Command { id: JsValue, command: String, params: serde_json::Value },
    /// `wordcraft:saved` `{ name }`: the parent stored the file it was handed.
    Saved { name: String },
    /// `wordcraft:save-failed` `{ name, error }`: the parent couldn't store it.
    SaveFailed { name: String, error: String },
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// Messages from the parent waiting for the frame.
    inbound: Rc<RefCell<Vec<Inbound>>>,
    /// Document saves handed to the parent and not yet confirmed.
    pending: Rc<RefCell<Vec<PendingSave>>>,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
    /// The app's unsaved-changes flag (`Services::on_dirty`), shared with the leave-page guard.
    dirty: Rc<Cell<bool>>,
    /// The app's save tickets (`Services::async_saves`): a handed-over save ends when the parent answers.
    saves: AsyncSaves,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new(dirty: Rc<Cell<bool>>, saves: AsyncSaves) -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, inbound: Rc::default(), pending: Rc::default(), reported: Rc::default(), dirty, saves })
    }

    /// Send saved and exported files to the parent instead of downloading them, and hear files
    /// and commands from it.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes)));
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "wordcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into()), ("protocol", PROTOCOL.into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "wordcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: the parent's commands and confirmations are applied, saves the parent
    /// never confirmed time out, and the parent hears when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut WordApp) {
        self.run_inbound(app);
        self.expire_pending();
        let dirty = self.dirty.get();
        if self.reported.get() != Some(dirty) && self.send(&[("type", "wordcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    /// Hand a file to the parent. A document save (a format that keeps everything) opens a save
    /// ticket the app reads right after this returns; the parent's answer closes it.
    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[("type", "wordcraft:save".into()), ("name", name.as_str().into()), ("bytes", buffer.clone().into())])
            .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        if wordcraft_ui_egui::keeps_everything(&name) {
            let ticket = self.saves.start();
            self.pending.borrow_mut().push(PendingSave { name, ticket, started: js_sys::Date::now() });
        }
        Ok(())
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let inbound = self.inbound.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            if let Some(message) = Inbound::parse(&data) {
                inbound.borrow_mut().push(message);
                ctx.request_repaint();
                return;
            }
            if string(&data, "type").as_deref() != Some("wordcraft:open") {
                return;
            }
            let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Document.docx".to_string());
            let Some(bytes) = bytes(&data) else { return };
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// The parent's messages since the last frame: commands run against the app and are answered;
    /// confirmations end the save they belong to.
    fn run_inbound(&self, app: &mut WordApp) {
        let queue = std::mem::take(&mut *self.inbound.borrow_mut());
        for message in queue {
            match message {
                Inbound::Command { id, command, params } => {
                    let result = self.run_command(app, &command, params);
                    self.post_result(&id, result);
                }
                Inbound::Saved { name } => {
                    if !self.finish(&name, SaveOutcome::Saved) {
                        log::debug!("the page stored {name}");
                    }
                }
                Inbound::SaveFailed { name, error } => {
                    log::error!("the page couldn't store {name}: {error}");
                    if !self.finish(&name, SaveOutcome::Failed(error.clone())) {
                        app.status(format!("Couldn't store {name}: {error}"));
                    }
                }
            }
        }
    }

    /// End the newest pending save of `name` (any pending save when the name is empty); false
    /// when there was none.
    fn finish(&self, name: &str, outcome: SaveOutcome) -> bool {
        let mut pending = self.pending.borrow_mut();
        let Some(i) = pending.iter().rposition(|p| name.is_empty() || p.name == name) else { return false };
        let p = pending.swap_remove(i);
        self.saves.finish(p.ticket, outcome);
        true
    }

    /// Saves the parent never answered count as failed, so the document stays unsaved and the
    /// app's prompts come back instead of waiting forever.
    fn expire_pending(&self) {
        let now = js_sys::Date::now();
        let expired: Vec<PendingSave> = {
            let mut pending = self.pending.borrow_mut();
            let (old, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending).into_iter().partition(|p| now - p.started > ACK_TIMEOUT_MS);
            *pending = keep;
            old
        };
        for p in expired {
            log::error!("the page didn't confirm the save of {}", p.name);
            self.saves.finish(p.ticket, SaveOutcome::Failed("the page didn't confirm the save".to_string()));
        }
    }

    /// `host.*` commands are the bridge's own; anything else is one of WordCraft's commands
    /// (`crates/engine/src/cmd/*`, the UI commands), as the control channel runs them.
    fn run_command(&self, app: &mut WordApp, command: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        match command {
            // Save hands the document to the parent (`post_save`) exactly as File › Save does.
            "host.save" => app.run("file.save", params),
            "host.state" => Ok(serde_json::json!({
                "dirty": app.session.dirty,
                "title": app.session.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string()),
                "pendingSaves": self.pending.borrow().len(),
                "protocol": PROTOCOL,
            })),
            other => app.run(other, params),
        }
    }

    fn post_result(&self, id: &JsValue, result: Result<serde_json::Value, String>) {
        let fields: Vec<(&str, JsValue)> = match result {
            Ok(value) => vec![("type", "wordcraft:result".into()), ("id", id.clone()), ("ok", true.into()), ("result", to_js(&value))],
            Err(error) => vec![("type", "wordcraft:result".into()), ("id", id.clone()), ("ok", false.into()), ("error", error.as_str().into())],
        };
        self.send(&fields);
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

impl Inbound {
    fn parse(data: &JsValue) -> Option<Self> {
        match string(data, "type")?.as_str() {
            "wordcraft:command" => {
                let command = string(data, "command")?;
                let id = js_sys::Reflect::get(data, &JsValue::from_str("id")).unwrap_or(JsValue::NULL);
                let params = json_field(data, "params").unwrap_or_else(|| serde_json::json!({}));
                Some(Self::Command { id, command, params })
            }
            "wordcraft:saved" => Some(Self::Saved { name: string(data, "name").unwrap_or_default() }),
            "wordcraft:save-failed" => {
                Some(Self::SaveFailed { name: string(data, "name").unwrap_or_default(), error: string(data, "error").unwrap_or_default() })
            }
            _ => None,
        }
    }
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}

/// A JSON field of a message as serde JSON (through `JSON.stringify`; absent or null → `None`).
fn json_field(data: &JsValue, key: &str) -> Option<serde_json::Value> {
    let value = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    let text: String = js_sys::JSON::stringify(&value).ok()?.into();
    serde_json::from_str(&text).ok()
}

/// serde JSON as a JavaScript value (`null` when it can't be converted).
fn to_js(value: &serde_json::Value) -> JsValue {
    js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}
