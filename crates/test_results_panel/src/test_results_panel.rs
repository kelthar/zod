//! Shows the failing tests of the last test run in a dock panel.
//!
//! A task opts in by setting the [`RESULTS_DIRECTORY_ENV`] environment variable to a directory
//! where it writes `.trx` files, e.g. `dotnet test --logger trx --results-directory <dir>`.

mod trx;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result};
use collections::HashSet;
use editor::{Bias, Editor, SelectionEffects, scroll::Autoscroll};
use fs::{Fs, RemoveOptions};
use futures::StreamExt as _;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Pixels, Render, SharedString, Subscription, Task, TaskExt, WeakEntity, Window, actions, px,
};
use language::Point;
use project::Project;
use task::{SpawnInTerminal, TaskId};
use ui::{CommonAnimationExt, Disclosure, ListItem, ListItemSpacing, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
    tasks::ScheduledTaskResult,
};

use crate::trx::{SourceLocation, TestFailure, TestRunSummary};

/// Environment variable that marks a task as a test run whose `.trx` results should be shown.
pub const RESULTS_DIRECTORY_ENV: &str = "ZOD_TEST_RESULTS_DIR";

const PANEL_KEY: &str = "TestResultsPanel";
/// Allows for coarse file system timestamps when telling results of this run from stale ones.
const MTIME_TOLERANCE: Duration = Duration::from_secs(2);

actions!(
    test_results_panel,
    [
        /// Toggles focus on the test results panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<TestResultsPanel>(window, cx);
        });
    })
    .detach();
}

/// Directory where the test run with the given name writes its `.trx` files.
pub fn results_directory(run_name: &str) -> PathBuf {
    let sanitized: String = run_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    paths::temp_dir().join("test-results").join(sanitized)
}

enum RunState {
    Idle,
    Running,
    Finished {
        result: ScheduledTaskResult,
        summary: Option<TestRunSummary>,
        error: Option<SharedString>,
    },
}

