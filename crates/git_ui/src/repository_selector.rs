use crate::git_status_icon;
use git::status::{FileStatus, StatusCode, TrackedStatus, UnmergedStatus, UnmergedStatusCode};
use gpui::{App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity};
use picker::{Picker, PickerDelegate, PickerEditorPosition};
use project::{Project, source_control_store::SourceControlStore};
use source_control::{SourceControlContentStatus, SourceControlRepositoryKey};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use workspace::{ModalView, Workspace};

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(open);
}

pub fn open(
    workspace: &mut Workspace,
    _: &zed_actions::git::SelectRepo,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    workspace.toggle_modal(window, cx, |window, cx| {
        RepositorySelector::new(project, rems(34.), window, cx)
    })
}

pub struct RepositorySelector {
    picker: Entity<Picker<RepositorySelectorDelegate>>,
}

impl RepositorySelector {
    pub fn new(
        project_handle: Entity<Project>,
        width: Rems,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let source_control_store = project_handle.read(cx).source_control_store().clone();
        let repository_entries = source_control_store.read_with(cx, |store, cx| {
            let mut repositories = store
                .repositories()
                .map(|snapshot| RepositoryEntry {
                    key: snapshot.repository.key.clone(),
                    provider_name: store
                        .provider_display_name(&snapshot.repository.key.provider_id, cx)
                        .unwrap_or_else(|| Arc::from(snapshot.repository.key.provider_id.as_str())),
                    display_name: snapshot.repository.display_name.clone(),
                    summary: snapshot.repository.summary.clone(),
                    status: repository_status(snapshot.statuses.as_ref()),
                })
                .collect::<Vec<_>>();
            repositories.sort_by(|left, right| {
                left.provider_name
                    .to_lowercase()
                    .cmp(&right.provider_name.to_lowercase())
                    .then_with(|| {
                        left.display_name
                            .to_lowercase()
                            .cmp(&right.display_name.to_lowercase())
                    })
            });
            repositories
        });
        let filtered_repositories = repository_entries.clone();

        let active_repository = source_control_store
            .read(cx)
            .active_repository()
            .map(|snapshot| snapshot.repository.key.clone());
        let selected_index = active_repository
            .as_ref()
            .and_then(|active| {
                filtered_repositories
                    .iter()
                    .position(|repository| &repository.key == active)
            })
            .unwrap_or(0);
        let delegate = RepositorySelectorDelegate {
            repository_selector: cx.entity().downgrade(),
            source_control_store,
            repository_entries,
            filtered_repositories,
            active_repository,
            selected_index,
        };

        let picker = cx.new(|cx| {
            Picker::uniform_list(delegate, window, cx)
                .initial_width(width)
                .show_scrollbar(true)
        });

        RepositorySelector { picker }
    }
}

//pub(crate) fn filtered_repository_entries(
//    git_store: &GitStore,
//    cx: &App,
//) -> Vec<Entity<Repository>> {
//    let repositories = git_store
//        .repositories()
//        .values()
//        .sorted_by_key(|repo| {
//            let repo = repo.read(cx);
//            (
//                repo.dot_git_abs_path.clone(),
//                repo.worktree_abs_path.clone(),
//            )
//        })
//        .collect::<Vec<&Entity<Repository>>>();
//
//    repositories
//        .chunk_by(|a, b| a.read(cx).dot_git_abs_path == b.read(cx).dot_git_abs_path)
//        .flat_map(|chunk| {
//            let has_non_single_file_worktree = chunk
//                .iter()
//                .any(|repo| !repo.read(cx).is_from_single_file_worktree);
//            chunk.iter().filter(move |repo| {
//                // Remove any entry that comes from a single file worktree and represents a repository that is also represented by a non-single-file worktree.
//                !repo.read(cx).is_from_single_file_worktree || !has_non_single_file_worktree
//            })
//        })
//        .map(|&repo| repo.clone())
//        .collect()
//}

impl EventEmitter<DismissEvent> for RepositorySelector {}

impl Focusable for RepositorySelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for RepositorySelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("GitRepositorySelector")
            .child(self.picker.clone())
    }
}

impl ModalView for RepositorySelector {}

pub struct RepositorySelectorDelegate {
    repository_selector: WeakEntity<RepositorySelector>,
    source_control_store: Entity<SourceControlStore>,
    repository_entries: Vec<RepositoryEntry>,
    filtered_repositories: Vec<RepositoryEntry>,
    active_repository: Option<SourceControlRepositoryKey>,
    selected_index: usize,
}

