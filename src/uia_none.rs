//! No outside accessibility client here: the hardening check reads the tree
//! through the GUI's in-process `access` hook instead (MAC-LINUX-PLAN §7.1).

use anyhow::{Result, bail};
use serde_json::Value;

pub fn uia_snapshot(_window: isize) -> Result<Value> {
    bail!("UI Automation is Windows-only; use the in-process access hook")
}

pub fn uia_invoke(_window: isize, _name: &str) -> Result<()> {
    bail!("UI Automation is Windows-only; use the in-process access hook")
}
