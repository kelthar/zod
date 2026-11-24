use anyhow::{Context as _, Result};
use editor::{Editor, MultiBufferOffset};
use fs::Fs;
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{App, Context, DismissEvent, Entity, Task, WeakEntity, Window};
use picker::{Picker, PickerDelegate};
use project::{Project, ProjectPath};
use serde::Deserialize;
use snippet::Snippet;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::{ResultExt, rel_path::RelPath};
use workspace::{Workspace, notifications::NotifyTaskExt};
use worktree::Snapshot;

const FILE_TEMPLATES_FILE_NAME: &str = "filetemplates.json";

#[derive(Deserialize)]
struct FileTemplateContent {
    body: FileTemplateBody,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    extension: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FileTemplateBody {
    Text(String),
    Lines(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
struct FileTemplate {
    name: String,
    description: Option<String>,
    extension: Option<String>,
    body: String,
}

fn parse_file_templates(content: &str) -> Result<Vec<FileTemplate>> {
    if content.trim().is_empty() {
        return Ok(Vec::new());
    }
    let templates: BTreeMap<String, FileTemplateContent> =
        settings::parse_json_with_comments(content)?;
    Ok(templates
        .into_iter()
        .map(|(name, template)| FileTemplate {
            name,
            description: template.description,
            extension: template
                .extension
                .map(|extension| extension.trim_start_matches('.').to_string())
                .filter(|extension| !extension.is_empty()),
            body: match template.body {
                FileTemplateBody::Text(text) => text,
                FileTemplateBody::Lines(lines) => lines.join("\n"),
            },
        })
        .collect())
}

/// Later paths override templates with the same name from earlier paths.
async fn load_file_templates(
    fs: Arc<dyn Fs>,
    template_paths: Vec<PathBuf>,
) -> (Vec<FileTemplate>, Vec<String>) {
    let mut templates: Vec<FileTemplate> = Vec::new();
    let mut errors = Vec::new();
    for template_path in template_paths {
        if !fs.is_file(&template_path).await {
            continue;
        }
        match fs
            .load(&template_path)
            .await
            .and_then(|content| parse_file_templates(&content))
        {
            Ok(loaded_templates) => {
                for template in loaded_templates {
                    templates.retain(|existing| existing.name != template.name);
                    templates.push(template);
                }
            }
            Err(error) => errors.push(format!("{}: {error:#}", template_path.display())),
        }
    }
    templates.sort_by(|left, right| left.name.cmp(&right.name));
    (templates, errors)
}

fn find_csharp_project_file(snapshot: &Snapshot, directory: &RelPath) -> Option<Arc<RelPath>> {
    directory.ancestors().find_map(|ancestor| {
        snapshot
            .child_entries(ancestor)
            .find(|entry| entry.is_file() && entry.path.extension() == Some("csproj"))
            .map(|entry| entry.path.clone())
    })
}

fn root_namespace_from_project_file(project_file_content: &str, project_name: &str) -> String {
    project_file_content
        .split_once("<RootNamespace>")
        .and_then(|(_, rest)| rest.split_once("</RootNamespace>"))
        .map(|(root_namespace, _)| root_namespace.trim())
        // MSBuild properties such as `$(MSBuildProjectName)` can't be evaluated here.
        .filter(|root_namespace| !root_namespace.is_empty() && !root_namespace.contains("$("))
        .unwrap_or(project_name)
        .to_string()
}

fn sanitize_namespace_part(part: &str) -> Option<String> {
    let mut sanitized: String = part
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        return None;
    }
    if sanitized.starts_with(|character: char| character.is_ascii_digit()) {
        sanitized.insert(0, '_');
    }
    Some(sanitized)
}

/// `project_directory` is the directory containing the `.csproj` (or the worktree root when
/// there is none), and every folder between it and `directory` becomes a namespace segment.
fn namespace_for_directory(
    root_namespace: Option<&str>,
    project_directory: &RelPath,
    directory: &RelPath,
    worktree_root_name: &str,
) -> String {
    let folders = directory
        .components()
        .skip(project_directory.components().count());
    let mut segments: Vec<&str> = root_namespace.into_iter().chain(folders).collect();
    if segments.is_empty() {
        segments.push(worktree_root_name);
    }
    segments
        .iter()
        .flat_map(|segment| segment.split('.'))
        .filter_map(sanitize_namespace_part)
        .collect::<Vec<_>>()
        .join(".")
}

fn escape_snippet_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('$', "\\$")
        .replace('}', "\\}")
}

fn expand_template_variables(body: &str, namespace: &str, file_name: &str) -> String {
    body.replace("${namespace}", &escape_snippet_text(namespace))
        .replace("${filename}", &escape_snippet_text(file_name))
}

fn file_name_with_extension(file_name: &str, extension: Option<&str>) -> String {
    match extension {
        Some(extension) if !file_name.ends_with(&format!(".{extension}")) => {
            format!("{file_name}.{extension}")
        }
        _ => file_name.to_string(),
    }
}

pub(crate) fn open_new_file_from_template(
    workspace: &mut Workspace,
    directory: ProjectPath,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    let fs = workspace.app_state().fs.clone();
    let mut template_paths = vec![paths::config_dir().join(FILE_TEMPLATES_FILE_NAME)];
    if let Some(worktree) = project.read(cx).worktree_for_id(directory.worktree_id, cx) {
        template_paths.push(
            worktree
                .read(cx)
                .abs_path()
                .join(".zed")
                .join(FILE_TEMPLATES_FILE_NAME),
        );
    }
    cx.spawn_in(window, async move |workspace, cx| {
        let (templates, load_errors) = load_file_templates(fs, template_paths).await;
        workspace.update_in(cx, |workspace, window, cx| {
            let workspace_handle = cx.entity().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| {
                let delegate = NewFileFromTemplateDelegate::new(
                    workspace_handle,
                    project,
                    directory,
                    templates,
                    load_errors,
                );
                Picker::uniform_list(delegate, window, cx)
            });
        })
    })
    .detach_and_log_err(cx);
}

