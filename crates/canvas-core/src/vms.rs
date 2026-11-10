//! VM sessions in the client (docs/VM-SESSIONS.md): the marker of a box that
//! runs on a VM, and what shows only when the service has a VM pack (the
//! bus snapshot's `vm` view). Without a pack nothing here is drawn.

/// The mark of VM sessions (one cell wide in every console: see the test).
pub const VM_GLYPH: &str = "☁";
/// Before a VM session's name on its box.
pub const VM_PREFIX: &str = "☁ ";

/// The agents VM sessions run so far (Copilot CLI; docs/VM-SESSIONS-PLAN.md
/// Phase 5 adds the others).
pub fn runs_on_vm(kind: crate::agents::AgentKind) -> bool {
    kind == crate::agents::AgentKind::Copilot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vm_glyph_is_one_cell_wide() {
        use unicode_width::UnicodeWidthStr;
        assert_eq!(VM_GLYPH.width(), 1);
        assert_eq!(VM_PREFIX.width(), 2);
        assert_eq!(VM_GLYPH.chars().count(), 1, "no variation selector (that would ask for an emoji, two cells)");
    }
}