pub struct TestResultsPanel {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    fs: Arc<dyn Fs>,
    position: DockPosition,
    run_label: Option<SharedString>,
    active_task: Option<TaskId>,
    started_at: SystemTime,
    state: RunState,
    expanded: HashSet<usize>,
    load_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl TestResultsPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            let workspace_entity = cx.entity();
            let project = workspace.project().clone();
            let fs = workspace.app_state().fs.clone();
            cx.new(|cx| {
                let subscription =
                    cx.subscribe_in(&workspace_entity, window, Self::handle_workspace_event);
                Self {
                    focus_handle: cx.focus_handle(),
                    workspace: workspace_entity.downgrade(),
                    project,
                    fs,
                    position: DockPosition::Right,
                    run_label: None,
                    active_task: None,
                    started_at: SystemTime::now(),
                    state: RunState::Idle,
                    expanded: HashSet::default(),
                    load_task: None,
                    _subscriptions: vec![subscription],
                }
            })
        })
    }

    fn handle_workspace_event(
        &mut self,
        _: &Entity<Workspace>,
        event: &workspace::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            workspace::Event::TaskStarted(task) => {
                if results_directory_of(task).is_some() {
                    self.task_started(task, window, cx);
                }
            }
            workspace::Event::TaskFinished { task, result } => {
                if let Some(directory) = results_directory_of(task)
                    && self.active_task.as_ref() == Some(&task.id)
                {
                    self.task_finished(directory, *result, cx);
                }
            }
            _ => {}
        }
    }

    fn task_started(&mut self, task: &SpawnInTerminal, window: &mut Window, cx: &mut Context<Self>) {
        self.active_task = Some(task.id.clone());
        self.run_label = Some(task.label.clone().into());
        self.started_at = SystemTime::now();
        self.state = RunState::Running;
        self.expanded.clear();
        self.load_task = None;
        cx.notify();

        // Opening the panel updates this entity, so it can't happen during this update.
        let workspace = self.workspace.clone();
        window.defer(cx, move |window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_panel::<TestResultsPanel>(window, cx)
                })
                .log_err();
        });
    }

    fn task_finished(
        &mut self,
        directory: PathBuf,
        result: ScheduledTaskResult,
        cx: &mut Context<Self>,
    ) {
        let fs = self.fs.clone();
        let started_at = self.started_at;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let summary = read_results(fs.as_ref(), &directory, started_at).await;
                    fs.remove_dir(
                        &directory,
                        RemoveOptions {
                            recursive: true,
                            ignore_if_not_exists: true,
                        },
                    )
                    .await
                    .log_err();
                    summary
                })
                .await;
            this.update(cx, |this, cx| {
                this.state = match loaded {
                    Ok(summary) => RunState::Finished {
                        result,
                        summary,
                        error: None,
                    },
                    Err(error) => RunState::Finished {
                        result,
                        summary: None,
                        error: Some(format!("{error:#}").into()),
                    },
                };
                cx.notify();
            })
            .log_err();
        }));
    }

    fn open_failure(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let RunState::Finished {
            summary: Some(summary),
            ..
        } = &self.state
        else {
            return;
        };
        let Some(failure) = summary.failures.get(index).cloned() else {
            return;
        };
        let workspace = self.workspace.clone();
        let project = self.project.clone();
        cx.spawn_in(window, async move |_, cx| {
            match failure.location {
                Some(location) => open_location(workspace, location, cx).await,
                None => open_test_symbol(workspace, project, &failure, cx).await,
            }
        })
        .detach_and_log_err(cx);
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let status: AnyElement = match &self.state {
            RunState::Idle => Label::new("Run tests from the Tests tab (F5) to see results here.")
                .color(Color::Muted)
                .into_any_element(),
            RunState::Running => h_flex()
                .gap_1()
                .child(
                    Icon::new(IconName::ArrowCircle)
                        .size(IconSize::Small)
                        .color(Color::Muted)
                        .with_rotate_animation(2),
                )
                .child(Label::new("Running tests…").color(Color::Muted))
                .into_any_element(),
            RunState::Finished {
                result,
                summary,
                error,
            } => match (summary, error) {
                (_, Some(error)) => Label::new(format!("Could not read test results: {error}"))
                    .color(Color::Error)
                    .into_any_element(),
                (Some(summary), None) if summary.total() > 0 => {
                    let failed = summary.failures.len();
                    let (icon, color) = if failed == 0 {
                        (IconName::Check, Color::Success)
                    } else {
                        (IconName::XCircle, Color::Error)
                    };
                    let mut parts = Vec::new();
                    if failed > 0 {
                        parts.push(format!("{failed} failed"));
                    }
                    parts.push(format!("{} passed", summary.passed));
                    if summary.skipped > 0 {
                        parts.push(format!("{} skipped", summary.skipped));
                    }
                    h_flex()
                        .gap_1()
                        .child(Icon::new(icon).size(IconSize::Small).color(color))
                        .child(Label::new(parts.join(" · ")).color(color))
                        .into_any_element()
                }
                _ => {
                    let reason = match result {
                        ScheduledTaskResult::Cancelled => "Test run was cancelled.",
                        ScheduledTaskResult::SpawnFailed => "Test run could not be started.",
                        ScheduledTaskResult::Success | ScheduledTaskResult::Failure => {
                            "No test results were produced. Check the terminal for build errors."
                        }
                    };
                    Label::new(reason).color(Color::Warning).into_any_element()
                }
            },
        };

        v_flex()
            .p_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .when_some(self.run_label.clone(), |this, label| {
                this.child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
            })
            .child(status)
    }

    fn render_failure(
        &self,
        index: usize,
        failure: &TestFailure,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.expanded.contains(&index);
        let location_label = failure.location.as_ref().map(|location| {
            let file_name = location
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            format!("{file_name}:{}", location.line)
        });
        let tooltip_text: SharedString = match &failure.location {
            Some(location) => {
                format!("Go to {}:{}", location.path.display(), location.line).into()
            }
            None => "Go to test".into(),
        };
        let first_message_line = failure.message.lines().next().unwrap_or_default().to_string();

        v_flex()
            .w_full()
            .pb_1()
            .child(
                ListItem::new(("test-failure", index))
                    .spacing(ListItemSpacing::Sparse)
                    .start_slot(
                        h_flex()
                            .gap_1()
                            .child(
                                Disclosure::new(("test-failure-disclosure", index), expanded)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.expanded.remove(&index) {
                                            this.expanded.insert(index);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::Small)
                                    .color(Color::Error),
                            ),
                    )
                    .child(Label::new(failure.name.clone()).truncate())
                    .end_slot::<Label>(location_label.map(|label| {
                        Label::new(label).size(LabelSize::Small).color(Color::Muted)
                    }))
                    .tooltip(Tooltip::text(tooltip_text))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_failure(index, window, cx);
                    })),
            )
            .child(
                v_flex()
                    .pl_8()
                    .pr_2()
                    .gap_1()
                    .when(!expanded && !first_message_line.is_empty(), |this| {
                        this.child(
                            Label::new(first_message_line)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        )
                    })
                    .when(expanded && !failure.message.is_empty(), |this| {
                        this.child(
                            Label::new(failure.message.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                    })
                    .when(expanded && !failure.stack_trace.is_empty(), |this| {
                        this.child(
                            Label::new(failure.stack_trace.clone())
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .buffer_font(cx),
                        )
                    }),
            )
    }
}

fn results_directory_of(task: &SpawnInTerminal) -> Option<PathBuf> {
    task.env.get(RESULTS_DIRECTORY_ENV).map(PathBuf::from)
}