pub(crate) struct NewFileFromTemplateDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    directory: ProjectPath,
    templates: Vec<FileTemplate>,
    load_errors: Vec<String>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    chosen_template: Option<FileTemplate>,
    query: String,
}

impl NewFileFromTemplateDelegate {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<Project>,
        directory: ProjectPath,
        templates: Vec<FileTemplate>,
        load_errors: Vec<String>,
    ) -> Self {
        Self {
            workspace,
            project,
            directory,
            templates,
            load_errors,
            matches: Vec::new(),
            selected_index: 0,
            chosen_template: None,
            query: String::new(),
        }
    }

    fn new_file_path(&self, cx: &App) -> Option<ProjectPath> {
        let template = self.chosen_template.as_ref()?;
        let file_name = self.query.trim().trim_start_matches('/');
        if file_name.is_empty() || file_name.ends_with('/') {
            return None;
        }
        let file_name = file_name_with_extension(file_name, template.extension.as_deref());
        let path_style = self.project.read(cx).path_style(cx);
        let relative_path = RelPath::new(Path::new(&file_name), path_style).ok()?;
        Some(ProjectPath {
            worktree_id: self.directory.worktree_id,
            path: self.directory.path.join(&relative_path).into(),
        })
    }

    fn path_exists(&self, project_path: &ProjectPath, cx: &App) -> bool {
        self.project
            .read(cx)
            .entry_for_path(project_path, cx)
            .is_some()
    }

    fn directory_display(&self, cx: &App) -> String {
        let project = self.project.read(cx);
        let root_name = project
            .worktree_for_id(self.directory.worktree_id, cx)
            .map(|worktree| worktree.read(cx).root_name_str().to_string())
            .unwrap_or_default();
        let directory = self.directory.path.display(project.path_style(cx));
        if directory.is_empty() {
            root_name
        } else {
            format!("{root_name}/{directory}")
        }
    }

    fn create_file(
        &self,
        template: FileTemplate,
        new_file_path: ProjectPath,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        let fs = project.read(cx).fs().clone();
        window.spawn(cx, async move |cx| {
            let (snapshot, worktree_abs_path) = project.read_with(cx, |project, cx| {
                project
                    .worktree_for_id(new_file_path.worktree_id, cx)
                    .map(|worktree| {
                        let worktree = worktree.read(cx);
                        (worktree.snapshot(), worktree.abs_path())
                    })
                    .context("worktree not found")
            })?;
            let directory = new_file_path
                .path
                .parent()
                .context("new file path has no parent")?;
            let (root_namespace, project_directory) =
                match find_csharp_project_file(&snapshot, directory) {
                    Some(project_file) => {
                        let content = fs
                            .load(&worktree_abs_path.join(project_file.as_std_path()))
                            .await?;
                        let project_name = project_file.file_stem().unwrap_or_default();
                        (
                            Some(root_namespace_from_project_file(&content, project_name)),
                            project_file.parent().unwrap_or(RelPath::empty()).into_arc(),
                        )
                    }
                    None => (None, RelPath::empty_arc()),
                };
            let namespace = namespace_for_directory(
                root_namespace.as_deref(),
                &project_directory,
                directory,
                snapshot.root_name_str(),
            );
            let file_stem = new_file_path.path.file_stem().unwrap_or_default();
            let snippet = Snippet::parse(&expand_template_variables(
                &template.body,
                &namespace,
                file_stem,
            ))
            .with_context(|| format!("invalid body in file template {:?}", template.name))?;

            project
                .update(cx, |project, cx| {
                    project.create_entry(new_file_path.clone(), false, cx)
                })
                .await?;
            let open_task = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_path(new_file_path, None, true, window, cx)
            })?;
            let item = open_task.await?;
            let editor = cx
                .update(|_, cx| item.act_as::<Editor>(cx))?
                .context("new file did not open in an editor")?;
            editor.update_in(cx, |editor, window, cx| {
                editor.insert_snippet(
                    &[MultiBufferOffset(0)..MultiBufferOffset(0)],
                    snippet,
                    window,
                    cx,
                )
            })??;
            Ok(())
        })
    }
}

