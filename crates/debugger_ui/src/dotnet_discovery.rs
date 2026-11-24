//! Discovers runnable and test .NET (Core) projects in the open worktrees, so that they can be
//! offered in the new process modal without any `debug.json` / `tasks.json` configuration.
//!
//! Project properties are evaluated with `dotnet msbuild -getProperty`, so values coming from
//! `Directory.Build.props` and other imports are taken into account.

use std::{
    borrow::Cow,
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use collections::HashMap;
use futures::StreamExt as _;
use gpui::{App, AppContext as _, Entity, Task};
use project::{Project, TaskSourceKind};
use task::{BuildTaskDefinition, DebugScenario, ResolvedTask, TaskContext, TaskTemplate};

pub(crate) const NETCOREDBG_ADAPTER: &str = "netcoredbg";
const DOTNET_ID_BASE: &str = "dotnet";
const MAX_CONCURRENT_EVALUATIONS: usize = 8;
const EVALUATED_PROPERTIES: &[&str] = &[
    "TargetFramework",
    "TargetFrameworks",
    "TargetFrameworkIdentifier",
    "OutputType",
    "IsTestProject",
    "TargetPath",
];

#[derive(Debug, Default)]
pub(crate) struct DotnetDiscovery {
    pub solutions: Vec<PathBuf>,
    pub projects: Vec<DotnetProject>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DotnetProject {
    pub name: String,
    pub csproj: PathBuf,
    pub target_path: PathBuf,
    pub is_test: bool,
    pub is_runnable: bool,
    pub launch_profiles: Vec<LaunchProfile>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LaunchProfile {
    pub name: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Default)]
pub(crate) struct DotnetEntries {
    pub debug_scenarios: Vec<(TaskSourceKind, DebugScenario)>,
    pub run_tasks: Vec<(TaskSourceKind, ResolvedTask)>,
    pub test_tasks: Vec<(TaskSourceKind, ResolvedTask)>,
}

pub(crate) fn discover(project: &Entity<Project>, cx: &App) -> Task<DotnetDiscovery> {
    let project = project.read(cx);
    if !project.is_local() {
        return Task::ready(DotnetDiscovery::default());
    }

    let mut solutions = Vec::new();
    let mut csprojs = Vec::new();
    for worktree in project.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path();
        for entry in worktree.files(false, 0) {
            let path = &entry.path;
            match path.extension() {
                Some("csproj") => {
                    let in_build_output = path
                        .components()
                        .any(|component| matches!(component, "bin" | "obj" | "node_modules"));
                    if !in_build_output {
                        csprojs.push(root.join(path.as_std_path()));
                    }
                }
                Some("sln" | "slnx") if path.components().count() == 1 => {
                    solutions.push(root.join(path.as_std_path()));
                }
                _ => {}
            }
        }
    }

    if csprojs.is_empty() {
        return Task::ready(DotnetDiscovery::default());
    }

    cx.background_spawn(async move {
        let Some(dotnet) = find_dotnet() else {
            log::info!("dotnet executable not found, skipping .NET project discovery");
            return DotnetDiscovery::default();
        };
        let mut projects = futures::stream::iter(csprojs)
            .map(|csproj| {
                let dotnet = dotnet.clone();
                async move {
                    evaluate_project(&dotnet, &csproj)
                        .await
                        .with_context(|| format!("evaluating {}", csproj.display()))
                }
            })
            .buffer_unordered(MAX_CONCURRENT_EVALUATIONS)
            .filter_map(|result| async move {
                result
                    .inspect_err(|error| log::warn!("{error:#}"))
                    .ok()
                    .flatten()
            })
            .collect::<Vec<_>>()
            .await;
        projects.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.csproj.cmp(&b.csproj)));
        solutions.sort();
        DotnetDiscovery {
            solutions,
            projects,
        }
    })
}

