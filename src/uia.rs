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
            CUIAutomation, IUIAutomation, IUIAutomationElement6, IUIAutomationSelectionItemPattern, IUIAutomationValuePattern, TreeScope_Descendants, UIA_ButtonControlTypeId,
            UIA_DocumentControlTypeId, UIA_SelectionItemPatternId, UIA_TabItemControlTypeId, UIA_TreeItemControlTypeId, UIA_ValuePatternId,
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
                    c if c == UIA_TabItemControlTypeId.0 => "TabItem".to_owned(),
                    _ => e.CurrentLocalizedControlType().map(|b| b.to_string()).unwrap_or_default(),
                };
                let description = e.cast::<IUIAutomationElement6>().ok().and_then(|e| e.CurrentFullDescription().ok()).map(|b| b.to_string()).unwrap_or_default();
                let value = e.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId).ok().and_then(|p| p.CurrentValue().ok()).map(|b| b.to_string());
                let selected = e.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId).ok().and_then(|p| p.CurrentIsSelected().ok()).map(|b| b.as_bool());
                nodes.push(json!({ "type": kind, "name": e.CurrentName().map(|b| b.to_string()).unwrap_or_default(), "description": description, "value": value, "selected": selected }));
            }
            Ok(json!({ "window": window.CurrentName().map(|b| b.to_string()).unwrap_or_default(), "count": nodes.len(), "nodes": nodes }))
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the UIA client thread panicked"))?
}

/// Presses the first button or tab named `name` through UIA: its Invoke
/// pattern, else (a tab) its SelectionItem pattern's Select.
pub fn uia_invoke(hwnd: isize, name: &str) -> Result<()> {
    let name = name.to_owned();
    std::thread::spawn(move || -> Result<()> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
        use windows::Win32::UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern, TreeScope_Descendants, UIA_ButtonControlTypeId, UIA_InvokePatternId,
            UIA_SelectionItemPatternId, UIA_TabItemControlTypeId,
        };
        // SAFETY: COM calls on this thread's own apartment and objects.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let window = uia.ElementFromHandle(HWND(hwnd as *mut _))?;
            let all = window.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            for i in 0..all.Length()? {
                let e = all.GetElement(i)?;
                let control = e.CurrentControlType().map(|c| c.0);
                let pressable = control.is_ok_and(|c| c == UIA_ButtonControlTypeId.0 || c == UIA_TabItemControlTypeId.0);
                if pressable && e.CurrentName().map(|b| b.to_string()).is_ok_and(|n| n.trim() == name) {
                    return match e.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId) {
                        Ok(invoke) => Ok(invoke.Invoke()?),
                        Err(_) => Ok(e.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)?.Select()?),
                    };
                }
            }
            anyhow::bail!("no button or tab {name:?}")
        }
    })
    .join()
    .map_err(|_| anyhow::anyhow!("the UIA client thread panicked"))?
}
