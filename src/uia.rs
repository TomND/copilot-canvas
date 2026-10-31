//! UI Automation, the Windows accessibility API, read from outside the GUI
//! (the hardening check; macOS and Linux use the in-process `access` hook,
//! MAC-LINUX-PLAN §7.1).

use anyhow::Result;
use serde_json::{Value, json};

/// The window's UI Automation elements as a UIA client sees them (the
/// `IUIAutomation` COM API: what Narrator and test tools use): control type,
/// name, full description and value.
pub fn uia_snapshot(hwnd: isize) -> Result<Value> {
    std::thread::spawn(move || -> Result<Value> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationElement6, IUIAutomationValuePattern, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_DocumentControlTypeId,
            UIA_TreeItemControlTypeId, UIA_ValuePatternId,
        };
        use windows::core::Interface;
        // SAFETY: COM calls on this thread's own apartment and objects.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let window = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
            let all = window.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            let mut nodes = Vec::new();
            for i in 0..all.Length()? {
                let e = all.GetElement(i)?;
                let control = e.CurrentControlType().map(|c| c.0).unwrap_or(0);
                let kind = match control {
                    c if c == UIA_ButtonControlTypeId.0 => "Button".to_owned(),
                    c if c == UIA_TreeItemControlTypeId.0 => "TreeItem".to_owned(),
                    c if c == UIA_DocumentControlTypeId.0 => "Document".to_owned(),
                    _ => e.CurrentLocalizedControlType().map(|b| b.to_string()).unwrap_or_default(),
                };
                let description = e.cast::<IUIAutomationElement6>().ok().and_then(|e| e.CurrentFullDescription().ok()).map(|b| b.to_string()).unwrap_or_default();
                let value = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok().and_then(|p| p.CurrentValue().ok()).map(|b| b.to_string());
                nodes.push(json!({ "type": kind, "name": e.CurrentName().map(|b| b.to_string()).unwrap_or_default(), "description": description, "value": value }));
            }
            Ok(json!({ "window": window.CurrentName().map(|b| b.to_string()).unwrap_or_default(), "count": nodes.len(), "nodes": nodes }))
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the UIA client thread panicked"))?
}

/// Presses the first button named `name` through UIA's Invoke pattern.
pub fn uia_invoke(hwnd: isize, name: &str) -> Result<()> {
    let name = name.to_owned();
    std::thread::spawn(move || -> Result<()> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationInvokePattern, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_InvokePatternId};
        // SAFETY: COM calls on this thread's own apartment and objects.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let window = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
            let all = window.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            for i in 0..all.Length()? {
                let e = all.GetElement(i)?;
                if e.CurrentControlType().map(|c| c.0) == Ok(UIA_ButtonControlTypeId.0) && e.CurrentName().map(|b| b.to_string()).is_ok_and(|n| n.trim() == name) {
                    e.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)?.Invoke()?;
                    return Ok(());
                }
            }
            anyhow::bail!("no button {name:?}")
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the UIA client thread panicked"))?
}