#[derive(Clone)]
struct RepositoryEntry {
    key: SourceControlRepositoryKey,
    provider_name: Arc<str>,
    display_name: Arc<str>,
    summary: Option<Arc<str>>,
    status: Option<FileStatus>,
}

impl PickerDelegate for RepositorySelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "repository selector"
    }

    fn match_count(&self) -> usize {
        self.filtered_repositories.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix.min(self.filtered_repositories.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select a repository...".into()
    }

    fn editor_position(&self) -> PickerEditorPosition {
        PickerEditorPosition::End
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let all_repositories = self.repository_entries.clone();

        let repo_names: Vec<(RepositoryEntry, String)> = all_repositories
            .iter()
            .map(|repository| {
                (
                    repository.clone(),
                    format!("{} {}", repository.provider_name, repository.display_name)
                        .to_lowercase(),
                )
            })
            .collect();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_repositories = cx
                .background_spawn(async move {
                    if query.is_empty() {
                        all_repositories
                    } else {
                        let query_lower = query.to_lowercase();
                        repo_names
                            .into_iter()
                            .filter(|(_, display_name)| display_name.contains(&query_lower))
                            .map(|(repo, _)| repo)
                            .collect()
                    }
                })
                .await;

            this.update_in(cx, |this, window, cx| {
                let mut sorted_repositories = filtered_repositories;
                sorted_repositories.sort_by(|left, right| {
                    left.provider_name
                        .to_lowercase()
                        .cmp(&right.provider_name.to_lowercase())
                        .then_with(|| {
                            left.display_name
                                .to_lowercase()
                                .cmp(&right.display_name.to_lowercase())
                        })
                });
                let selected_index = this
                    .delegate
                    .active_repository
                    .as_ref()
                    .and_then(|active| {
                        sorted_repositories
                            .iter()
                            .position(|repository| &repository.key == active)
                    })
                    .unwrap_or(0);
                this.delegate.filtered_repositories = sorted_repositories;
                this.delegate.set_selected_index(selected_index, window, cx);
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(selected_repo) = self.filtered_repositories.get(self.selected_index) else {
            return;
        };
        self.source_control_store.update(cx, |store, cx| {
            store.set_active_repository(selected_repo.key.clone(), cx);
        });
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.repository_selector
            .update(cx, |_this, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let repository = self.filtered_repositories.get(ix)?;
        let is_active = self
            .active_repository
            .as_ref()
            .is_some_and(|active| active == &repository.key);

        let mut item = ListItem::new(ix)
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                h_flex()
                    .gap_1()
                    .child(Label::new(repository.display_name.clone()))
                    .child(
                        Label::new(repository.provider_name.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                    .when_some(repository.summary.clone(), |this, summary| {
                        this.child(
                            Label::new(summary)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .when(is_active, |this| {
                        this.child(
                            Icon::new(IconName::Check)
                                .size(IconSize::Small)
                                .color(Color::Accent),
                        )
                    }),
            );

        if let Some(status) = repository.status {
            item = item.end_slot(div().pr_2().child(git_status_icon(status)));
        }

        Some(item)
    }
}

fn repository_status(statuses: &[source_control::SourceControlStatusEntry]) -> Option<FileStatus> {
    let mut content_statuses = statuses
        .iter()
        .flat_map(|status| [status.content_status, status.property_status]);
    if content_statuses
        .clone()
        .any(|status| status == SourceControlContentStatus::Conflicted)
    {
        Some(FileStatus::Unmerged(UnmergedStatus {
            first_head: UnmergedStatusCode::Updated,
            second_head: UnmergedStatusCode::Updated,
        }))
    } else if content_statuses.clone().any(|status| {
        matches!(
            status,
            SourceControlContentStatus::Deleted | SourceControlContentStatus::Missing
        )
    }) {
        Some(FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Deleted,
            worktree_status: StatusCode::Unmodified,
        }))
    } else if content_statuses
        .clone()
        .any(|status| status == SourceControlContentStatus::Modified)
    {
        Some(FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Modified,
            worktree_status: StatusCode::Unmodified,
        }))
    } else if content_statuses.any(SourceControlContentStatus::is_changed) {
        Some(FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Added,
            worktree_status: StatusCode::Unmodified,
        }))
    } else {
        None
    }
}
