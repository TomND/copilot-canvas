//! The menu bar (MAC-LINUX-PLAN D8, §5.6) in place of winit's default one,
//! whose Quit ends the app at once. Quit (⌘Q, the Dock's Quit) asks, as
//! closing the last window does on Windows: detach and keep the sessions
//! running, or stop them all; with several windows the others follow the
//! one that asked. Logging out, restarting or shutting down quits without
//! asking (the service keeps the sessions). The Edit and Window items with
//! the canvas's own keys (⌘Z, ⌘C, ⌘V, ⇧⌘N …) act exactly as those keys do;
//! while a report's page has the keyboard the Edit items are the page's
//! (copy its text).

use std::sync::Once;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, mutability, sel};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSObject, NSString};

use crate::shell::{MenuCommand, menu_command};

/// Apple Events: `kCoreEventClass`, `kAEQuitApplication`, `keyAEQuitReason`
/// and the reasons a log out, restart or shut down gives.
const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const QUIT_APPLICATION: u32 = u32::from_be_bytes(*b"quit");
const QUIT_REASON: u32 = u32::from_be_bytes(*b"why?");
const SYSTEM_REASONS: [&[u8; 4]; 6] = [b"logo", b"rlgo", b"rrst", b"rsdn", b"rest", b"shut"];

declare_class!(
    /// Where the menu's own items land.
    struct MenuTarget;

    unsafe impl ClassType for MenuTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "CopilotCanvasMenuTarget";
    }

    impl DeclaredClass for MenuTarget {
        type Ivars = ();
    }

    unsafe impl MenuTarget {
        #[method(quit:)]
        fn quit(&self, _sender: Option<&AnyObject>) {
            menu_command(MenuCommand::Quit);
        }

        #[method(showHelp:)]
        fn show_help(&self, _sender: Option<&AnyObject>) {
            menu_command(MenuCommand::Help);
        }

        #[method(showSettings:)]
        fn show_settings(&self, _sender: Option<&AnyObject>) {
            menu_command(MenuCommand::Settings);
        }

        #[method(newWindow:)]
        fn new_window(&self, _sender: Option<&AnyObject>) {
            menu_command(MenuCommand::Key('n', true));
        }

        #[method(undo:)]
        fn undo(&self, _sender: Option<&AnyObject>) {
            menu_command(MenuCommand::Key('z', false));
        }

        #[method(redo:)]
        fn redo(&self, _sender: Option<&AnyObject>) {
            menu_command(MenuCommand::Key('z', true));
        }

        #[method(cut:)]
        fn cut(&self, sender: Option<&AnyObject>) {
            edit(sel!(cut:), sender, 'x');
        }

        #[method(copy:)]
        fn copy(&self, sender: Option<&AnyObject>) {
            edit(sel!(copy:), sender, 'c');
        }

        #[method(paste:)]
        fn paste(&self, sender: Option<&AnyObject>) {
            edit(sel!(paste:), sender, 'v');
        }

        #[method(selectAll:)]
        fn select_all(&self, sender: Option<&AnyObject>) {
            edit(sel!(selectAll:), sender, 'a');
        }

        /// The 'quit' Apple Event (the Dock's Quit, a log out).
        #[method(handleQuitEvent:withReplyEvent:)]
        fn quit_event(&self, event: &AnyObject, _reply: &AnyObject) {
            // SAFETY: the NSAppleEventDescriptor AppKit hands over; plain reads.
            let reason: u32 = unsafe {
                let why: Option<Retained<AnyObject>> = msg_send_id![event, attributeDescriptorForKeyword: QUIT_REASON];
                why.map_or(0, |w| msg_send![&*w, enumCodeValue])
            };
            if SYSTEM_REASONS.iter().any(|r| u32::from_be_bytes(**r) == reason) {
                if let Some(mtm) = MainThreadMarker::new() {
                    // SAFETY: AppKit's own termination (winit saves nothing it needs; the service keeps the sessions).
                    unsafe { NSApplication::sharedApplication(mtm).terminate(None) };
                }
                return;
            }
            menu_command(MenuCommand::Quit);
        }
    }
);

/// An Edit item: the report page's when it has the keyboard, else the canvas key.
fn edit(action: Sel, sender: Option<&AnyObject>, key: char) {
    if let Some(mtm) = MainThreadMarker::new()
        && let Some(window) = NSApplication::sharedApplication(mtm).keyWindow()
        && let Some(responder) = window.firstResponder()
    {
        // SAFETY: class and selector checks before the message.
        unsafe {
            let web: bool = AnyObject::class(&responder).name().contains("ReportView");
            let answers: bool = msg_send![&*responder, respondsToSelector: action];
            if web && answers {
                let _: () = msg_send![&*responder, performSelector: action, withObject: sender];
                return;
            }
        }
    }
    menu_command(MenuCommand::Key(key, false));
}

