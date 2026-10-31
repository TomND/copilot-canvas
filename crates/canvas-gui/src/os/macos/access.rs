//! The accessibility tree as VoiceOver reads it (MAC-LINUX-PLAN §7.1): the
//! NSAccessibility protocol of the window's content view, where AccessKit
//! answers. The hardening check walks it in-process through the test hooks:
//! a client in another process would need the Accessibility permission (a
//! prompt), and VoiceOver's AX calls end in these same methods.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, msg_send_id};
use objc2_foundation::{NSArray, NSString};
use serde_json::{Value, json};

/// One element's text property (strings; numbers and others as their description).
fn text(object: &AnyObject, property: &str) -> Option<String> {
    // SAFETY: NSAccessibility getters on objects AppKit or AccessKit hands out
    // (the selectors are the protocol's; a nil answer is None).
    unsafe {
        let value: Option<Retained<AnyObject>> = match property {
            "role" => msg_send_id![object, accessibilityRole],
            "subrole" => msg_send_id![object, accessibilitySubrole],
            "label" => msg_send_id![object, accessibilityLabel],
            "title" => msg_send_id![object, accessibilityTitle],
            "help" => msg_send_id![object, accessibilityHelp],
            "value" => msg_send_id![object, accessibilityValue],
            _ => None,
        };
        let value = value?;
        let is_string: bool = msg_send![&*value, isKindOfClass: objc2::class!(NSString)];
        let text: Retained<NSString> = if is_string { Retained::cast(value) } else { msg_send_id![&*value, description] };
        Some(text.to_string()).filter(|t| !t.is_empty())
    }
}

fn children(object: &AnyObject) -> Vec<Retained<AnyObject>> {
    // SAFETY: as above; the array holds accessibility elements.
    unsafe {
        let responds: bool = msg_send![object, respondsToSelector: objc2::sel!(accessibilityChildren)];
        if !responds {
            return Vec::new();
        }
        let list: Option<Retained<NSArray<AnyObject>>> = msg_send_id![object, accessibilityChildren];
        list.map(|l| l.to_vec_retained()).unwrap_or_default()
    }
}

/// UIA's names for the roles the check looks for (as on Windows and Linux).
fn kind(role: &str, subrole: &str) -> String {
    match (role, subrole) {
        ("AXButton", _) => "Button",
        ("AXRow" | "AXOutlineRow", _) | (_, "AXOutlineRow") => "TreeItem",
        ("AXTextArea" | "AXDocument" | "AXWebArea", _) => "Document",
        ("AXSheet" | "AXDialog", _) | (_, "AXDialog" | "AXSystemDialog") => "dialog",
        _ => role,
    }
    .to_owned()
}

/// Walks the content view `view`'s accessibility tree.
fn walk(view: isize, mut each: impl FnMut(&AnyObject, &Value) -> bool) -> Value {
    // SAFETY: the window's live content view, on the main thread (hooks run there).
    let Some(root) = (unsafe { (view as *const AnyObject).as_ref() }) else { return json!({ "count": 0, "nodes": [] }) };
    let mut nodes = Vec::new();
    let mut stack = children(root);
    stack.reverse();
    while let Some(element) = stack.pop() {
        let (role, subrole) = (text(&element, "role").unwrap_or_default(), text(&element, "subrole").unwrap_or_default());
        let node = json!({
            "type": kind(&role, &subrole),
            "role": role,
            "subrole": subrole,
            "name": text(&element, "label").or_else(|| text(&element, "title")),
            "description": text(&element, "help"),
            "value": text(&element, "value"),
        });
        if !each(&element, &node) {
            break;
        }
        nodes.push(node);
        let mut more = children(&element);
        more.reverse();
        stack.extend(more);
        if nodes.len() > 5000 {
            break;
        }
    }
    json!({ "count": nodes.len(), "nodes": nodes })
}

/// The tree (`{"count", "nodes": [{type, role, name, description, value}]}`).
pub fn snapshot(view: isize) -> Value {
    walk(view, |_, _| true)
}

/// Presses the button named `name` (`accessibilityPerformPress`, as VoiceOver's VO-Space).
pub fn press(view: isize, name: &str) -> bool {
    let mut pressed = false;
    walk(view, |element, node| {
        if node["type"] == "Button" && node["name"].as_str().is_some_and(|n| n.trim() == name) {
            // SAFETY: an accessibility element's press action.
            pressed = unsafe { msg_send![element, accessibilityPerformPress] };
            return false;
        }
        true
    });
    pressed
}