impl PickerDelegate for NewFileFromTemplateDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "new-file-from-template"
    }

    fn match_count(&self) -> usize {
        if self.chosen_template.is_some() {
            usize::from(!self.query.trim().is_empty())
        } else {
            self.matches.len()
        }
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn placeholder_text(&self, _window: &mut Window, cx: &mut App) -> Arc<str> {
        match &self.chosen_template {
            Some(template) => format!(
                "Name of the new {} in {}",
                template.name,
                self.directory_display(cx)
            )
            .into(),
            None => format!("New file from template in {}", self.directory_display(cx)).into(),
        }
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        if self.chosen_template.is_some() {
            return Some("Type a file name".into());
        }
        if !self.load_errors.is_empty() {
            return Some(format!("Failed to load templates: {}", self.load_errors.join("; ")).into());
        }
        if self.templates.is_empty() {
            return Some(
                format!(
                    "No templates found. Add them to {} or .zed/{FILE_TEMPLATES_FILE_NAME}",
                    paths::config_dir().join(FILE_TEMPLATES_FILE_NAME).display()
                )
                .into(),
            );
        }
        Some("No matching templates".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query.clone();
        self.selected_index = 0;
        if self.chosen_template.is_some() {
            return Task::ready(());
        }
        let candidates = self
            .templates
            .iter()
            .enumerate()
            .map(|(index, template)| StringMatchCandidate::new(index, &template.name))
            .collect::<Vec<_>>();
        if query.is_empty() {
            self.matches = candidates
                .into_iter()
                .map(|candidate| StringMatch {
                    candidate_id: candidate.id,
                    score: 0.0,
                    positions: Vec::new(),
                    string: candidate.string,
                })
                .collect();
            return Task::ready(());
        }
        let executor = cx.background_executor().clone();
        cx.spawn_in(window, async move |picker, cx| {
            let matches = fuzzy::match_strings(
                &candidates,
                &query,
                false,
                true,
                100,
                &Default::default(),
                executor,
            )
            .await;
            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = matches;
                    cx.notify();
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(template) = self.chosen_template.clone() else {
            let Some(template) = self
                .matches
                .get(self.selected_index)
                .and_then(|string_match| self.templates.get(string_match.candidate_id))
            else {
                return;
            };
            self.chosen_template = Some(template.clone());
            cx.defer_in(window, |picker, window, cx| {
                picker.set_query("", window, cx);
                picker.refresh_placeholder(window, cx);
                picker.refresh(window, cx);
            });
            return;
        };
        let Some(new_file_path) = self.new_file_path(cx) else {
            return;
        };
        if self.path_exists(&new_file_path, cx) {
            return;
        }
        self.create_file(template, new_file_path, window, cx)
            .detach_and_notify_err(self.workspace.clone(), window, cx);
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        if self.chosen_template.is_some() {
            let new_file_path = self.new_file_path(cx);
            let (label, color) = match &new_file_path {
                Some(path) if self.path_exists(path, cx) => (
                    format!(
                        "{} already exists",
                        path.path.display(self.project.read(cx).path_style(cx))
                    ),
                    Color::Error,
                ),
                Some(path) => (
                    format!(
                        "Create {}",
                        path.path.display(self.project.read(cx).path_style(cx))
                    ),
                    Color::Default,
                ),
                None => ("Invalid file name".to_string(), Color::Error),
            };
            return Some(
                ListItem::new(ix)
                    .inset(true)
                    .spacing(ListItemSpacing::Sparse)
                    .toggle_state(selected)
                    .child(Label::new(label).color(color)),
            );
        }
        let string_match = self.matches.get(ix)?;
        let template = self.templates.get(string_match.candidate_id)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .gap_2()
                        .child(HighlightedLabel::new(
                            template.name.clone(),
                            string_match.positions.clone(),
                        ))
                        .when_some(template.description.clone(), |this, description| {
                            this.child(Label::new(description).color(Color::Muted))
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use project::FakeFs;
    use gpui::TestAppContext;
    use serde_json::json;
    use settings::SettingsStore;
    use util::{path, rel_path::rel_path};

    #[test]
    fn test_parse_file_templates() {
        let templates = parse_file_templates(
            r##"{
                // Comments and VS Code snippet fields like "prefix" are allowed.
                "C# Class": {
                    "prefix": "cls",
                    "extension": ".cs",
                    "description": "New class",
                    "body": ["namespace ${namespace};", "", "public class ${filename}", "{", "    $0", "}"],
                },
                "Readme": { "body": "# ${filename}" },
            }"##,
        )
        .unwrap();
        assert_eq!(
            templates,
            vec![
                FileTemplate {
                    name: "C# Class".into(),
                    description: Some("New class".into()),
                    extension: Some("cs".into()),
                    body: "namespace ${namespace};\n\npublic class ${filename}\n{\n    $0\n}".into(),
                },
                FileTemplate {
                    name: "Readme".into(),
                    description: None,
                    extension: None,
                    body: "# ${filename}".into(),
                },
            ]
        );
    }

    #[test]
    fn test_template_expansion_and_file_names() {
        let body = "namespace ${namespace};\npublic ${1|class,record|} ${filename} { $0 }";
        let snippet =
            Snippet::parse(&expand_template_variables(body, "My.App", "Odd$Name}")).unwrap();
        assert_eq!(
            snippet.text,
            "namespace My.App;\npublic class Odd$Name} {  }"
        );
        assert_eq!(file_name_with_extension("OrderService", Some("cs")), "OrderService.cs");
        assert_eq!(file_name_with_extension("OrderService.cs", Some("cs")), "OrderService.cs");
        assert_eq!(file_name_with_extension("Foo.Bar", Some("cs")), "Foo.Bar.cs");
        assert_eq!(file_name_with_extension("notes", None), "notes");
    }

    #[test]
    fn test_namespace_for_directory() {
        assert_eq!(
            namespace_for_directory(
                Some("Roxtec.Orion.Mercurius"),
                rel_path("Roxtec.Orion.Mercurius"),
                rel_path("Roxtec.Orion.Mercurius/Services/Http-Clients"),
                "orion",
            ),
            "Roxtec.Orion.Mercurius.Services.Http_Clients"
        );
        assert_eq!(
            namespace_for_directory(None, RelPath::empty(), rel_path("Folder1/Folder2"), "Root"),
            "Folder1.Folder2"
        );
        assert_eq!(
            namespace_for_directory(None, RelPath::empty(), RelPath::empty(), "my-app"),
            "my_app"
        );
        assert_eq!(
            namespace_for_directory(Some("App"), RelPath::empty(), rel_path("2024"), "root"),
            "App._2024"
        );
        assert_eq!(
            root_namespace_from_project_file(
                "<Project><PropertyGroup><RootNamespace> Custom.Root </RootNamespace></PropertyGroup></Project>",
                "Project"
            ),
            "Custom.Root"
        );
        assert_eq!(
            root_namespace_from_project_file(
                "<RootNamespace>$(MSBuildProjectName)</RootNamespace>",
                "Project"
            ),
            "Project"
        );
    }

    #[gpui::test]
    async fn test_load_templates_and_find_project_file(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "global.json": r#"{ "Class": { "body": "global" }, "Enum": { "body": "enum" } }"#,
                "project": {
                    ".zed": { "filetemplates.json": r#"{ "Class": { "body": "project" } }"# },
                    "App": {
                        "App.csproj": "<Project />",
                        "Services": { "Http": {} },
                    },
                },
            }),
        )
        .await;

        let (templates, errors) = load_file_templates(
            fs.clone(),
            vec![
                PathBuf::from(path!("/root/global.json")),
                PathBuf::from(path!("/root/missing.json")),
                PathBuf::from(path!("/root/project/.zed/filetemplates.json")),
            ],
        )
        .await;
        assert!(errors.is_empty());
        let bodies: Vec<_> = templates
            .iter()
            .map(|template| (template.name.as_str(), template.body.as_str()))
            .collect();
        assert_eq!(bodies, vec![("Class", "project"), ("Enum", "enum")]);

        let project = Project::test(fs.clone(), [path!("/root/project").as_ref()], cx).await;
        cx.run_until_parked();
        let snapshot = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).snapshot()
        });
        assert_eq!(
            find_csharp_project_file(&snapshot, rel_path("App/Services/Http")).as_deref(),
            Some(rel_path("App/App.csproj"))
        );
        assert_eq!(find_csharp_project_file(&snapshot, rel_path(".zed")), None);
    }
}