fn item(mtm: MainThreadMarker, title: &str, action: Option<Sel>, key: &str, mods: NSEventModifierFlags, target: Option<&MenuTarget>) -> Retained<NSMenuItem> {
    // SAFETY: a new menu item on the main thread.
    let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(mtm.alloc(), &NSString::from_str(title), action, &NSString::from_str(key)) };
    if !key.is_empty() {
        item.setKeyEquivalentModifierMask(mods);
    }
    if let Some(target) = target {
        // SAFETY: the target lives as long as the app (leaked below).
        unsafe { item.setTarget(Some(target)) };
    }
    item
}

fn submenu(mtm: MainThreadMarker, bar: &NSMenu, title: &str, items: Vec<Retained<NSMenuItem>>) -> Retained<NSMenu> {
    // SAFETY: new menus on the main thread.
    let menu = unsafe { NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(title)) };
    for i in &items {
        menu.addItem(i);
    }
    let holder = item(mtm, title, None, "", NSEventModifierFlags::empty(), None);
    holder.setSubmenu(Some(&menu));
    bar.addItem(&holder);
    menu
}

/// Puts the menu bar in place (once, when the first window opens) and takes
/// the 'quit' Apple Event.
pub fn install() {
    static ONCE: Once = Once::new();
    let Some(mtm) = MainThreadMarker::new() else { return };
    ONCE.call_once(|| {
        let target: Retained<MenuTarget> = {
            let this = mtm.alloc::<MenuTarget>().set_ivars(());
            // SAFETY: NSObject's init.
            unsafe { msg_send_id![super(this), init] }
        };
        let t = Some(&*target);
        let cmd = NSEventModifierFlags::NSEventModifierFlagCommand;
        let cmd_shift = cmd | NSEventModifierFlags::NSEventModifierFlagShift;
        let cmd_alt = cmd | NSEventModifierFlags::NSEventModifierFlagOption;
        let none = NSEventModifierFlags::empty();
        let sep = || NSMenuItem::separatorItem(mtm);
        // SAFETY: a new menu on the main thread.
        let bar = unsafe { NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str("")) };
        submenu(
            mtm,
            &bar,
            "Copilot Canvas",
            vec![
                item(mtm, "About Copilot Canvas", Some(sel!(orderFrontStandardAboutPanel:)), "", none, None),
                sep(),
                item(mtm, "Settings…", Some(sel!(showSettings:)), ",", cmd, t),
                sep(),
                item(mtm, "Hide Copilot Canvas", Some(sel!(hide:)), "h", cmd, None),
                item(mtm, "Hide Others", Some(sel!(hideOtherApplications:)), "h", cmd_alt, None),
                item(mtm, "Show All", Some(sel!(unhideAllApplications:)), "", none, None),
                sep(),
                item(mtm, "Quit Copilot Canvas…", Some(sel!(quit:)), "q", cmd, t),
            ],
        );
        submenu(
            mtm,
            &bar,
            "Edit",
            vec![
                item(mtm, "Undo", Some(sel!(undo:)), "z", cmd, t),
                item(mtm, "Redo", Some(sel!(redo:)), "z", cmd_shift, t),
                sep(),
                item(mtm, "Cut", Some(sel!(cut:)), "x", cmd, t),
                item(mtm, "Copy", Some(sel!(copy:)), "c", cmd, t),
                item(mtm, "Paste", Some(sel!(paste:)), "v", cmd, t),
                item(mtm, "Select All", Some(sel!(selectAll:)), "a", cmd, t),
            ],
        );
        let window = submenu(
            mtm,
            &bar,
            "Window",
            vec![
                item(mtm, "Minimize", Some(sel!(performMiniaturize:)), "m", cmd, None),
                item(mtm, "Zoom", Some(sel!(performZoom:)), "", none, None),
                sep(),
                item(mtm, "New Window", Some(sel!(newWindow:)), "n", cmd_shift, t),
                sep(),
                item(mtm, "Bring All to Front", Some(sel!(arrangeInFront:)), "", none, None),
            ],
        );
        let help = submenu(mtm, &bar, "Help", vec![item(mtm, "Copilot Canvas Help", Some(sel!(showHelp:)), "?", cmd, t)]);
        let app = NSApplication::sharedApplication(mtm);
        app.setMainMenu(Some(&bar));
        // SAFETY: AppKit's menus and Apple Event handler, on the main thread;
        // the target is leaked so it outlives both.
        unsafe {
            app.setWindowsMenu(Some(&window));
            app.setHelpMenu(Some(&help));
            let manager: Retained<AnyObject> = msg_send_id![objc2::class!(NSAppleEventManager), sharedAppleEventManager];
            let _: () = msg_send![&*manager, setEventHandler: &*target, andSelector: sel!(handleQuitEvent:withReplyEvent:), forEventClass: CORE_EVENT_CLASS, andEventID: QUIT_APPLICATION];
        }
        std::mem::forget(target);
    });
}
