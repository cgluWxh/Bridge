//! Phone notifications as Windows toasts, with a reply box and swipe-away
//! that reach the phone.
//!
//! Built directly on Windows.UI.Notifications rather than a wrapper crate:
//! the reply box (a toast `<input>`), the Activated event that hands us what
//! was typed, and the Dismissed event (swiped away → dismissed on the phone
//! too) aren't exposed by the wrappers.
//!
//! Unpackaged apps need an AppUserModelID registered in the Start Menu to
//! toast; this borrows PowerShell's, which works everywhere without an
//! installer. The toast's events still come to this process while it runs.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use windows::core::{Interface, HSTRING};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::{IPropertyValue, TypedEventHandler};
use windows::UI::Notifications::{
    ToastActivatedEventArgs, ToastDismissalReason, ToastDismissedEventArgs, ToastNotification, ToastNotificationManager,
};

use crate::ble::Event;

const APP_ID: &str = "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";
const GROUP: &str = "bridge";

/// Toasts on screen, by phone id. Kept so their event handlers stay alive,
/// and so a toast we remove on the phone's say-so isn't reported back as
/// "dismissed by the user".
static SHOWN: Mutex<Option<HashMap<u32, ToastNotification>>> = Mutex::new(None);

/// Where the phone's app icons are cached, one PNG per package.
pub fn icon_dir() -> std::path::PathBuf {
    crate::store::data_dir().join("icons")
}

pub fn icon_path(package: &str) -> std::path::PathBuf {
    icon_dir().join(format!("{package}.png"))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A plain toast from Bridge itself (file done, pairing…).
pub fn show(app: &str, title: &str, body: &str) {
    let xml = format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text><text>{}</text></binding></visual><audio silent="true"/></toast>"#,
        escape(app), escape(title), escape(body)
    );
    if let Err(e) = deliver(&xml, None, None) {
        crate::log!("notify", "toast failed: {e}");
    }
}

/// Keep local transfer callbacks alive separately from mirrored notifications,
/// so dismissing a file toast never dismisses a notification on the phone.
static RECEIVED: Mutex<std::collections::VecDeque<ToastNotification>> = Mutex::new(std::collections::VecDeque::new());

/// One summary per completed batch. Capture the actual saved path, not a path
/// supplied through toast arguments (and not the original collision-prone name).
pub fn show_received(report: &crate::incoming::Report, events: Sender<Event>) {
    let Some(first) = report.saved.first() else { return };
    let target = first.clone();
    let name = first.file_name().unwrap_or_default().to_string_lossy();
    let title = if report.saved.len() == 1 { "File received".to_string() }
        else { format!("{} files received", report.saved.len()) };
    let body = if report.saved.len() == 1 { name.into_owned() }
        else { format!("{name} and {} more files", report.saved.len() - 1) };
    let failures = if report.failed == 0 { String::new() } else { format!(" · {} failed", report.failed) };
    let hint = if report.saved.len() == 1 { "Click to show the file." } else { "Click to show the first file." };
    let xml = format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>Bridge · {}</text><text>{}</text><text>Saved to Downloads{}. {hint}</text></binding></visual><actions><action content="Show in folder" arguments="reveal" activationType="foreground"/></actions></toast>"#,
        escape(&title), escape(&body), escape(&failures)
    );
    let result = (|| -> windows::core::Result<()> {
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        toast.Activated(&TypedEventHandler::new(move |_: &Option<ToastNotification>, _: &Option<windows::core::IInspectable>| {
            let _ = events.send(Event::RevealReceived(target.clone()));
            Ok(())
        }))?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
        let mut shown = RECEIVED.lock().unwrap();
        shown.push_back(toast);
        while shown.len() > 100 { shown.pop_front(); }
        Ok(())
    })();
    if let Err(e) = result { crate::log!("notify", "file toast failed: {e}"); }
}

/// Open Explorer and select, never open/execute the received file itself.
/// Separate arguments avoid shell interpretation of spaces and metacharacters.
pub fn reveal_received(path: &std::path::Path) -> std::io::Result<()> {
    let mut command = std::process::Command::new("explorer.exe");
    if path.is_file() {
        command.arg("/select,").arg(path);
    } else {
        command.arg(path.parent().filter(|p| p.is_dir()).map(|p| p.to_path_buf())
            .unwrap_or_else(crate::files::downloads_dir));
    }
    command.spawn().map(|_| ())
}