/// Builds debug scenarios, run tasks and test tasks out of the discovered projects.
pub(crate) fn entries(discovery: &DotnetDiscovery, task_context: &TaskContext) -> DotnetEntries {
    let mut entries = DotnetEntries::default();
    let resolve = |template: TaskTemplate| template.resolve_task(DOTNET_ID_BASE, task_context);

    for solution in &discovery.solutions {
        if !discovery.projects.iter().any(|project| project.is_test) {
            break;
        }
        let file_name = file_name(solution);
        let template = test_template(
            format!("Test all ({file_name})"),
            solution,
            solution.parent(),
        );
        if let Some(task) = resolve(template) {
            entries.test_tasks.push((source_kind(solution), task));
        }
    }

    for project in &discovery.projects {
        let kind = source_kind(&project.csproj);
        let project_dir = project.csproj.parent();

        if project.is_test {
            let template = test_template(
                format!("Test {}", project.name),
                &project.csproj,
                project_dir,
            );
            if let Some(task) = resolve(template) {
                entries.test_tasks.push((kind.clone(), task));
            }
        }

        if !project.is_runnable {
            continue;
        }

        let profiles = if project.launch_profiles.is_empty() {
            vec![None]
        } else {
            project.launch_profiles.iter().map(Some).collect()
        };
        for profile in profiles {
            let label = match profile {
                Some(profile) => format!("{}: {}", project.name, profile.name),
                None => project.name.clone(),
            };

            let mut run_args = vec!["run".into(), "--project".into(), path_arg(&project.csproj)];
            if let Some(profile) = profile {
                run_args.extend(["--launch-profile".into(), profile.name.clone()]);
            }
            if let Some(task) = resolve(dotnet_template(
                format!("Run {label}"),
                run_args,
                project_dir,
            )) {
                entries.run_tasks.push((kind.clone(), task));
            }

            entries
                .debug_scenarios
                .push((kind.clone(), debug_scenario(project, profile, label)));
        }
    }

    entries
}

fn debug_scenario(
    project: &DotnetProject,
    profile: Option<&LaunchProfile>,
    label: String,
) -> DebugScenario {
    let cwd = profile
        .and_then(|profile| profile.working_directory.clone())
        .or_else(|| project.csproj.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let args = profile
        .map(|profile| profile.args.clone())
        .unwrap_or_default();
    let env = profile
        .map(|profile| profile.env.clone())
        .unwrap_or_default();

    DebugScenario {
        adapter: NETCOREDBG_ADAPTER.into(),
        label: label.into(),
        build: Some(BuildTaskDefinition::Template {
            task_template: dotnet_template(
                format!("Build {}", project.name),
                vec!["build".into(), path_arg(&project.csproj)],
                project.csproj.parent(),
            ),
            locator_name: None,
        }),
        config: serde_json::json!({
            "request": "launch",
            "program": project.target_path,
            "args": args,
            "cwd": cwd,
            "env": env,
            "stopAtEntry": false,
        }),
        tcp_connection: None,
    }
}

fn dotnet_template(label: String, args: Vec<String>, cwd: Option<&Path>) -> TaskTemplate {
    TaskTemplate {
        label,
        command: "dotnet".into(),
        args,
        cwd: cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
        ..TaskTemplate::default()
    }
}

/// A `dotnet test` task that writes `.trx` results for the test results panel to pick up.
fn test_template(label: String, target: &Path, cwd: Option<&Path>) -> TaskTemplate {
    let results_directory = test_results_panel::results_directory(&label);
    let mut template = dotnet_template(
        label,
        vec![
            "test".into(),
            path_arg(target),
            "--logger".into(),
            "trx".into(),
            "--results-directory".into(),
            path_arg(&results_directory),
        ],
        cwd,
    );
    template.env.insert(
        test_results_panel::RESULTS_DIRECTORY_ENV.into(),
        path_arg(&results_directory),
    );
    template
}

fn source_kind(path: &Path) -> TaskSourceKind {
    TaskSourceKind::AbsPath {
        id_base: Cow::Borrowed(DOTNET_ID_BASE),
        abs_path: path.to_path_buf(),
    }
}

fn path_arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn find_dotnet() -> Option<PathBuf> {
    let executable = if cfg!(windows) {
        "dotnet.exe"
    } else {
        "dotnet"
    };
    let from_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(executable));
    let well_known = [
        std::env::var_os("DOTNET_ROOT").map(PathBuf::from),
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".dotnet")),
        Some(PathBuf::from("/usr/local/share/dotnet")),
        Some(PathBuf::from("/opt/homebrew/bin")),
        Some(PathBuf::from("/usr/share/dotnet")),
    ]
    .into_iter()
    .flatten()
    .map(|dir| dir.join(executable));
    from_path.chain(well_known).find(|path| path.is_file())
}

