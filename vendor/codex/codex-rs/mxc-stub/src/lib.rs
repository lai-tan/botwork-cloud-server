use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_protocol::models::PermissionProfile;

pub const CODEX_WINDOWS_MXC_ARG1: &str = "--__codex-windows-mxc";

pub fn is_available() -> bool {
    false
}

pub fn run_main() -> anyhow::Result<()> {
    anyhow::bail!("MXC sandbox not available in this build")
}

#[derive(Debug)]
pub struct CreateMxcCommandArgsParams<'a> {
    pub command: Vec<String>,
    pub permission_profile: &'a PermissionProfile,
    pub sandbox_policy_cwd: &'a Path,
    pub managed_network: Option<&'a ManagedNetworkSandboxContext>,
    pub env: &'a mut HashMap<String, String>,
}

pub fn create_command_args(_args: CreateMxcCommandArgsParams<'_>) -> Result<Vec<String>> {
    anyhow::bail!("MXC sandbox not available in this build")
}
