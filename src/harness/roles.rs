//! Role Binding load/defaults (machine `config.json`).

use std::collections::BTreeMap;

pub use crate::config::{RoleBinding, default_role_bindings, load_machine_config};

use crate::error::Result;

/// Load role bindings from machine config (defaults fill a missing field).
pub fn load_role_bindings() -> Result<BTreeMap<String, RoleBinding>> {
    Ok(load_machine_config()?.role_bindings)
}

/// Command to spawn for Grok: `implementor` binding, else `planner`, else `"grok"`.
pub fn resolve_grok_command() -> Result<String> {
    let bindings = load_role_bindings()?;
    if let Some(b) = bindings.get("implementor")
        && !b.command.is_empty()
    {
        return Ok(b.command.clone());
    }
    if let Some(b) = bindings.get("planner")
        && !b.command.is_empty()
    {
        return Ok(b.command.clone());
    }
    Ok("grok".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_map_planner_and_implementor_to_grok() {
        let b = default_role_bindings();
        assert_eq!(b["planner"].harness, "grok");
        assert_eq!(b["implementor"].harness, "grok");
        assert_eq!(b["planner"].command, "grok");
        assert!(b["planner"].model.is_none());
        assert!(!b.contains_key("antigravity"));
    }
}