async fn evaluate_project(dotnet: &Path, csproj: &Path) -> Result<Option<DotnetProject>> {
    let mut properties = msbuild_properties(dotnet, csproj, None).await?;
    if property(&properties, "TargetFramework").is_empty() {
        // Multi-targeted project: evaluate again for the first .NET Core target framework.
        let Some(target_framework) = property(&properties, "TargetFrameworks")
            .split(';')
            .map(str::trim)
            .find(|framework| is_net_core_target_framework(framework))
            .map(str::to_string)
        else {
            return Ok(None);
        };
        properties = msbuild_properties(dotnet, csproj, Some(&target_framework)).await?;
    }

    if property(&properties, "TargetFrameworkIdentifier") != ".NETCoreApp" {
        return Ok(None);
    }
    let is_test = property(&properties, "IsTestProject").eq_ignore_ascii_case("true");
    let output_type = property(&properties, "OutputType");
    let is_runnable = !is_test
        && (output_type.eq_ignore_ascii_case("Exe") || output_type.eq_ignore_ascii_case("WinExe"));
    if !is_test && !is_runnable {
        return Ok(None);
    }

    let target_path = property(&properties, "TargetPath");
    anyhow::ensure!(!target_path.is_empty(), "TargetPath is empty");
    let project_dir = csproj.parent().context("csproj has no parent directory")?;

    let launch_profiles = if is_runnable {
        let launch_settings = project_dir.join("Properties").join("launchSettings.json");
        match std::fs::read_to_string(&launch_settings) {
            Ok(content) => parse_launch_settings(&content, project_dir)
                .with_context(|| format!("parsing {}", launch_settings.display()))?,
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    Ok(Some(DotnetProject {
        name: csproj
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
        csproj: csproj.to_path_buf(),
        target_path: PathBuf::from(target_path),
        is_test,
        is_runnable,
        launch_profiles,
    }))
}

async fn msbuild_properties(
    dotnet: &Path,
    csproj: &Path,
    target_framework: Option<&str>,
) -> Result<HashMap<String, String>> {
    let mut command = util::command::new_command(dotnet);
    command.arg("msbuild").arg(csproj);
    for property in EVALUATED_PROPERTIES {
        command.arg(format!("-getProperty:{property}"));
    }
    if let Some(target_framework) = target_framework {
        command.arg(format!("-property:TargetFramework={target_framework}"));
    }
    if let Some(dir) = csproj.parent() {
        command.current_dir(dir);
    }
    command
        .env("DOTNET_CLI_TELEMETRY_OPTOUT", "1")
        .env("DOTNET_NOLOGO", "1");

    let output = command.output().await.context("running dotnet msbuild")?;
    anyhow::ensure!(
        output.status.success(),
        "dotnet msbuild failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_msbuild_properties(&output.stdout)
}

fn property<'a>(properties: &'a HashMap<String, String>, name: &str) -> &'a str {
    properties.get(name).map(String::as_str).unwrap_or_default()
}

fn parse_msbuild_properties(output: &[u8]) -> Result<HashMap<String, String>> {
    #[derive(serde::Deserialize)]
    struct Output {
        #[serde(rename = "Properties")]
        properties: HashMap<String, String>,
    }
    let output: Output =
        serde_json::from_slice(output).context("parsing dotnet msbuild -getProperty output")?;
    Ok(output.properties)
}

/// `netcoreappX.Y` and `netX.Y` (.NET 5+) are .NET Core; `net48`-style monikers are .NET Framework.
fn is_net_core_target_framework(framework: &str) -> bool {
    let framework = framework.to_ascii_lowercase();
    if framework.starts_with("netcoreapp") {
        return true;
    }
    framework.strip_prefix("net").is_some_and(|version| {
        let version = version.split('-').next().unwrap_or_default();
        version.starts_with(|c: char| c.is_ascii_digit()) && version.contains('.')
    })
}

fn parse_launch_settings(content: &str, project_dir: &Path) -> Result<Vec<LaunchProfile>> {
    let settings: serde_json_lenient::Value = serde_json_lenient::from_str(content)?;
    let Some(profiles) = settings
        .get("profiles")
        .and_then(|profiles| profiles.as_object())
    else {
        return Ok(Vec::new());
    };

    let string = |value: &serde_json_lenient::Value, key: &str| {
        value
            .get(key)
            .and_then(|value| value.as_str())
            .map(str::to_string)
    };

    Ok(profiles
        .iter()
        .filter(|(_, profile)| string(profile, "commandName").as_deref() == Some("Project"))
        .map(|(name, profile)| {
            let mut env = profile
                .get("environmentVariables")
                .and_then(|env| env.as_object())
                .map(|env| {
                    env.iter()
                        .filter_map(|(key, value)| {
                            let value = match value {
                                serde_json_lenient::Value::String(value) => value.clone(),
                                serde_json_lenient::Value::Null => return None,
                                other => other.to_string(),
                            };
                            Some((key.clone(), value))
                        })
                        .collect::<BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            if let Some(urls) = string(profile, "applicationUrl") {
                env.entry("ASPNETCORE_URLS".into()).or_insert(urls);
            }
            let args = string(profile, "commandLineArgs")
                .map(|args| {
                    shlex::split(&args)
                        .unwrap_or_else(|| args.split_whitespace().map(str::to_string).collect())
                })
                .unwrap_or_default();
            let working_directory = string(profile, "workingDirectory")
                .filter(|dir| !dir.trim().is_empty())
                .map(|dir| {
                    let project_dir_string = format!("{}/", project_dir.display());
                    let dir = dir.replace("$(ProjectDir)", &project_dir_string);
                    project_dir.join(dir)
                });
            LaunchProfile {
                name: name.clone(),
                args,
                env,
                working_directory,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_net_core_target_framework() {
        for framework in [
            "net8.0",
            "net10.0",
            "net9.0-windows",
            "netcoreapp3.1",
            "NET6.0",
        ] {
            assert!(is_net_core_target_framework(framework), "{framework}");
        }
        for framework in ["net48", "net472", "netstandard2.0", "netstandard2.1", ""] {
            assert!(!is_net_core_target_framework(framework), "{framework}");
        }
    }

    #[test]
    fn test_parse_msbuild_properties() {
        let properties = parse_msbuild_properties(
            br#"{
              "Properties": {
                "TargetFramework": "net10.0",
                "TargetFrameworkIdentifier": ".NETCoreApp",
                "OutputType": "Exe",
                "IsTestProject": ""
              }
            }"#,
        )
        .unwrap();
        assert_eq!(property(&properties, "TargetFramework"), "net10.0");
        assert_eq!(property(&properties, "IsTestProject"), "");
        assert_eq!(property(&properties, "Missing"), "");
    }

    #[test]
    fn test_parse_launch_settings() {
        let project_dir = Path::new("/repo/Api");
        let profiles = parse_launch_settings(
            r#"{
              // comments are allowed
              "profiles": {
                "Web": {
                  "commandName": "Project",
                  "environmentVariables": { "ASPNETCORE_ENVIRONMENT": "Development" },
                  "applicationUrl": "https://localhost:7164;http://localhost:5060",
                  "commandLineArgs": "--seed \"two words\"",
                  "workingDirectory": "$(ProjectDir)data"
                },
                "IIS Express": { "commandName": "IISExpress" },
                "Plain": { "commandName": "Project" },
              }
            }"#,
            project_dir,
        )
        .unwrap();

        assert_eq!(
            profiles,
            vec![
                LaunchProfile {
                    name: "Web".into(),
                    args: vec!["--seed".into(), "two words".into()],
                    env: BTreeMap::from([
                        ("ASPNETCORE_ENVIRONMENT".into(), "Development".into()),
                        (
                            "ASPNETCORE_URLS".into(),
                            "https://localhost:7164;http://localhost:5060".into()
                        ),
                    ]),
                    working_directory: Some(PathBuf::from("/repo/Api/data")),
                },
                LaunchProfile {
                    name: "Plain".into(),
                    ..LaunchProfile::default()
                },
            ]
        );
    }

    #[test]
    fn test_entries() {
        let api = DotnetProject {
            name: "Api".into(),
            csproj: PathBuf::from("/repo/Api/Api.csproj"),
            target_path: PathBuf::from("/repo/Api/bin/Debug/net10.0/Api.dll"),
            is_test: false,
            is_runnable: true,
            launch_profiles: vec![LaunchProfile {
                name: "Web".into(),
                ..LaunchProfile::default()
            }],
        };
        let worker = DotnetProject {
            name: "Worker".into(),
            csproj: PathBuf::from("/repo/Worker/Worker.csproj"),
            target_path: PathBuf::from("/repo/Worker/bin/Debug/net10.0/Worker.dll"),
            launch_profiles: Vec::new(),
            ..api
        };
        let tests = DotnetProject {
            name: "Api.Tests".into(),
            csproj: PathBuf::from("/repo/Api.Tests/Api.Tests.csproj"),
            target_path: PathBuf::from("/repo/Api.Tests/bin/Debug/net10.0/Api.Tests.dll"),
            is_test: true,
            is_runnable: false,
            launch_profiles: Vec::new(),
        };
        let discovery = DotnetDiscovery {
            solutions: vec![PathBuf::from("/repo/App.sln")],
            projects: vec![api, tests, worker],
        };

        let entries = entries(&discovery, &TaskContext::default());

        let labels = |tasks: &[(TaskSourceKind, ResolvedTask)]| {
            tasks
                .iter()
                .map(|(_, task)| task.resolved_label.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(labels(&entries.run_tasks), ["Run Api: Web", "Run Worker"]);
        assert_eq!(
            labels(&entries.test_tasks),
            ["Test all (App.sln)", "Test Api.Tests"]
        );
        assert_eq!(
            entries.run_tasks[0].1.resolved.args,
            [
                "run",
                "--project",
                "/repo/Api/Api.csproj",
                "--launch-profile",
                "Web"
            ]
        );
        let test_task = &entries.test_tasks[1].1.resolved;
        let results_directory = test_task.env[test_results_panel::RESULTS_DIRECTORY_ENV].clone();
        assert_eq!(
            test_task.args,
            [
                "test",
                "/repo/Api.Tests/Api.Tests.csproj",
                "--logger",
                "trx",
                "--results-directory",
                results_directory.as_str(),
            ]
        );

        let scenarios = entries
            .debug_scenarios
            .iter()
            .map(|(_, scenario)| {
                (
                    scenario.label.to_string(),
                    scenario.config["program"].clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            scenarios,
            [
                (
                    "Api: Web".to_string(),
                    serde_json::json!("/repo/Api/bin/Debug/net10.0/Api.dll")
                ),
                (
                    "Worker".to_string(),
                    serde_json::json!("/repo/Worker/bin/Debug/net10.0/Worker.dll")
                ),
            ]
        );
        assert!(
            entries
                .debug_scenarios
                .iter()
                .all(|(_, scenario)| scenario.adapter == NETCOREDBG_ADAPTER)
        );
    }
}
