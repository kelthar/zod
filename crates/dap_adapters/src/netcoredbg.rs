use anyhow::{Result, bail};
use async_trait::async_trait;
use collections::HashMap;
use dap::{StartDebuggingRequestArguments, adapters::DebugTaskDefinition, netcoredbg};
use gpui::AsyncApp;
use language::LanguageName;
use std::{ffi::OsStr, path::PathBuf};
use task::{DebugScenario, ZedDebugConfig};

use crate::*;

/// .NET debugging through Samsung's netcoredbg. The binary is not downloaded here: the debugger
/// panel asks the user before downloading it (see [`dap::netcoredbg`]).
#[derive(Default)]
pub(crate) struct NetCoreDbgDebugAdapter;

#[async_trait(?Send)]
impl DebugAdapter for NetCoreDbgDebugAdapter {
    fn name(&self) -> DebugAdapterName {
        DebugAdapterName(netcoredbg::ADAPTER_NAME.into())
    }

    fn adapter_language_name(&self) -> Option<LanguageName> {
        Some(LanguageName::new_static("C#"))
    }

    async fn config_from_zed_format(&self, zed_scenario: ZedDebugConfig) -> Result<DebugScenario> {
        let mut config = serde_json::Map::default();
        match &zed_scenario.request {
            dap::DebugRequest::Attach(attach) => {
                config.insert("request".into(), "attach".into());
                if let Some(process_id) = attach.process_id {
                    config.insert("processId".into(), process_id.into());
                }
            }
            dap::DebugRequest::Launch(launch) => {
                config.insert("request".into(), "launch".into());
                config.insert("program".into(), launch.program.clone().into());
                if !launch.args.is_empty() {
                    config.insert("args".into(), launch.args.clone().into());
                }
                if !launch.env.is_empty() {
                    config.insert("env".into(), launch.env_json());
                }
                if let Some(cwd) = launch.cwd.as_ref() {
                    config.insert("cwd".into(), cwd.to_string_lossy().into_owned().into());
                }
                if let Some(stop_on_entry) = zed_scenario.stop_on_entry {
                    config.insert("stopAtEntry".into(), stop_on_entry.into());
                }
            }
        }

        Ok(DebugScenario {
            adapter: zed_scenario.adapter,
            label: zed_scenario.label,
            build: None,
            config: serde_json::Value::Object(config),
            tcp_connection: None,
        })
    }

    fn dap_schema(&self) -> serde_json::Value {
        json!({
            "oneOf": [
                {
                    "type": "object",
                    "required": ["request", "program"],
                    "properties": {
                        "request": { "type": "string", "enum": ["launch"] },
                        "program": {
                            "type": "string",
                            "description": "Path to the application dll, e.g. bin/Debug/net10.0/App.dll"
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Command line arguments passed to the program"
                        },
                        "cwd": { "type": "string", "description": "Working directory of the program" },
                        "env": { "type": "object", "description": "Environment variables of the program" },
                        "stopAtEntry": {
                            "type": "boolean",
                            "description": "Stop at the entry point of the program",
                            "default": false
                        },
                        "justMyCode": {
                            "type": "boolean",
                            "description": "Only debug user code",
                            "default": true
                        },
                        "enableStepFiltering": {
                            "type": "boolean",
                            "description": "Step over properties and operators",
                            "default": true
                        }
                    }
                },
                {
                    "type": "object",
                    "required": ["request", "processId"],
                    "properties": {
                        "request": { "type": "string", "enum": ["attach"] },
                        "processId": {
                            "type": ["integer", "string"],
                            "description": "Id of the process to attach to"
                        }
                    }
                }
            ]
        })
    }

    async fn get_binary(
        &self,
        delegate: &Arc<dyn DapDelegate>,
        config: &DebugTaskDefinition,
        user_installed_path: Option<PathBuf>,
        user_args: Option<Vec<String>>,
        user_env: Option<HashMap<String, String>>,
        _: &mut AsyncApp,
    ) -> Result<DebugAdapterBinary> {
        let binary = match netcoredbg::find_binary(user_installed_path.as_deref()) {
            Some(binary) => binary,
            None => match delegate.which(OsStr::new("netcoredbg")).await {
                Some(binary) => binary,
                None => bail!(
                    "netcoredbg was not found. Download it from {} and set \"dap\": {{ \"netcoredbg\": {{ \"binary\": \"<path>\" }} }} in your settings.",
                    netcoredbg::RELEASES_URL
                ),
            },
        };

        let mut arguments = user_args.unwrap_or_default();
        if !arguments
            .iter()
            .any(|argument| argument.starts_with("--interpreter"))
        {
            arguments.insert(0, "--interpreter=vscode".into());
        }

        let mut configuration = config.config.clone();
        if let Some(configuration) = configuration.as_object_mut() {
            configuration
                .entry("cwd")
                .or_insert_with(|| delegate.worktree_root_path().to_string_lossy().into());
        }

        let mut envs = delegate.shell_env().await;
        envs.extend(user_env.unwrap_or_default());

        Ok(DebugAdapterBinary {
            command: Some(binary.to_string_lossy().into_owned()),
            arguments,
            envs,
            cwd: Some(delegate.worktree_root_path().to_path_buf()),
            connection: None,
            request_args: StartDebuggingRequestArguments {
                request: self.request_kind(&config.config).await?,
                configuration,
            },
        })
    }
}
