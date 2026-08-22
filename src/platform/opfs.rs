//! the working tree, stored in the origin private file system.
//!
//! this is the direct replacement for the old `staged: new Map()` staging
//! area. that map was allocated per request, so every edit the agent made
//! existed only inside the request that made it — when the run ended before
//! `git_commit`, the work was gone. the transcript of that failure reads
//! "re-staging it right now" over and over.
//!
//! opfs is real, origin-scoped, durable storage. a write here survives the
//! run that made it, a reload, a crash, and a closed tab. nothing needs to
//! be "preserved" at a deadline, because nothing was ever only in memory.

use js_sys::{Function, Object, Promise, Reflect};
use std::collections::BTreeMap;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemWritableFileStream};

const TREE_DIR: &str = "repo";
const INDEX_FILE: &str = "vanish-index.json";

/// one file as the harness knows it: the bytes on disk plus the github blob
/// they came from, which is what makes "dirty" answerable without a server.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct IndexEntry {
    /// blob sha of the content last synced from (or committed to) github.
    pub base_sha: String,
    pub size: usize,
    /// set when the local bytes no longer match `base_sha`.
    pub dirty: bool,
}

pub type Index = BTreeMap<String, IndexEntry>;

/// javascript rejections are rarely plain strings; dig out something a human
/// can act on instead of rendering `JsValue(Object)` into the ui.
pub fn describe(v: &JsValue) -> String {
    v.as_string()
        .or_else(|| {
            Reflect::get(v, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{v:?}"))
}

fn err(context: &str, e: JsValue) -> String {
    format!("{context}: {}", describe(&e))
}

/// pull a method off a js object and call it, returning its promise.
/// opfs option structs have churned across web-sys releases, so every call
/// goes through reflection: this cannot break on a dependency bump.
fn method(target: &JsValue, name: &str) -> Result<Function, String> {
    Reflect::get(target, &JsValue::from_str(name))
        .map_err(|e| err(&format!("{name} missing"), e))?
        .dyn_into::<Function>()
        .map_err(|_| format!("{name} is not callable"))
}

fn as_promise(v: JsValue, what: &str) -> Result<Promise, String> {
    v.dyn_into::<Promise>()
        .map_err(|_| format!("{what} did not return a promise"))
}

/// opfs is reachable from both the window and a worker, but through
/// different navigator objects. resolve whichever one this context has.
async fn root() -> Result<FileSystemDirectoryHandle, String> {
    let global = js_sys::global();
    let navigator = Reflect::get(&global, &JsValue::from_str("navigator"))
        .map_err(|e| err("no navigator in this context", e))?;
    let storage = Reflect::get(&navigator, &JsValue::from_str("storage"))
        .map_err(|e| err("navigator.storage unavailable", e))?;
    let get_directory = method(&storage, "getDirectory")
        .map_err(|_| "opfs is not supported by this browser".to_string())?;

    let promise = as_promise(
        get_directory
            .call0(&storage)
            .map_err(|e| err("getDirectory() threw", e))?,
        "getDirectory()",
    )?;

    JsFuture::from(promise)
        .await
        .map_err(|e| err("opfs root unavailable", e))?
        .dyn_into::<FileSystemDirectoryHandle>()
        .map_err(|_| "opfs root was not a directory handle".to_string())
}

fn opts(create: bool) -> Object {
    let o = Object::new();
    let _ = Reflect::set(
        &o,
        &JsValue::from_str("create"),
        &JsValue::from_bool(create),
    );
    o
}

async fn child_dir(
    parent: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> Result<FileSystemDirectoryHandle, String> {
    let f = method(parent, "getDirectoryHandle")?;
    let p = as_promise(
        f.call2(parent, &JsValue::from_str(name), &opts(create))
            .map_err(|e| err(&format!("getDirectoryHandle({name}) threw"), e))?,
        "getDirectoryHandle",
    )?;
    JsFuture::from(p)
        .await
        .map_err(|e| err(&format!("directory {name}"), e))?
        .dyn_into()
        .map_err(|_| format!("{name} was not a directory"))
}

async fn child_file(
    parent: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> Result<FileSystemFileHandle, String> {
    let f = method(parent, "getFileHandle")?;
    let p = as_promise(
        f.call2(parent, &JsValue::from_str(name), &opts(create))
            .map_err(|e| err(&format!("getFileHandle({name}) threw"), e))?,
        "getFileHandle",
    )?;
    JsFuture::from(p)
        .await
        .map_err(|e| err(&format!("file {name}"), e))?
        .dyn_into()
        .map_err(|_| format!("{name} was not a file"))
}

/// normalize to forward slashes and reject traversal before it can escape
/// the tree. the old harness had this guard server-side; keeping it means a
/// confused or hostile tool call still cannot write outside the repo.
pub fn normalize(path: &str) -> Result<Vec<String>, String> {
    let cleaned = path.replace('\\', "/");
    let mut parts = Vec::new();
    for seg in cleaned.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return Err(format!("path {path} escapes the working tree")),
            s => parts.push(s.to_string()),
        }
    }
    if parts.is_empty() {
        return Err("empty path".to_string());
    }
    Ok(parts)
}

async fn tree_root(create: bool) -> Result<FileSystemDirectoryHandle, String> {
    let r = root().await?;
    child_dir(&r, TREE_DIR, create).await
}

/// walk to the directory holding `parts`, creating intermediates on write.
async fn parent_of(parts: &[String], create: bool) -> Result<FileSystemDirectoryHandle, String> {
    let mut dir = tree_root(create).await?;
    for seg in &parts[..parts.len() - 1] {
        dir = child_dir(&dir, seg, create).await?;
    }
    Ok(dir)
}

/// read a handle's contents as text. shared by file reads and the index.
async fn handle_text(handle: &JsValue, what: &str) -> Result<String, String> {
    let get_file = method(handle, "getFile")?;
    let p = as_promise(
        get_file
            .call0(handle)
            .map_err(|e| err("getFile() threw", e))?,
        "getFile",
    )?;
    let file = JsFuture::from(p).await.map_err(|e| err(what, e))?;

    let text_fn = method(&file, "text")?;
    let tp = as_promise(
        text_fn
            .call0(&file)
            .map_err(|e| err("file.text() threw", e))?,
        "file.text",
    )?;

    JsFuture::from(tp)
        .await
        .map_err(|e| err(&format!("reading {what}"), e))?
        .as_string()
        .ok_or_else(|| format!("{what} did not decode as utf-8 text"))
}

pub async fn read(path: &str) -> Result<String, String> {
    let parts = normalize(path)?;
    let dir = parent_of(&parts, false).await?;
    let handle = child_file(&dir, parts.last().unwrap(), false).await?;
    handle_text(handle.as_ref(), path).await
}

/// write text through a handle, closing the stream so the bytes actually
/// land. a dropped stream leaves a zero-length file, which reads back as
/// silent corruption rather than an error.
async fn handle_write(handle: &JsValue, content: &str, what: &str) -> Result<(), String> {
    let cw = method(handle, "createWritable")?;
    let p = as_promise(
        cw.call0(handle)
            .map_err(|e| err("createWritable() threw", e))?,
        "createWritable",
    )?;
    let writable: FileSystemWritableFileStream = JsFuture::from(p)
        .await
        .map_err(|e| err(&format!("opening {what} for write"), e))?
        .dyn_into()
        .map_err(|_| "createWritable did not yield a writable stream".to_string())?;

    let wp = writable
        .write_with_str(content)
        .map_err(|e| err(&format!("writing {what}"), e))?;
    JsFuture::from(wp)
        .await
        .map_err(|e| err(&format!("writing {what}"), e))?;

    JsFuture::from(writable.close())
        .await
        .map_err(|e| err(&format!("closing {what}"), e))?;
    Ok(())
}

pub async fn write(path: &str, content: &str) -> Result<(), String> {
    let parts = normalize(path)?;
    let dir = parent_of(&parts, true).await?;
    let handle = child_file(&dir, parts.last().unwrap(), true).await?;
    handle_write(handle.as_ref(), content, path).await
}

pub async fn delete(path: &str) -> Result<(), String> {
    let parts = normalize(path)?;
    let dir = parent_of(&parts, false).await?;
    let f = method(&dir, "removeEntry")?;
    let p = as_promise(
        f.call1(&dir, &JsValue::from_str(parts.last().unwrap()))
            .map_err(|e| err("removeEntry threw", e))?,
        "removeEntry",
    )?;
    JsFuture::from(p)
        .await
        .map_err(|e| err(&format!("deleting {path}"), e))?;
    Ok(())
}

// ---- index -----------------------------------------------------------
// the index is the tree's table of contents and its dirty state. keeping it
// in one file means listing never depends on async directory iteration,
// which is the least consistently supported corner of the opfs api.

pub async fn load_index() -> Index {
    let Ok(r) = root().await else {
        return Index::new();
    };
    let Ok(handle) = child_file(&r, INDEX_FILE, false).await else {
        // absent index simply means a tree that has never been synced.
        return Index::new();
    };
    match handle_text(handle.as_ref(), INDEX_FILE).await {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Index::new(),
    }
}

pub async fn save_index(index: &Index) -> Result<(), String> {
    let r = root().await?;
    let handle = child_file(&r, INDEX_FILE, true).await?;
    let body = serde_json::to_string(index).map_err(|e| format!("serializing index: {e}"))?;
    handle_write(handle.as_ref(), &body, INDEX_FILE).await
}
