//! Notifications through UserNotifications (MAC-LINUX-PLAN §5.6). macOS
//! delivers them only for an app bundle and with the user's permission (the
//! first toast asks). Outside the bundle (the bare binaries, tests, CI)
//! nothing is shown and the toast records why; so does a toast while
//! notifications are off for the app.

use std::ptr::NonNull;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSArray, NSBundle, NSError, NSString, NSUUID};
use objc2_user_notifications::{UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification, UNNotificationRequest, UNNotificationSettings, UNUserNotificationCenter};

const NOT_BUNDLED: &str = "not shown: macOS shows notifications only for the app bundle (Atelier.app)";
const TURNED_OFF: &str = "not shown: notifications are off for Atelier (System Settings › Notifications)";

/// Running from `Atelier.app` (a bundle identifier, a `.app` folder).
fn bundled() -> bool {
    let bundle = NSBundle::mainBundle();
    // SAFETY: plain property reads of the main bundle.
    unsafe { bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app") }
}

/// Asks for permission once (the first toast; the answer is waited for on
/// the toast thread, never on the UI thread).
pub fn toast_thread_init() -> Result<(), String> {
    if !bundled() {
        return Err(NOT_BUNDLED.into());
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    let block = RcBlock::new(move |granted: Bool, _error: *mut NSError| {
        let _ = tx.send(granted.as_bool());
    });
    // SAFETY: the bundle's notification center; the block is reference counted.
    unsafe {
        let options = UNAuthorizationOptions::UNAuthorizationOptionAlert | UNAuthorizationOptions::UNAuthorizationOptionSound | UNAuthorizationOptions::UNAuthorizationOptionBadge;
        UNUserNotificationCenter::currentNotificationCenter().requestAuthorizationWithOptions_completionHandler(options, &block);
    }
    // Whatever the answer, each toast checks the settings again (they change).
    let _ = rx.recv_timeout(Duration::from_secs(120));
    Ok(())
}

fn authorized() -> bool {
    let (tx, rx) = crossbeam_channel::bounded(1);
    let block = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: the settings object the center hands over.
        let status = unsafe { settings.as_ref().authorizationStatus() };
        let _ = tx.send(matches!(status, UNAuthorizationStatus::Authorized | UNAuthorizationStatus::Provisional | UNAuthorizationStatus::Ephemeral));
    });
    // SAFETY: as above.
    unsafe { UNUserNotificationCenter::currentNotificationCenter().getNotificationSettingsWithCompletionHandler(&block) };
    rx.recv_timeout(Duration::from_secs(10)).unwrap_or(false)
}

pub fn show_toast(title: &str, body: &str) -> Result<(), String> {
    if !bundled() {
        return Err(NOT_BUNDLED.into());
    }
    if !authorized() {
        return Err(TURNED_OFF.into());
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    let block = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the error (or null) the center hands over.
        let _ = tx.send(unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string()));
    });
    // SAFETY: a new content and request, added to the bundle's center.
    unsafe {
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        let id = NSUUID::UUID().UUIDString();
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, Some(&block));
    }
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(error)) => Err(error),
        Err(_) => Err("macOS did not answer".into()),
    }
}

/// The app's notifications in Notification Center (`None` outside the bundle).
pub fn toast_history_count() -> Option<u32> {
    if !bundled() {
        return None;
    }
    let (tx, rx) = crossbeam_channel::bounded(1);
    let block = RcBlock::new(move |delivered: NonNull<NSArray<UNNotification>>| {
        // SAFETY: the array the center hands over.
        let _ = tx.send(unsafe { delivered.as_ref() }.count() as u32);
    });
    // SAFETY: as above.
    unsafe { UNUserNotificationCenter::currentNotificationCenter().getDeliveredNotificationsWithCompletionHandler(&block) };
    rx.recv_timeout(Duration::from_secs(5)).ok()
}
