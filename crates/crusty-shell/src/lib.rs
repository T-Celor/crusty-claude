//! Bash tool: command risk classification, background shells, secret-free environment, RTK hand-off.

pub mod background;
pub mod bash;
pub mod env;
pub mod risk;
pub mod rtk_external;
mod sandbox;
pub mod shell;

pub use background::{BackgroundShells, BashOutputTool, KillShellTool, ShellStatus, shell_tools};
pub use bash::BashTool;
pub use env::scrubbed_env;
pub use risk::{ClassifyEnv, classify_command, classify_command_in};
pub use rtk_external::{find_binary, rtk_rewrite};
pub use sandbox::{Keep, Launch, SandboxSetup};
pub use shell::{ShellSpec, detect_shell};
