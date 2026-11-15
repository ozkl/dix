//! Browser host access: uploads through the file picker, downloads as blobs.
//! There is no host file system, so images live in memory while they are open.

use super::{DropTarget, EventSender, HostEvent, ImageSource};
use crate::model::VolumeId;
use dix_core::ops::Bundle;
use dix_core::VfsPath;
use wasm_bindgen::JsCast as _;

/// The picked file goes into the open dialog, as a dropped one does: its
/// partitions are listed there before it is opened.
pub fn pick_image(tx: EventSender) {
    wasm_bindgen_futures::spawn_local(async move {
        let Some(file) = rfd::AsyncFileDialog::new().set_title("Open file system image").pick_file().await else {
            return;
        };
        let name = file.file_name();
        let data = file.read().await;
        tx.send(HostEvent::ImageDropped(ImageSource::Bytes { name, data }));
    });
}

pub fn pick_import_files(tx: EventSender, volume: VolumeId, dir: VfsPath) {
    wasm_bindgen_futures::spawn_local(async move {
        let Some(files) = rfd::AsyncFileDialog::new().set_title("Import files").pick_files().await else { return };
        let mut items = Vec::new();
        for f in files {
            items.push((f.file_name(), f.read().await));
        }
        tx.deliver(DropTarget::Import { volume, dir }, items);
    });
}

pub fn export(tx: EventSender, items: Vec<Bundle>) {
    let mut skipped = 0;
    let mut n = 0;
    for item in &items {
        match item {
            Bundle::File { name, data, .. } => {
                if let Err(e) = download(name, data) {
                    return tx.error(format!("Download failed: {e}"));
                }
                n += 1;
            }
            Bundle::Dir { .. } => skipped += 1,
        }
    }
    if skipped > 0 {
        tx.error(format!("{skipped} folder(s) skipped: browsers can only download single files"));
    } else {
        tx.info(format!("Downloading {n} file(s)"));
    }
}

pub fn save_bytes(tx: EventSender, name: &str, bytes: Vec<u8>) {
    match download(name, &bytes) {
        Ok(()) => tx.info(format!("Downloading {name}")),
        Err(e) => tx.error(format!("Download failed: {e}")),
    }
}

pub fn read_dropped(tx: EventSender, files: Vec<eframe::egui::DroppedFileHandle>, target: DropTarget) {
    wasm_bindgen_futures::spawn_local(async move {
        let mut out = Vec::new();
        for f in files {
            let name = f.path().file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            match f.bytes_async().await {
                Ok(bytes) => out.push((name, bytes)),
                Err(e) => return tx.error(format!("{name}: {e}")),
            }
        }
        tx.deliver(target, out);
    });
}

/// Triggers a browser download of `data`.
fn download(name: &str, data: &[u8]) -> Result<(), String> {
    let err = |e: wasm_bindgen::JsValue| format!("{e:?}");
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let array = js_sys::Uint8Array::from(data);
    let parts = js_sys::Array::new();
    parts.push(&array);
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("application/octet-stream");
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts).map_err(err)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(err)?;
    let a: web_sys::HtmlAnchorElement =
        document.create_element("a").map_err(err)?.dyn_into().map_err(|_| "not an anchor")?;
    a.set_href(&url);
    a.set_download(name);
    a.click();
    // Revoke later so the browser has time to start the download.
    let revoke = wasm_bindgen::closure::Closure::once_into_js(move || {
        let _ = web_sys::Url::revoke_object_url(&url);
    });
    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 10_000);
    Ok(())
}

pub fn set_leave_warning(enabled: bool) {
    let Some(window) = web_sys::window() else { return };
    if enabled {
        let handler = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::Event)>::new(|e: web_sys::Event| {
            // Modern browsers show their own generic "Leave site?" text.
            e.prevent_default();
        });
        window.set_onbeforeunload(Some(handler.as_ref().unchecked_ref()));
        handler.forget(); // tiny, and replaced at most a few times per session
    } else {
        window.set_onbeforeunload(None);
    }
}