async fn read_results(
    fs: &dyn Fs,
    directory: &Path,
    started_at: SystemTime,
) -> Result<Option<TestRunSummary>> {
    if !fs.is_dir(directory).await {
        return Ok(None);
    }
    let threshold = started_at
        .checked_sub(MTIME_TOLERANCE)
        .unwrap_or(started_at);
    let mut entries = fs.read_dir(directory).await?;
    let mut summary: Option<TestRunSummary> = None;
    while let Some(path) = entries.next().await {
        let path = path?;
        if path.extension().is_none_or(|extension| extension != "trx") {
            continue;
        }
        let is_current = fs
            .metadata(&path)
            .await?
            .is_some_and(|metadata| metadata.mtime.timestamp_for_user() >= threshold);
        if !is_current {
            continue;
        }
        let contents = fs.load(&path).await?;
        let file_summary =
            trx::parse(&contents).with_context(|| format!("parsing {}", path.display()))?;
        summary.get_or_insert_default().merge(file_summary);
    }
    Ok(summary)
}

async fn open_location(
    workspace: WeakEntity<Workspace>,
    location: SourceLocation,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let open = workspace.update_in(cx, |workspace, window, cx| {
        workspace.open_abs_path(
            location.path.clone(),
            OpenOptions {
                visible: Some(OpenVisible::None),
                ..OpenOptions::default()
            },
            window,
            cx,
        )
    })?;
    let item = open.await?;
    if let Some(editor) = item.downcast::<Editor>() {
        editor.update_in(cx, |editor, window, cx| {
            let point = Point::new(location.line.saturating_sub(1), 0);
            editor.go_to_singleton_buffer_point(point, window, cx);
        })?;
    }
    Ok(())
}

/// Used when the failure has no stack trace with source information, e.g. when a test fails in
/// its fixture setup, by asking the language server for the test method.
async fn open_test_symbol(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    failure: &TestFailure,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let method_name = failure
        .method_name
        .clone()
        .context("the test result has no source location")?;
    let class_name = failure
        .class_name
        .as_deref()
        .and_then(|class_name| class_name.rsplit(['.', '+']).next())
        .unwrap_or_default()
        .to_string();

    let symbols = project
        .update(cx, |project, cx| project.symbols(&method_name, cx))
        .await?;
    let call_prefix = format!("{method_name}(");
    let candidates: Vec<_> = symbols
        .into_iter()
        .filter(|symbol| symbol.name == method_name || symbol.name.starts_with(&call_prefix))
        .collect();
    let symbol = candidates
        .iter()
        .find(|symbol| {
            !class_name.is_empty()
                && symbol
                    .container_name
                    .as_deref()
                    .is_some_and(|container| container.contains(&class_name))
        })
        .or_else(|| candidates.first())
        .cloned()
    .with_context(|| format!("could not find the source of test {}", failure.name))?;

    let buffer = project
        .update(cx, |project, cx| project.open_buffer_for_symbol(&symbol, cx))
        .await?;
    workspace.update_in(cx, |workspace, window, cx| {
        let position = buffer
            .read(cx)
            .clip_point_utf16(symbol.range.start, Bias::Left);
        let editor = workspace.open_project_item::<Editor>(
            None, buffer, true, true, true, true, window, cx,
        );
        editor.update(cx, |editor, cx| {
            let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
            let Some(buffer_snapshot) = multibuffer_snapshot.as_singleton() else {
                return;
            };
            let text_anchor = buffer_snapshot.anchor_before(position);
            let Some(anchor) = multibuffer_snapshot.anchor_in_buffer(text_anchor) else {
                return;
            };
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()).unfold(),
                window,
                cx,
                |selections| selections.select_ranges([anchor..anchor]),
            );
        });
    })?;
    Ok(())
}

impl EventEmitter<PanelEvent> for TestResultsPanel {}

impl Focusable for TestResultsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for TestResultsPanel {
    fn persistent_name() -> &'static str {
        "TestResultsPanel"
    }

    fn panel_key() -> &'static str {
        PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(380.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::ListTodo)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Test Results")
    }

    fn icon_label(&self, _: &Window, _: &App) -> Option<String> {
        match &self.state {
            RunState::Finished {
                summary: Some(summary),
                ..
            } if !summary.failures.is_empty() => Some(summary.failures.len().to_string()),
            _ => None,
        }
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Render for TestResultsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let failures = match &self.state {
            RunState::Finished {
                summary: Some(summary),
                ..
            } => summary
                .failures
                .iter()
                .enumerate()
                .map(|(index, failure)| self.render_failure(index, failure, cx).into_any_element())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };

        v_flex()
            .key_context("TestResultsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .id("test-failures")
                    .flex_1()
                    .overflow_y_scroll()
                    .py_1()
                    .children(failures),
            )
    }
}