/// One phone notification. With `id`, the same id replaces the toast (a chat's
/// next message), "reply" puts a text box on it when the app supports replies,
/// and swiping it away dismisses it on the phone.
pub fn show_phone(n: &crate::protocol::PhoneNotification, events: Sender<Event>) {
    let icon = (!n.package.is_empty())
        .then(|| icon_path(&n.package))
        .filter(|p| p.exists())
        .map(|p| format!(r#"<image placement="appLogoOverride" hint-crop="circle" src="{}"/>"#, escape(&p.to_string_lossy())))
        .unwrap_or_default();
    let actions = if n.replyable && n.id != 0 {
        r#"<actions><input id="reply" type="text" placeHolderContent="Reply"/><action content="Send" arguments="reply" hint-inputId="reply" activationType="foreground"/></actions>"#
    } else {
        ""
    };
    // App name first, as on the Mac: it's the most useful thing to see.
    let xml = format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text><text>{}</text>{icon}</binding></visual>{actions}<audio silent="true"/></toast>"#,
        escape(&n.app), escape(&n.title), escape(&n.body)
    );
    let id = (n.id != 0).then_some(n.id);
    if let Err(e) = deliver(&xml, id, Some(events)) {
        crate::log!("notify", "toast failed: {e}");
    }
}

/// The phone says it's gone: take the toast down (from the screen and from
/// Action Center).
pub fn remove(id: u32) {
    let had = SHOWN.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id).is_some();
    if !had {
        return;
    }
    if let Ok(history) = ToastNotificationManager::History() {
        let _ = history.RemoveGroupedTagWithId(&HSTRING::from(id.to_string()), &HSTRING::from(GROUP), &HSTRING::from(APP_ID));
    }
}

fn deliver(xml: &str, id: Option<u32>, events: Option<Sender<Event>>) -> windows::core::Result<()> {
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    let untagged_phone = id.is_none() && events.is_some();
    if let Some(tx) = events.clone() {
        toast.Activated(&TypedEventHandler::new(move |_: &Option<ToastNotification>, args: &Option<windows::core::IInspectable>| {
            let Some(args) = args else { return Ok(()) };
            let Ok(args) = args.cast::<ToastActivatedEventArgs>() else { return Ok(()) };
            let action = args.Arguments()?.to_string();
            if action == "reply" {
                let Some(id) = id else { return Ok(()) };
                let text = args
                    .UserInput()
                    .and_then(|input| input.Lookup(&HSTRING::from("reply")))
                    .and_then(|v| v.cast::<IPropertyValue>())
                    .and_then(|v| v.GetString())
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    let _ = tx.send(Event::ToastReply { id, text });
                }
            } else if action.is_empty() {
                // Only the body click: replying never starts a mirror session.
                let _ = tx.send(Event::NotificationClicked);
            }
            Ok(())
        }))?;
    }
    if let (Some(id), Some(events)) = (id, events) {
        toast.SetTag(&HSTRING::from(id.to_string()))?;
        toast.SetGroup(&HSTRING::from(GROUP))?;
        let tx = events;
        toast.Dismissed(&TypedEventHandler::new(move |_: &Option<ToastNotification>, args: &Option<ToastDismissedEventArgs>| {
            // Only the user swiping it away counts; timing out into Action
            // Center, or us removing it because the phone did, doesn't.
            if let Some(args) = args {
                if args.Reason()? == ToastDismissalReason::UserCanceled {
                    let still_ours = SHOWN.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id).is_some();
                    if still_ours {
                        let _ = tx.send(Event::ToastDismissed(id));
                    }
                }
            }
            Ok(())
        }))?;

        let mut shown = SHOWN.lock().unwrap();
        let map = shown.get_or_insert_with(HashMap::new);
        map.insert(id, toast.clone());
        if map.len() > 100 {
            // Oldest ids first: they're long gone from the screen.
            let mut ids: Vec<u32> = map.keys().copied().collect();
            ids.sort();
            for old in ids.into_iter().take(map.len() - 100) {
                map.remove(&old);
            }
        }
    }
    if untagged_phone {
        // Even a phone notification without an Android id has a clickable body.
        static UNTAGGED: Mutex<std::collections::VecDeque<ToastNotification>> = Mutex::new(std::collections::VecDeque::new());
        let mut shown = UNTAGGED.lock().unwrap();
        shown.push_back(toast.clone());
        while shown.len() > 100 { shown.pop_front(); }
    }
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))?.Show(&toast)?;
    Ok(())
}
