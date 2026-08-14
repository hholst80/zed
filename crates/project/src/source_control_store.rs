use crate::{
    ProjectEnvironment,
    git_store::{GitStore, GitStoreEvent, RepositoryEvent, RepositoryId},
    worktree_store::{WorktreeStore, WorktreeStoreEvent},
};
use async_trait::async_trait;
use collections::{BTreeMap, HashMap, HashSet};
use extension::{
    Extension, ExtensionHostProxy, ExtensionSourceControlProviderProxy, WorktreeDelegate,
};
use fs::Fs;
use futures::future::Shared;
use git::status::{FileStatus, StatusCode, TrackedStatus};
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, Global, Subscription, Task,
};
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use source_control::{
    SourceControlAuthentication, SourceControlBlameLine, SourceControlCapabilities,
    SourceControlCapability, SourceControlContentStatus, SourceControlError,
    SourceControlErrorKind, SourceControlExecutionOptions, SourceControlHistoryEntry,
    SourceControlNodeKind, SourceControlOperation, SourceControlOperationOutput, SourceControlPath,
    SourceControlProviderId, SourceControlRepository, SourceControlRepositoryId,
    SourceControlRepositoryKey, SourceControlRevision, SourceControlSelectionEligibility,
    SourceControlStatusEntry, SourceControlTrustDecision,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use util::rel_path::RelPath;
use worktree::{Snapshot, WorktreeId};

struct GlobalSourceControlProviderRegistry(Entity<SourceControlProviderRegistry>);

impl Global for GlobalSourceControlProviderRegistry {}

#[derive(Clone)]
pub struct SourceControlProviderDescriptor {
    pub provider_id: SourceControlProviderId,
    pub display_name: Arc<str>,
    pub extension: Arc<dyn Extension>,
}

#[derive(Default)]
pub struct SourceControlProviderRegistry {
    providers: BTreeMap<SourceControlProviderId, SourceControlProviderDescriptor>,
}

impl SourceControlProviderRegistry {
    pub fn init(cx: &mut App) {
        let registry = Self::default_global(cx);
        ExtensionHostProxy::default_global(cx).register_source_control_provider_proxy(
            SourceControlProviderRegistryProxy { registry },
        );
    }

    pub fn default_global(cx: &mut App) -> Entity<Self> {
        if !cx.has_global::<GlobalSourceControlProviderRegistry>() {
            let registry = cx.new(|_| Self::default());
            cx.set_global(GlobalSourceControlProviderRegistry(registry));
        }
        cx.global::<GlobalSourceControlProviderRegistry>().0.clone()
    }

    pub fn providers(&self) -> impl Iterator<Item = &SourceControlProviderDescriptor> {
        self.providers.values()
    }

    pub fn provider(
        &self,
        provider_id: &SourceControlProviderId,
    ) -> Option<&SourceControlProviderDescriptor> {
        self.providers.get(provider_id)
    }
}

struct SourceControlProviderRegistryProxy {
    registry: Entity<SourceControlProviderRegistry>,
}

impl ExtensionSourceControlProviderProxy for SourceControlProviderRegistryProxy {
    fn register_source_control_provider(
        &self,
        extension: Arc<dyn Extension>,
        provider_id: SourceControlProviderId,
        display_name: Arc<str>,
        cx: &mut App,
    ) {
        self.registry.update(cx, |registry, cx| {
            if let Some(existing) = registry.providers.get(&provider_id)
                && existing.extension.manifest().id != extension.manifest().id
            {
                log::error!(
                    "Source-control provider {} is already registered by extension {}; ignoring registration from {}",
                    provider_id,
                    existing.extension.manifest().id,
                    extension.manifest().id,
                );
                return;
            }
            registry.providers.insert(
                provider_id.clone(),
                SourceControlProviderDescriptor {
                    provider_id,
                    display_name,
                    extension,
                },
            );
            cx.notify();
        });
    }

    fn unregister_source_control_provider(
        &self,
        extension_id: &Arc<str>,
        provider_id: &SourceControlProviderId,
        cx: &mut App,
    ) {
        self.registry.update(cx, |registry, cx| {
            if registry
                .providers
                .get(provider_id)
                .is_some_and(|provider| &provider.extension.manifest().id == extension_id)
            {
                registry.providers.remove(provider_id);
                cx.notify();
            }
        });
    }
}

struct LocalSourceControlContext {
    provider_registry: Entity<SourceControlProviderRegistry>,
    worktree_store: Entity<WorktreeStore>,
    environment: Entity<ProjectEnvironment>,
    fs: Arc<dyn Fs>,
}

#[derive(Clone)]
struct RemoteSourceControlContext {
    client: AnyProtoClient,
    project_id: u64,
    read_only: bool,
}

struct SourceControlWorktreeDelegate {
    snapshot: Snapshot,
    fs: Arc<dyn Fs>,
    shell_environment: Shared<Task<Option<HashMap<String, String>>>>,
}

#[async_trait]
impl WorktreeDelegate for SourceControlWorktreeDelegate {
    fn id(&self) -> u64 {
        self.snapshot.id().to_proto()
    }

    fn root_path(&self) -> String {
        self.snapshot.abs_path().to_string_lossy().into_owned()
    }

    async fn read_text_file(&self, path: &RelPath) -> anyhow::Result<String> {
        self.fs
            .load(&self.snapshot.abs_path().join(path.as_std_path()))
            .await
    }

    async fn which(&self, binary_name: String) -> Option<String> {
        let environment = self.shell_environment.clone().await.unwrap_or_default();
        which::which_in(
            binary_name,
            environment.get("PATH"),
            self.snapshot.abs_path(),
        )
        .ok()
        .map(|path| path.to_string_lossy().into_owned())
    }

    async fn shell_env(&self) -> Vec<(String, String)> {
        self.shell_environment
            .clone()
            .await
            .unwrap_or_default()
            .into_iter()
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceControlRepositorySnapshot {
    pub repository: SourceControlRepository,
    pub statuses: Arc<[SourceControlStatusEntry]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceControlStoreEvent {
    RepositoriesChanged,
    RepositoryUpdated(SourceControlRepositoryKey),
    ActiveRepositoryChanged(Option<SourceControlRepositoryKey>),
}

pub struct SourceControlStore {
    git_store: Entity<GitStore>,
    repositories: BTreeMap<SourceControlRepositoryKey, SourceControlRepositorySnapshot>,
    active_repository: Option<SourceControlRepositoryKey>,
    provider_errors: BTreeMap<SourceControlProviderId, SourceControlError>,
    provider_display_names: BTreeMap<SourceControlProviderId, Arc<str>>,
    local: Option<LocalSourceControlContext>,
    remote: Option<RemoteSourceControlContext>,
    allow_remote_mutations: bool,
    extension_refresh_generation: usize,
    extension_refresh_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SourceControlStoreEvent> for SourceControlStore {}

impl SourceControlStore {
    pub fn init(client: &AnyProtoClient) {
        client.add_entity_request_handler(Self::handle_snapshot_request);
        client.add_entity_request_handler(Self::handle_load_text_request);
        client.add_entity_request_handler(Self::handle_diff_request);
        client.add_entity_request_handler(Self::handle_history_request);
        client.add_entity_request_handler(Self::handle_blame_request);
        client.add_entity_request_handler(Self::handle_execute_request);
    }

    pub fn new(git_store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let git_subscription = cx.subscribe(&git_store, |this, _, event, cx| {
            this.handle_git_store_event(event, cx)
        });
        let mut store = Self {
            git_store,
            repositories: BTreeMap::default(),
            active_repository: None,
            provider_errors: BTreeMap::default(),
            provider_display_names: BTreeMap::default(),
            local: None,
            remote: None,
            allow_remote_mutations: false,
            extension_refresh_generation: 0,
            extension_refresh_task: None,
            _subscriptions: vec![git_subscription],
        };
        store.synchronize_git_repositories(cx);
        store
    }

    pub fn remote(
        git_store: Entity<GitStore>,
        client: AnyProtoClient,
        project_id: u64,
        read_only: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut store = Self::new(git_store, cx);
        store.remote = Some(RemoteSourceControlContext {
            client,
            project_id,
            read_only,
        });
        store.schedule_remote_refresh(cx);
        store
    }

    pub fn allow_remote_mutations(&mut self) {
        self.allow_remote_mutations = true;
    }

    pub fn local(
        git_store: Entity<GitStore>,
        worktree_store: Entity<WorktreeStore>,
        environment: Entity<ProjectEnvironment>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut store = Self::new(git_store, cx);
        let provider_registry = SourceControlProviderRegistry::default_global(cx);
        store
            ._subscriptions
            .push(cx.observe(&provider_registry, |store, _, cx| {
                store.schedule_extension_refresh(cx)
            }));
        store
            ._subscriptions
            .push(cx.subscribe(&worktree_store, |store, _, event, cx| {
                if matches!(
                    event,
                    WorktreeStoreEvent::WorktreeAdded(_)
                        | WorktreeStoreEvent::WorktreeRemoved(_, _)
                        | WorktreeStoreEvent::WorktreeReleased(_, _)
                        | WorktreeStoreEvent::WorktreeUpdatedEntries(_, _)
                ) {
                    store.schedule_extension_refresh(cx);
                }
            }));
        store.local = Some(LocalSourceControlContext {
            provider_registry,
            worktree_store,
            environment,
            fs,
        });
        store.schedule_extension_refresh(cx);
        store
    }

    pub fn repositories(&self) -> impl Iterator<Item = &SourceControlRepositorySnapshot> {
        self.repositories.values()
    }

    pub fn repository(
        &self,
        key: &SourceControlRepositoryKey,
    ) -> Option<&SourceControlRepositorySnapshot> {
        self.repositories.get(key)
    }

    pub fn active_repository(&self) -> Option<&SourceControlRepositorySnapshot> {
        self.active_repository
            .as_ref()
            .and_then(|key| self.repositories.get(key))
    }

    pub fn provider_error(
        &self,
        provider_id: &SourceControlProviderId,
    ) -> Option<&SourceControlError> {
        self.provider_errors.get(provider_id)
    }

    pub fn provider_errors(
        &self,
    ) -> impl Iterator<Item = (&SourceControlProviderId, &SourceControlError)> {
        self.provider_errors.iter()
    }

    pub fn provider_display_name(
        &self,
        provider_id: &SourceControlProviderId,
        cx: &App,
    ) -> Option<Arc<str>> {
        if provider_id == &SourceControlProviderId::git() {
            return Some(Arc::from("Git"));
        }
        self.local
            .as_ref()
            .and_then(|local| {
                local
                    .provider_registry
                    .read(cx)
                    .provider(provider_id)
                    .map(|provider| provider.display_name.clone())
            })
            .or_else(|| self.provider_display_names.get(provider_id).cloned())
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.synchronize_git_repositories(cx);
        if self.remote.is_some() {
            self.schedule_remote_refresh(cx);
        } else {
            self.schedule_extension_refresh(cx);
        }
    }

    pub fn load_text(
        &self,
        key: &SourceControlRepositoryKey,
        path: SourceControlPath,
        revision: Option<SourceControlRevision>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Vec<u8>>, SourceControlError>> {
        if let Some(remote) = self.remote.clone() {
            if let Err(error) = self.validate_repository(key, SourceControlCapability::BaseText) {
                return Task::ready(Err(error));
            }
            let request = proto::SourceControlLoadTextRequest {
                project_id: remote.project_id,
                provider_id: key.provider_id.as_str().to_owned(),
                repository_id: key.repository_id.as_str().to_owned(),
                path: path.as_str().to_owned(),
                revision: revision.map(revision_to_proto),
            };
            return cx.background_spawn(async move {
                let response = remote
                    .client
                    .request(request)
                    .await
                    .map_err(remote_transport_error)?;
                if let Some(error) = response.error {
                    return Err(error_from_proto(error));
                }
                Ok(response.contents)
            });
        }

        let (provider, repository, worktree) =
            match self.extension_request(key, SourceControlCapability::BaseText, cx) {
                Ok(request) => request,
                Err(error) => return Task::ready(Err(error)),
            };
        cx.background_spawn(async move {
            provider
                .extension
                .source_control_load_text(
                    provider.provider_id,
                    repository.key.repository_id,
                    path,
                    revision,
                    worktree,
                )
                .await
        })
    }

    pub fn history(
        &self,
        key: &SourceControlRepositoryKey,
        path: Option<SourceControlPath>,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SourceControlHistoryEntry>, SourceControlError>> {
        if let Some(remote) = self.remote.clone() {
            if let Err(error) = self.validate_repository(key, SourceControlCapability::History) {
                return Task::ready(Err(error));
            }
            let request = proto::SourceControlHistoryRequest {
                project_id: remote.project_id,
                provider_id: key.provider_id.as_str().to_owned(),
                repository_id: key.repository_id.as_str().to_owned(),
                path: path.map(|path| path.as_str().to_owned()),
                limit: u32::try_from(limit).unwrap_or(u32::MAX),
            };
            return cx.background_spawn(async move {
                let response = remote
                    .client
                    .request(request)
                    .await
                    .map_err(remote_transport_error)?;
                if let Some(error) = response.error {
                    return Err(error_from_proto(error));
                }
                response
                    .entries
                    .into_iter()
                    .map(history_entry_from_proto)
                    .collect()
            });
        }

        let (provider, repository, worktree) =
            match self.extension_request(key, SourceControlCapability::History, cx) {
                Ok(request) => request,
                Err(error) => return Task::ready(Err(error)),
            };
        cx.background_spawn(async move {
            provider
                .extension
                .source_control_history(
                    provider.provider_id,
                    repository.key.repository_id,
                    path,
                    limit,
                    worktree,
                )
                .await
        })
    }

    pub fn diff(
        &self,
        key: &SourceControlRepositoryKey,
        path: SourceControlPath,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<Arc<str>>, SourceControlError>> {
        if let Some(remote) = self.remote.clone() {
            if let Err(error) = self.validate_repository(key, SourceControlCapability::Diff) {
                return Task::ready(Err(error));
            }
            let request = proto::SourceControlDiffRequest {
                project_id: remote.project_id,
                provider_id: key.provider_id.as_str().to_owned(),
                repository_id: key.repository_id.as_str().to_owned(),
                path: path.as_str().to_owned(),
            };
            return cx.background_spawn(async move {
                let response = remote
                    .client
                    .request(request)
                    .await
                    .map_err(remote_transport_error)?;
                if let Some(error) = response.error {
                    return Err(error_from_proto(error));
                }
                Ok(response.diff.map(Arc::from))
            });
        }

        let (provider, repository, worktree) =
            match self.extension_request(key, SourceControlCapability::Diff, cx) {
                Ok(request) => request,
                Err(error) => return Task::ready(Err(error)),
            };
        cx.background_spawn(async move {
            provider
                .extension
                .source_control_diff(
                    provider.provider_id,
                    repository.key.repository_id,
                    path,
                    worktree,
                )
                .await
        })
    }

    pub fn blame(
        &self,
        key: &SourceControlRepositoryKey,
        path: SourceControlPath,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<SourceControlBlameLine>, SourceControlError>> {
        if let Some(remote) = self.remote.clone() {
            if let Err(error) = self.validate_repository(key, SourceControlCapability::Blame) {
                return Task::ready(Err(error));
            }
            let request = proto::SourceControlBlameRequest {
                project_id: remote.project_id,
                provider_id: key.provider_id.as_str().to_owned(),
                repository_id: key.repository_id.as_str().to_owned(),
                path: path.as_str().to_owned(),
            };
            return cx.background_spawn(async move {
                let response = remote
                    .client
                    .request(request)
                    .await
                    .map_err(remote_transport_error)?;
                if let Some(error) = response.error {
                    return Err(error_from_proto(error));
                }
                response
                    .lines
                    .into_iter()
                    .map(blame_line_from_proto)
                    .collect()
            });
        }

        let (provider, repository, worktree) =
            match self.extension_request(key, SourceControlCapability::Blame, cx) {
                Ok(request) => request,
                Err(error) => return Task::ready(Err(error)),
            };
        cx.background_spawn(async move {
            provider
                .extension
                .source_control_blame(
                    provider.provider_id,
                    repository.key.repository_id,
                    path,
                    worktree,
                )
                .await
        })
    }

    pub fn execute(
        &self,
        key: &SourceControlRepositoryKey,
        operation: SourceControlOperation,
        cx: &mut Context<Self>,
    ) -> Task<Result<SourceControlOperationOutput, SourceControlError>> {
        self.execute_with_options(key, operation, SourceControlExecutionOptions::default(), cx)
    }

    pub fn execute_with_options(
        &self,
        key: &SourceControlRepositoryKey,
        operation: SourceControlOperation,
        options: SourceControlExecutionOptions,
        cx: &mut Context<Self>,
    ) -> Task<Result<SourceControlOperationOutput, SourceControlError>> {
        let capability = operation.required_capability();
        let expected_checkout_destination = match &operation {
            SourceControlOperation::Checkout { destination, .. } => Some(destination.clone()),
            _ => None,
        };
        if let Some(remote) = self.remote.clone() {
            if let Err(error) = self.validate_repository(key, capability) {
                return Task::ready(Err(error));
            }
            if remote.read_only {
                return Task::ready(Err(SourceControlError::new(
                    SourceControlErrorKind::InvalidRequest,
                    "source-control mutations are disabled for collaboration guests",
                )));
            }
            let operation = match operation_to_proto(operation) {
                Ok(operation) => operation,
                Err(error) => return Task::ready(Err(error)),
            };
            let request = proto::SourceControlExecuteRequest {
                project_id: remote.project_id,
                provider_id: key.provider_id.as_str().to_owned(),
                repository_id: key.repository_id.as_str().to_owned(),
                operation: Some(operation),
                options: Some(execution_options_to_proto(options)),
            };
            return cx.spawn(async move |this, cx| {
                let response = remote
                    .client
                    .request(request)
                    .await
                    .map_err(remote_transport_error)?;
                if let Some(error) = response.error {
                    return Err(error_from_proto(error));
                }
                let output = operation_output_from_proto(response);
                validate_operation_output(expected_checkout_destination.as_deref(), &output)?;
                if output.refresh_required {
                    this.update(cx, |this, cx| this.schedule_remote_refresh(cx))
                        .ok();
                }
                Ok(output)
            });
        }

        let (provider, repository, worktree) = match self.extension_request(key, capability, cx) {
            Ok(request) => request,
            Err(error) => return Task::ready(Err(error)),
        };
        cx.spawn(async move |this, cx| {
            let result = provider
                .extension
                .source_control_execute(
                    provider.provider_id,
                    repository.key.repository_id,
                    operation,
                    options,
                    worktree,
                )
                .await;
            let result = result.and_then(|output| {
                validate_operation_output(expected_checkout_destination.as_deref(), &output)?;
                Ok(output)
            });
            if result.as_ref().is_ok_and(|output| output.refresh_required) {
                this.update(cx, |this, cx| this.schedule_extension_refresh(cx))
                    .ok();
            }
            result
        })
    }

    fn extension_request(
        &self,
        key: &SourceControlRepositoryKey,
        capability: SourceControlCapability,
        cx: &mut App,
    ) -> Result<
        (
            SourceControlProviderDescriptor,
            SourceControlRepository,
            Arc<dyn WorktreeDelegate>,
        ),
        SourceControlError,
    > {
        let repository = self.validate_repository(key, capability)?;
        let local = self.local.as_ref().ok_or_else(|| {
            SourceControlError::new(
                SourceControlErrorKind::ProviderUnavailable,
                "extension source-control providers are unavailable for this project",
            )
        })?;
        let provider = local
            .provider_registry
            .read(cx)
            .provider(&key.provider_id)
            .cloned()
            .ok_or_else(|| {
                SourceControlError::new(
                    SourceControlErrorKind::ProviderUnavailable,
                    "the source-control provider is no longer loaded",
                )
            })?;
        let worktree_id = repository.worktree_ids.first().copied().ok_or_else(|| {
            SourceControlError::new(
                SourceControlErrorKind::RepositoryNotFound,
                "the source-control repository has no open worktree",
            )
        })?;
        let worktree = local
            .worktree_store
            .read(cx)
            .worktree_for_id(WorktreeId::from_proto(worktree_id), cx)
            .ok_or_else(|| {
                SourceControlError::new(
                    SourceControlErrorKind::RepositoryNotFound,
                    "the repository worktree is no longer open",
                )
            })?;
        let shell_environment = local.environment.update(cx, |environment, cx| {
            environment.worktree_environment(worktree.clone(), cx)
        });
        let delegate = Arc::new(SourceControlWorktreeDelegate {
            snapshot: worktree.read(cx).snapshot(),
            fs: local.fs.clone(),
            shell_environment,
        });
        Ok((provider, repository, delegate))
    }

    fn validate_repository(
        &self,
        key: &SourceControlRepositoryKey,
        capability: SourceControlCapability,
    ) -> Result<SourceControlRepository, SourceControlError> {
        if key.provider_id == SourceControlProviderId::git() {
            return Err(SourceControlError::unsupported(capability));
        }
        let repository = self
            .repositories
            .get(key)
            .map(|snapshot| snapshot.repository.clone())
            .ok_or_else(|| {
                SourceControlError::new(
                    SourceControlErrorKind::RepositoryNotFound,
                    "the source-control repository is no longer available",
                )
            })?;
        if !repository.capabilities.supports(capability) {
            return Err(SourceControlError::unsupported(capability));
        }
        Ok(repository)
    }

    pub fn set_active_repository(
        &mut self,
        key: SourceControlRepositoryKey,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.repositories.contains_key(&key) {
            return false;
        }

        if self.active_repository.as_ref() != Some(&key) {
            self.active_repository = Some(key.clone());
            cx.emit(SourceControlStoreEvent::ActiveRepositoryChanged(Some(
                key.clone(),
            )));
        }

        if key.provider_id == SourceControlProviderId::git() {
            let Some(repository_id) = key
                .repository_id
                .as_str()
                .parse::<u64>()
                .ok()
                .map(RepositoryId)
            else {
                return false;
            };
            let repository = self
                .git_store
                .read(cx)
                .repositories()
                .get(&repository_id)
                .cloned();
            let Some(repository) = repository else {
                return false;
            };
            repository.update(cx, |repository, cx| repository.set_as_active_repository(cx));
        }
        true
    }

    pub fn replace_provider_repositories(
        &mut self,
        provider_id: &SourceControlProviderId,
        repositories: impl IntoIterator<Item = SourceControlRepositorySnapshot>,
        cx: &mut Context<Self>,
    ) {
        self.repositories
            .retain(|key, _| &key.provider_id != provider_id);
        for snapshot in repositories {
            if &snapshot.repository.key.provider_id == provider_id {
                self.repositories
                    .insert(snapshot.repository.key.clone(), snapshot);
            }
        }

        if self
            .active_repository
            .as_ref()
            .is_none_or(|key| !self.repositories.contains_key(key))
        {
            self.active_repository = self.default_active_repository();
            cx.emit(SourceControlStoreEvent::ActiveRepositoryChanged(
                self.active_repository.clone(),
            ));
        }
        cx.emit(SourceControlStoreEvent::RepositoriesChanged);
    }

    fn schedule_remote_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(remote) = self.remote.clone() else {
            return;
        };

        self.extension_refresh_generation = self.extension_refresh_generation.wrapping_add(1);
        let generation = self.extension_refresh_generation;
        let timer = cx.background_executor().timer(Duration::from_millis(150));
        self.extension_refresh_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let response = remote
                .client
                .request(proto::SourceControlSnapshotRequest {
                    project_id: remote.project_id,
                })
                .await;

            this.update(cx, |this, cx| {
                if this.extension_refresh_generation != generation {
                    return;
                }

                match response {
                    Ok(response) => match remote_snapshot_from_proto(response) {
                        Ok((repositories, provider_names, provider_errors)) => {
                            this.repositories
                                .retain(|key, _| key.provider_id == SourceControlProviderId::git());
                            for snapshot in repositories {
                                this.repositories
                                    .insert(snapshot.repository.key.clone(), snapshot);
                            }
                            this.provider_display_names = provider_names;
                            this.provider_errors = provider_errors;
                        }
                        Err(error) => {
                            this.repositories
                                .retain(|key, _| key.provider_id == SourceControlProviderId::git());
                            this.provider_display_names.clear();
                            this.provider_errors.clear();
                            this.provider_errors
                                .insert(SourceControlProviderId::subversion(), error);
                        }
                    },
                    Err(error) => {
                        this.repositories
                            .retain(|key, _| key.provider_id == SourceControlProviderId::git());
                        this.provider_display_names.clear();
                        this.provider_errors.clear();
                        this.provider_errors.insert(
                            SourceControlProviderId::subversion(),
                            remote_transport_error(error),
                        );
                    }
                }

                if this
                    .active_repository
                    .as_ref()
                    .is_none_or(|key| !this.repositories.contains_key(key))
                {
                    this.active_repository = this.default_active_repository();
                    cx.emit(SourceControlStoreEvent::ActiveRepositoryChanged(
                        this.active_repository.clone(),
                    ));
                }
                cx.emit(SourceControlStoreEvent::RepositoriesChanged);
            })
            .ok();
        }));
    }

    fn schedule_extension_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(local) = &self.local else {
            return;
        };

        self.extension_refresh_generation = self.extension_refresh_generation.wrapping_add(1);
        let generation = self.extension_refresh_generation;
        let providers = local
            .provider_registry
            .read(cx)
            .providers()
            .cloned()
            .collect::<Vec<_>>();
        let worktrees = local
            .worktree_store
            .read(cx)
            .visible_worktrees(cx)
            .collect::<Vec<_>>();
        let delegates = worktrees
            .into_iter()
            .map(|worktree| {
                let shell_environment = local.environment.update(cx, |environment, cx| {
                    environment.worktree_environment(worktree.clone(), cx)
                });
                Arc::new(SourceControlWorktreeDelegate {
                    snapshot: worktree.read(cx).snapshot(),
                    fs: local.fs.clone(),
                    shell_environment,
                }) as Arc<dyn WorktreeDelegate>
            })
            .collect::<Vec<_>>();
        let timer = cx.background_executor().timer(Duration::from_millis(150));

        self.extension_refresh_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let mut refreshed_providers = Vec::with_capacity(providers.len());

            for descriptor in providers {
                let mut repositories = BTreeMap::default();
                let mut provider_error = None;
                for worktree in &delegates {
                    match descriptor
                        .extension
                        .source_control_discover(descriptor.provider_id.clone(), worktree.clone())
                        .await
                    {
                        Ok(discovered) => {
                            for repository in discovered {
                                repositories
                                    .entry(repository.key.clone())
                                    .and_modify(|current: &mut SourceControlRepository| {
                                        for worktree_id in &repository.worktree_ids {
                                            if !current.worktree_ids.contains(worktree_id) {
                                                current.worktree_ids.push(*worktree_id);
                                            }
                                        }
                                        current.worktree_ids.sort_unstable();
                                    })
                                    .or_insert(repository);
                            }
                        }
                        Err(error) => provider_error = Some(error),
                    }
                }

                let mut snapshots = Vec::with_capacity(repositories.len());
                for repository in repositories.into_values() {
                    let delegate = repository.worktree_ids.iter().find_map(|worktree_id| {
                        delegates
                            .iter()
                            .find(|delegate| delegate.id() == *worktree_id)
                            .cloned()
                    });
                    let Some(delegate) = delegate else {
                        provider_error = Some(SourceControlError::new(
                            SourceControlErrorKind::RepositoryNotFound,
                            "the source-control repository has no open worktree",
                        ));
                        continue;
                    };
                    match descriptor
                        .extension
                        .source_control_status(
                            descriptor.provider_id.clone(),
                            repository.key.repository_id.clone(),
                            delegate,
                        )
                        .await
                    {
                        Ok(statuses) => snapshots.push(SourceControlRepositorySnapshot {
                            repository,
                            statuses: statuses.into(),
                        }),
                        Err(error) => provider_error = Some(error),
                    }
                }

                refreshed_providers.push((descriptor.provider_id, snapshots, provider_error));
            }

            this.update(cx, |this, cx| {
                if this.extension_refresh_generation != generation {
                    return;
                }

                this.repositories
                    .retain(|key, _| key.provider_id == SourceControlProviderId::git());
                let previous_provider_errors = std::mem::take(&mut this.provider_errors);
                for (provider_id, snapshots, error) in refreshed_providers {
                    if let Some(error) = error {
                        if previous_provider_errors.get(&provider_id) != Some(&error) {
                            log::warn!(
                                "Source-control provider {provider_id} refresh failed: {error}"
                            );
                        }
                        this.provider_errors.insert(provider_id.clone(), error);
                    }
                    for snapshot in snapshots {
                        if snapshot.repository.key.provider_id == provider_id {
                            this.repositories
                                .insert(snapshot.repository.key.clone(), snapshot);
                        }
                    }
                }

                if this
                    .active_repository
                    .as_ref()
                    .is_none_or(|key| !this.repositories.contains_key(key))
                {
                    this.active_repository = this.default_active_repository();
                    cx.emit(SourceControlStoreEvent::ActiveRepositoryChanged(
                        this.active_repository.clone(),
                    ));
                }
                cx.emit(SourceControlStoreEvent::RepositoriesChanged);
            })
            .ok();
        }));
    }

    async fn handle_snapshot_request(
        this: Entity<Self>,
        _: TypedEnvelope<proto::SourceControlSnapshotRequest>,
        cx: AsyncApp,
    ) -> anyhow::Result<proto::SourceControlSnapshotResponse> {
        Ok(this.read_with(&cx, |this, cx| {
            let repositories = this
                .repositories
                .values()
                .filter(|snapshot| {
                    snapshot.repository.key.provider_id != SourceControlProviderId::git()
                })
                .map(|snapshot| {
                    let provider_name = this
                        .provider_display_name(&snapshot.repository.key.provider_id, cx)
                        .unwrap_or_else(|| {
                            Arc::from(snapshot.repository.key.provider_id.as_str().to_owned())
                        });
                    repository_snapshot_to_proto(snapshot, provider_name)
                })
                .collect();
            let provider_failures = this
                .provider_errors
                .iter()
                .filter(|(provider_id, _)| **provider_id != SourceControlProviderId::git())
                .map(|(provider_id, error)| proto::SourceControlProviderFailure {
                    provider_id: provider_id.as_str().to_owned(),
                    error: Some(error_to_proto(error)),
                })
                .collect();
            proto::SourceControlSnapshotResponse {
                repositories,
                provider_failures,
            }
        }))
    }

    async fn handle_load_text_request(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SourceControlLoadTextRequest>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<proto::SourceControlLoadTextResponse> {
        let payload = envelope.payload;
        let request = (|| {
            Ok((
                repository_key_from_proto(payload.provider_id, payload.repository_id)?,
                SourceControlPath::new(payload.path)?,
                payload.revision.map(revision_from_proto),
            ))
        })();
        let result = match request {
            Ok((key, path, revision)) => {
                this.update(&mut cx, |this, cx| this.load_text(&key, path, revision, cx))
                    .await
            }
            Err(error) => Err(error),
        };
        Ok(match result {
            Ok(contents) => proto::SourceControlLoadTextResponse {
                contents,
                error: None,
            },
            Err(error) => proto::SourceControlLoadTextResponse {
                contents: None,
                error: Some(error_to_proto(&error)),
            },
        })
    }

    async fn handle_diff_request(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SourceControlDiffRequest>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<proto::SourceControlDiffResponse> {
        let payload = envelope.payload;
        let request = (|| {
            Ok((
                repository_key_from_proto(payload.provider_id, payload.repository_id)?,
                SourceControlPath::new(payload.path)?,
            ))
        })();
        let result = match request {
            Ok((key, path)) => {
                this.update(&mut cx, |this, cx| this.diff(&key, path, cx))
                    .await
            }
            Err(error) => Err(error),
        };
        Ok(match result {
            Ok(diff) => proto::SourceControlDiffResponse {
                diff: diff.map(|diff| diff.to_string()),
                error: None,
            },
            Err(error) => proto::SourceControlDiffResponse {
                diff: None,
                error: Some(error_to_proto(&error)),
            },
        })
    }

    async fn handle_history_request(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SourceControlHistoryRequest>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<proto::SourceControlHistoryResponse> {
        let payload = envelope.payload;
        let request = (|| {
            Ok((
                repository_key_from_proto(payload.provider_id, payload.repository_id)?,
                payload.path.map(SourceControlPath::new).transpose()?,
                usize::try_from(payload.limit.min(1_000)).unwrap_or(1_000),
            ))
        })();
        let result = match request {
            Ok((key, path, limit)) => {
                this.update(&mut cx, |this, cx| this.history(&key, path, limit, cx))
                    .await
            }
            Err(error) => Err(error),
        };
        Ok(match result {
            Ok(entries) => proto::SourceControlHistoryResponse {
                entries: entries.into_iter().map(history_entry_to_proto).collect(),
                error: None,
            },
            Err(error) => proto::SourceControlHistoryResponse {
                entries: Vec::new(),
                error: Some(error_to_proto(&error)),
            },
        })
    }

    async fn handle_blame_request(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SourceControlBlameRequest>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<proto::SourceControlBlameResponse> {
        let payload = envelope.payload;
        let request = (|| {
            Ok((
                repository_key_from_proto(payload.provider_id, payload.repository_id)?,
                SourceControlPath::new(payload.path)?,
            ))
        })();
        let result = match request {
            Ok((key, path)) => {
                this.update(&mut cx, |this, cx| this.blame(&key, path, cx))
                    .await
            }
            Err(error) => Err(error),
        };
        Ok(match result {
            Ok(lines) => proto::SourceControlBlameResponse {
                lines: lines.into_iter().map(blame_line_to_proto).collect(),
                error: None,
            },
            Err(error) => proto::SourceControlBlameResponse {
                lines: Vec::new(),
                error: Some(error_to_proto(&error)),
            },
        })
    }

    async fn handle_execute_request(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::SourceControlExecuteRequest>,
        mut cx: AsyncApp,
    ) -> anyhow::Result<proto::SourceControlExecuteResponse> {
        if !this.read_with(&cx, |this, _| this.allow_remote_mutations) {
            return Ok(execute_error_response(SourceControlError::invalid_request(
                "source-control mutations are disabled for collaboration guests",
            )));
        }

        let payload = envelope.payload;
        let request = (|| {
            Ok((
                repository_key_from_proto(payload.provider_id, payload.repository_id)?,
                operation_from_proto(payload.operation.ok_or_else(|| {
                    SourceControlError::invalid_request("missing source-control operation")
                })?)?,
                execution_options_from_proto(payload.options.unwrap_or_default())?,
            ))
        })();
        let result = match request {
            Ok((key, operation, options)) => {
                this.update(&mut cx, |this, cx| {
                    this.execute_with_options(&key, operation, options, cx)
                })
                .await
            }
            Err(error) => Err(error),
        };
        Ok(match result {
            Ok(output) => operation_output_to_proto(output),
            Err(error) => execute_error_response(error),
        })
    }

    fn handle_git_store_event(&mut self, event: &GitStoreEvent, cx: &mut Context<Self>) {
        match event {
            GitStoreEvent::ActiveRepositoryChanged(repository_id) => {
                if self
                    .active_repository
                    .as_ref()
                    .is_some_and(|key| key.provider_id != SourceControlProviderId::git())
                {
                    return;
                }
                let key = repository_id.map(git_repository_key);
                if self.active_repository != key {
                    self.active_repository = key.clone();
                    cx.emit(SourceControlStoreEvent::ActiveRepositoryChanged(key));
                }
            }
            GitStoreEvent::RepositoryUpdated(repository_id, repository_event, _) => {
                if matches!(
                    repository_event,
                    RepositoryEvent::StatusesChanged
                        | RepositoryEvent::HeadChanged
                        | RepositoryEvent::BranchListChanged
                ) && self.synchronize_git_repository(*repository_id, cx)
                {
                    cx.emit(SourceControlStoreEvent::RepositoryUpdated(
                        git_repository_key(*repository_id),
                    ));
                }
            }
            GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_) => {
                self.synchronize_git_repositories(cx);
                cx.emit(SourceControlStoreEvent::RepositoriesChanged);
            }
            GitStoreEvent::IndexWriteError(_)
            | GitStoreEvent::JobsUpdated
            | GitStoreEvent::ConflictsUpdated
            | GitStoreEvent::GlobalConfigurationUpdated
            | GitStoreEvent::DiffBaseChanged(_) => {}
        }
    }

    fn synchronize_git_repositories(&mut self, cx: &mut Context<Self>) {
        let repository_ids = self
            .git_store
            .read(cx)
            .repositories()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let current_keys = repository_ids
            .iter()
            .copied()
            .map(git_repository_key)
            .collect::<HashSet<_>>();
        self.repositories.retain(|key, _| {
            key.provider_id != SourceControlProviderId::git() || current_keys.contains(key)
        });
        for repository_id in repository_ids {
            self.synchronize_git_repository(repository_id, cx);
        }

        let active_git_repository = self
            .git_store
            .read(cx)
            .active_repository()
            .map(|repository| git_repository_key(repository.read(cx).id));
        if self.active_repository.is_none()
            || self
                .active_repository
                .as_ref()
                .is_some_and(|key| key.provider_id == SourceControlProviderId::git())
        {
            self.active_repository =
                active_git_repository.or_else(|| self.default_active_repository());
        }
    }

    fn synchronize_git_repository(&mut self, repository_id: RepositoryId, cx: &App) -> bool {
        let (repository, worktree_ids) = {
            let git_store = self.git_store.read(cx);
            let repository = git_store.repositories().get(&repository_id).cloned();
            let worktree_ids = git_store
                .worktree_ids_for_repository(repository_id)
                .map(|worktree_id| worktree_id.to_proto())
                .collect::<Vec<_>>();
            (repository, worktree_ids)
        };
        let Some(repository) = repository else {
            return false;
        };
        let snapshot = repository.read(cx).snapshot();
        let key = git_repository_key(repository_id);
        let display_name = repository_display_name(&snapshot.work_directory_abs_path);
        let summary = snapshot
            .branch
            .as_ref()
            .map(|branch| Arc::<str>::from(branch.name().to_owned()));
        let statuses = snapshot
            .status()
            .filter_map(|entry| {
                let path = SourceControlPath::new(entry.repo_path.as_unix_str().to_owned()).ok()?;
                let (content_status, selection, copied) = git_status(entry.status);
                Some(SourceControlStatusEntry {
                    path,
                    node_kind: SourceControlNodeKind::Unknown,
                    content_status,
                    property_status: SourceControlContentStatus::Normal,
                    selection,
                    copied,
                    switched: false,
                    locked: false,
                    tree_conflicted: false,
                    out_of_date: false,
                    added_lines: entry.diff_stat.map(|diff| diff.added),
                    deleted_lines: entry.diff_stat.map(|diff| diff.deleted),
                })
            })
            .collect::<Vec<_>>();
        self.repositories.insert(
            key.clone(),
            SourceControlRepositorySnapshot {
                repository: SourceControlRepository {
                    key,
                    worktree_ids,
                    working_copy_path: snapshot.work_directory_abs_path,
                    display_name,
                    summary,
                    capabilities: SourceControlCapabilities::GIT,
                },
                statuses: statuses.into(),
            },
        );
        true
    }

    fn default_active_repository(&self) -> Option<SourceControlRepositoryKey> {
        self.repositories
            .keys()
            .find(|key| key.provider_id == SourceControlProviderId::git())
            .or_else(|| self.repositories.keys().next())
            .cloned()
    }
}

fn repository_snapshot_to_proto(
    snapshot: &SourceControlRepositorySnapshot,
    provider_display_name: Arc<str>,
) -> proto::SourceControlRepositorySnapshot {
    let repository = &snapshot.repository;
    proto::SourceControlRepositorySnapshot {
        provider_id: repository.key.provider_id.as_str().to_owned(),
        repository_id: repository.key.repository_id.as_str().to_owned(),
        worktree_ids: repository.worktree_ids.clone(),
        working_copy_path: repository.working_copy_path.to_string_lossy().into_owned(),
        display_name: repository.display_name.to_string(),
        summary: repository.summary.as_ref().map(ToString::to_string),
        capabilities: Some(capabilities_to_proto(repository.capabilities)),
        statuses: snapshot
            .statuses
            .iter()
            .map(status_entry_to_proto)
            .collect(),
        provider_display_name: provider_display_name.to_string(),
    }
}

type RemoteSnapshot = (
    Vec<SourceControlRepositorySnapshot>,
    BTreeMap<SourceControlProviderId, Arc<str>>,
    BTreeMap<SourceControlProviderId, SourceControlError>,
);

fn remote_snapshot_from_proto(
    response: proto::SourceControlSnapshotResponse,
) -> Result<RemoteSnapshot, SourceControlError> {
    let mut repositories = Vec::with_capacity(response.repositories.len());
    let mut provider_names = BTreeMap::default();
    for repository in response.repositories {
        let (snapshot, provider_name) = repository_snapshot_from_proto(repository)?;
        provider_names
            .entry(snapshot.repository.key.provider_id.clone())
            .or_insert(provider_name);
        repositories.push(snapshot);
    }

    let mut provider_errors = BTreeMap::default();
    for failure in response.provider_failures {
        let provider_id = SourceControlProviderId::new(failure.provider_id)?;
        let error = failure.error.ok_or_else(|| {
            SourceControlError::new(
                SourceControlErrorKind::MalformedResponse,
                "remote source-control provider failure omitted its error",
            )
        })?;
        provider_errors.insert(provider_id, error_from_proto(error));
    }
    Ok((repositories, provider_names, provider_errors))
}

fn repository_snapshot_from_proto(
    snapshot: proto::SourceControlRepositorySnapshot,
) -> Result<(SourceControlRepositorySnapshot, Arc<str>), SourceControlError> {
    let provider_id = SourceControlProviderId::new(snapshot.provider_id)?;
    let repository_id = SourceControlRepositoryId::new(snapshot.repository_id)?;
    let capabilities = snapshot.capabilities.ok_or_else(|| {
        SourceControlError::new(
            SourceControlErrorKind::MalformedResponse,
            "remote source-control repository omitted its capabilities",
        )
    })?;
    let statuses = snapshot
        .statuses
        .into_iter()
        .map(status_entry_from_proto)
        .collect::<Result<Vec<_>, _>>()?;
    let provider_name: Arc<str> = if snapshot.provider_display_name.is_empty() {
        Arc::from(provider_id.as_str().to_owned())
    } else {
        Arc::from(snapshot.provider_display_name)
    };
    let key = SourceControlRepositoryKey::new(provider_id, repository_id);
    Ok((
        SourceControlRepositorySnapshot {
            repository: SourceControlRepository {
                key,
                worktree_ids: snapshot.worktree_ids,
                working_copy_path: Arc::from(PathBuf::from(snapshot.working_copy_path)),
                display_name: Arc::from(snapshot.display_name),
                summary: snapshot.summary.map(Arc::from),
                capabilities: capabilities_from_proto(capabilities),
            },
            statuses: statuses.into(),
        },
        provider_name,
    ))
}

fn capabilities_to_proto(
    capabilities: SourceControlCapabilities,
) -> proto::SourceControlCapabilities {
    proto::SourceControlCapabilities {
        discovery: capabilities.discovery,
        status: capabilities.status,
        base_text: capabilities.base_text,
        diff: capabilities.diff,
        commit: capabilities.commit,
        add: capabilities.add,
        delete: capabilities.delete,
        revert: capabilities.revert,
        resolve: capabilities.resolve,
        ignore: capabilities.ignore,
        update: capabilities.update,
        checkout: capabilities.checkout,
        switch: capabilities.switch,
        history: capabilities.history,
        blame: capabilities.blame,
    }
}

fn capabilities_from_proto(
    capabilities: proto::SourceControlCapabilities,
) -> SourceControlCapabilities {
    SourceControlCapabilities {
        discovery: capabilities.discovery,
        status: capabilities.status,
        base_text: capabilities.base_text,
        diff: capabilities.diff,
        commit: capabilities.commit,
        add: capabilities.add,
        delete: capabilities.delete,
        revert: capabilities.revert,
        resolve: capabilities.resolve,
        ignore: capabilities.ignore,
        update: capabilities.update,
        checkout: capabilities.checkout,
        switch: capabilities.switch,
        history: capabilities.history,
        blame: capabilities.blame,
    }
}

fn status_entry_to_proto(entry: &SourceControlStatusEntry) -> proto::SourceControlStatusEntry {
    proto::SourceControlStatusEntry {
        path: entry.path.as_str().to_owned(),
        node_kind: node_kind_to_proto(entry.node_kind) as i32,
        content_status: content_status_to_proto(entry.content_status) as i32,
        property_status: content_status_to_proto(entry.property_status) as i32,
        selection: selection_to_proto(entry.selection) as i32,
        copied: entry.copied,
        switched: entry.switched,
        locked: entry.locked,
        tree_conflicted: entry.tree_conflicted,
        out_of_date: entry.out_of_date,
        added_lines: entry.added_lines,
        deleted_lines: entry.deleted_lines,
    }
}

fn status_entry_from_proto(
    entry: proto::SourceControlStatusEntry,
) -> Result<SourceControlStatusEntry, SourceControlError> {
    Ok(SourceControlStatusEntry {
        path: SourceControlPath::new(entry.path)?,
        node_kind: node_kind_from_proto(entry.node_kind)?,
        content_status: content_status_from_proto(entry.content_status)?,
        property_status: content_status_from_proto(entry.property_status)?,
        selection: selection_from_proto(entry.selection)?,
        copied: entry.copied,
        switched: entry.switched,
        locked: entry.locked,
        tree_conflicted: entry.tree_conflicted,
        out_of_date: entry.out_of_date,
        added_lines: entry.added_lines,
        deleted_lines: entry.deleted_lines,
    })
}

fn node_kind_to_proto(kind: SourceControlNodeKind) -> proto::SourceControlNodeKind {
    match kind {
        SourceControlNodeKind::File => proto::SourceControlNodeKind::File,
        SourceControlNodeKind::Directory => proto::SourceControlNodeKind::Directory,
        SourceControlNodeKind::SymbolicLink => proto::SourceControlNodeKind::SymbolicLink,
        SourceControlNodeKind::Unknown => proto::SourceControlNodeKind::Unknown,
    }
}

fn node_kind_from_proto(value: i32) -> Result<SourceControlNodeKind, SourceControlError> {
    match proto::SourceControlNodeKind::from_i32(value).ok_or_else(|| malformed_enum(value))? {
        proto::SourceControlNodeKind::File => Ok(SourceControlNodeKind::File),
        proto::SourceControlNodeKind::Directory => Ok(SourceControlNodeKind::Directory),
        proto::SourceControlNodeKind::SymbolicLink => Ok(SourceControlNodeKind::SymbolicLink),
        proto::SourceControlNodeKind::Unknown => Ok(SourceControlNodeKind::Unknown),
    }
}

fn content_status_to_proto(
    status: SourceControlContentStatus,
) -> proto::SourceControlContentStatus {
    match status {
        SourceControlContentStatus::Normal => proto::SourceControlContentStatus::Normal,
        SourceControlContentStatus::Modified => proto::SourceControlContentStatus::Modified,
        SourceControlContentStatus::Added => proto::SourceControlContentStatus::Added,
        SourceControlContentStatus::Deleted => proto::SourceControlContentStatus::Deleted,
        SourceControlContentStatus::Missing => proto::SourceControlContentStatus::Missing,
        SourceControlContentStatus::Replaced => proto::SourceControlContentStatus::Replaced,
        SourceControlContentStatus::Conflicted => proto::SourceControlContentStatus::Conflicted,
        SourceControlContentStatus::Obstructed => proto::SourceControlContentStatus::Obstructed,
        SourceControlContentStatus::Unversioned => proto::SourceControlContentStatus::Unversioned,
        SourceControlContentStatus::Ignored => proto::SourceControlContentStatus::Ignored,
        SourceControlContentStatus::External => proto::SourceControlContentStatus::External,
    }
}

fn content_status_from_proto(value: i32) -> Result<SourceControlContentStatus, SourceControlError> {
    match proto::SourceControlContentStatus::from_i32(value).ok_or_else(|| malformed_enum(value))? {
        proto::SourceControlContentStatus::Normal => Ok(SourceControlContentStatus::Normal),
        proto::SourceControlContentStatus::Modified => Ok(SourceControlContentStatus::Modified),
        proto::SourceControlContentStatus::Added => Ok(SourceControlContentStatus::Added),
        proto::SourceControlContentStatus::Deleted => Ok(SourceControlContentStatus::Deleted),
        proto::SourceControlContentStatus::Missing => Ok(SourceControlContentStatus::Missing),
        proto::SourceControlContentStatus::Replaced => Ok(SourceControlContentStatus::Replaced),
        proto::SourceControlContentStatus::Conflicted => Ok(SourceControlContentStatus::Conflicted),
        proto::SourceControlContentStatus::Obstructed => Ok(SourceControlContentStatus::Obstructed),
        proto::SourceControlContentStatus::Unversioned => {
            Ok(SourceControlContentStatus::Unversioned)
        }
        proto::SourceControlContentStatus::Ignored => Ok(SourceControlContentStatus::Ignored),
        proto::SourceControlContentStatus::External => Ok(SourceControlContentStatus::External),
    }
}

fn selection_to_proto(
    selection: SourceControlSelectionEligibility,
) -> proto::SourceControlSelectionEligibility {
    match selection {
        SourceControlSelectionEligibility::IncludedByDefault => {
            proto::SourceControlSelectionEligibility::IncludedByDefault
        }
        SourceControlSelectionEligibility::Available => {
            proto::SourceControlSelectionEligibility::Available
        }
        SourceControlSelectionEligibility::RequiresAdd => {
            proto::SourceControlSelectionEligibility::RequiresAdd
        }
        SourceControlSelectionEligibility::RequiresDelete => {
            proto::SourceControlSelectionEligibility::RequiresDelete
        }
        SourceControlSelectionEligibility::Unavailable => {
            proto::SourceControlSelectionEligibility::Unavailable
        }
    }
}

fn selection_from_proto(
    value: i32,
) -> Result<SourceControlSelectionEligibility, SourceControlError> {
    match proto::SourceControlSelectionEligibility::from_i32(value)
        .ok_or_else(|| malformed_enum(value))?
    {
        proto::SourceControlSelectionEligibility::IncludedByDefault => {
            Ok(SourceControlSelectionEligibility::IncludedByDefault)
        }
        proto::SourceControlSelectionEligibility::Available => {
            Ok(SourceControlSelectionEligibility::Available)
        }
        proto::SourceControlSelectionEligibility::RequiresAdd => {
            Ok(SourceControlSelectionEligibility::RequiresAdd)
        }
        proto::SourceControlSelectionEligibility::RequiresDelete => {
            Ok(SourceControlSelectionEligibility::RequiresDelete)
        }
        proto::SourceControlSelectionEligibility::Unavailable => {
            Ok(SourceControlSelectionEligibility::Unavailable)
        }
    }
}

fn malformed_enum(error: impl std::fmt::Display) -> SourceControlError {
    SourceControlError::new(
        SourceControlErrorKind::MalformedResponse,
        format!("remote source-control response contains an unknown enum value: {error}"),
    )
}

fn revision_to_proto(revision: SourceControlRevision) -> proto::SourceControlRevision {
    proto::SourceControlRevision {
        canonical: revision.canonical.to_string(),
        display: revision.display.to_string(),
    }
}

fn revision_from_proto(revision: proto::SourceControlRevision) -> SourceControlRevision {
    SourceControlRevision::new(revision.canonical, revision.display)
}

fn history_entry_to_proto(entry: SourceControlHistoryEntry) -> proto::SourceControlHistoryEntry {
    proto::SourceControlHistoryEntry {
        revision: Some(revision_to_proto(entry.revision)),
        author: entry.author.map(|author| author.to_string()),
        timestamp: entry.timestamp.map(|timestamp| timestamp.to_string()),
        message: entry.message.to_string(),
        changed_paths: entry
            .changed_paths
            .into_iter()
            .map(|path| path.as_str().to_owned())
            .collect(),
    }
}

fn history_entry_from_proto(
    entry: proto::SourceControlHistoryEntry,
) -> Result<SourceControlHistoryEntry, SourceControlError> {
    Ok(SourceControlHistoryEntry {
        revision: entry.revision.map(revision_from_proto).ok_or_else(|| {
            SourceControlError::new(
                SourceControlErrorKind::MalformedResponse,
                "remote source-control history entry omitted its revision",
            )
        })?,
        author: entry.author.map(Arc::from),
        timestamp: entry.timestamp.map(Arc::from),
        message: Arc::from(entry.message),
        changed_paths: entry
            .changed_paths
            .into_iter()
            .map(SourceControlPath::new)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn blame_line_to_proto(line: SourceControlBlameLine) -> proto::SourceControlBlameLine {
    proto::SourceControlBlameLine {
        line_number: line.line_number,
        revision: line.revision.map(revision_to_proto),
        author: line.author.map(|author| author.to_string()),
        timestamp: line.timestamp.map(|timestamp| timestamp.to_string()),
    }
}

fn blame_line_from_proto(
    line: proto::SourceControlBlameLine,
) -> Result<SourceControlBlameLine, SourceControlError> {
    Ok(SourceControlBlameLine {
        line_number: line.line_number,
        revision: line.revision.map(revision_from_proto),
        author: line.author.map(Arc::from),
        timestamp: line.timestamp.map(Arc::from),
    })
}

fn repository_key_from_proto(
    provider_id: String,
    repository_id: String,
) -> Result<SourceControlRepositoryKey, SourceControlError> {
    Ok(SourceControlRepositoryKey::new(
        SourceControlProviderId::new(provider_id)?,
        SourceControlRepositoryId::new(repository_id)?,
    ))
}

fn operation_to_proto(
    operation: SourceControlOperation,
) -> Result<proto::SourceControlOperation, SourceControlError> {
    use proto::source_control_operation::Operation;

    let operation = match operation {
        SourceControlOperation::Commit { paths, message } => {
            Operation::Commit(proto::SourceControlCommitOperation {
                paths: paths_to_proto(paths),
                message,
            })
        }
        SourceControlOperation::Add { paths } => {
            Operation::Add(proto::SourceControlPathsOperation {
                paths: paths_to_proto(paths),
            })
        }
        SourceControlOperation::Delete { paths } => {
            Operation::Delete(proto::SourceControlPathsOperation {
                paths: paths_to_proto(paths),
            })
        }
        SourceControlOperation::Revert { paths } => {
            Operation::Revert(proto::SourceControlPathsOperation {
                paths: paths_to_proto(paths),
            })
        }
        SourceControlOperation::Resolve { paths } => {
            Operation::Resolve(proto::SourceControlPathsOperation {
                paths: paths_to_proto(paths),
            })
        }
        SourceControlOperation::Ignore { directory, pattern } => {
            Operation::Ignore(proto::SourceControlIgnoreOperation {
                directory: directory.as_str().to_owned(),
                pattern,
            })
        }
        SourceControlOperation::Update => Operation::Update(proto::SourceControlNoArguments {}),
        SourceControlOperation::Checkout {
            url,
            destination,
            revision,
        } => Operation::Checkout(proto::SourceControlCheckoutOperation {
            url,
            destination: destination
                .to_str()
                .ok_or_else(|| {
                    SourceControlError::invalid_request(
                        "checkout destination is not valid Unicode for remote transport",
                    )
                })?
                .to_owned(),
            revision: revision.map(revision_to_proto),
        }),
        SourceControlOperation::Switch { url, revision } => {
            Operation::Switch(proto::SourceControlSwitchOperation {
                url,
                revision: revision.map(revision_to_proto),
            })
        }
    };
    Ok(proto::SourceControlOperation {
        operation: Some(operation),
    })
}

fn operation_from_proto(
    operation: proto::SourceControlOperation,
) -> Result<SourceControlOperation, SourceControlError> {
    use proto::source_control_operation::Operation;

    match operation.operation.ok_or_else(|| {
        SourceControlError::invalid_request("missing source-control operation variant")
    })? {
        Operation::Commit(operation) => Ok(SourceControlOperation::Commit {
            paths: paths_from_proto(operation.paths)?,
            message: operation.message,
        }),
        Operation::Add(operation) => Ok(SourceControlOperation::Add {
            paths: paths_from_proto(operation.paths)?,
        }),
        Operation::Delete(operation) => Ok(SourceControlOperation::Delete {
            paths: paths_from_proto(operation.paths)?,
        }),
        Operation::Revert(operation) => Ok(SourceControlOperation::Revert {
            paths: paths_from_proto(operation.paths)?,
        }),
        Operation::Resolve(operation) => Ok(SourceControlOperation::Resolve {
            paths: paths_from_proto(operation.paths)?,
        }),
        Operation::Ignore(operation) => Ok(SourceControlOperation::Ignore {
            directory: SourceControlPath::new(operation.directory)?,
            pattern: operation.pattern,
        }),
        Operation::Update(_) => Ok(SourceControlOperation::Update),
        Operation::Checkout(operation) => Ok(SourceControlOperation::Checkout {
            url: operation.url,
            destination: PathBuf::from(operation.destination),
            revision: operation.revision.map(revision_from_proto),
        }),
        Operation::Switch(operation) => Ok(SourceControlOperation::Switch {
            url: operation.url,
            revision: operation.revision.map(revision_from_proto),
        }),
    }
}

fn execution_options_to_proto(
    options: SourceControlExecutionOptions,
) -> proto::SourceControlExecutionOptions {
    proto::SourceControlExecutionOptions {
        authentication: options.authentication.map(|authentication| {
            proto::SourceControlAuthentication {
                username: authentication.username.to_string(),
                password: authentication.password.as_bytes().to_vec(),
            }
        }),
        trust: options
            .trust
            .map(|trust| proto::SourceControlTrustDecision {
                failures: trust
                    .failures
                    .into_iter()
                    .map(|failure| failure.to_string())
                    .collect(),
            }),
    }
}

fn execution_options_from_proto(
    options: proto::SourceControlExecutionOptions,
) -> Result<SourceControlExecutionOptions, SourceControlError> {
    let authentication = options
        .authentication
        .map(|authentication| {
            let password = String::from_utf8(authentication.password).map_err(|_| {
                SourceControlError::invalid_request("source-control password is not valid UTF-8")
            })?;
            Ok(SourceControlAuthentication {
                username: Arc::from(authentication.username),
                password: password.into(),
            })
        })
        .transpose()?;
    let trust = options.trust.map(|trust| SourceControlTrustDecision {
        failures: trust.failures.into_iter().map(Arc::from).collect(),
    });
    Ok(SourceControlExecutionOptions {
        authentication,
        trust,
    })
}

fn paths_to_proto(paths: Vec<SourceControlPath>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.as_str().to_owned())
        .collect()
}

fn paths_from_proto(paths: Vec<String>) -> Result<Vec<SourceControlPath>, SourceControlError> {
    paths.into_iter().map(SourceControlPath::new).collect()
}

fn operation_output_to_proto(
    output: SourceControlOperationOutput,
) -> proto::SourceControlExecuteResponse {
    proto::SourceControlExecuteResponse {
        message: output.message.map(|message| message.to_string()),
        opened_path: output
            .opened_path
            .map(|path| path.to_string_lossy().into_owned()),
        refresh_required: output.refresh_required,
        error: None,
    }
}

fn operation_output_from_proto(
    output: proto::SourceControlExecuteResponse,
) -> SourceControlOperationOutput {
    SourceControlOperationOutput {
        message: output.message.map(Arc::from),
        opened_path: output.opened_path.map(PathBuf::from),
        refresh_required: output.refresh_required,
    }
}

fn validate_operation_output(
    expected_checkout_destination: Option<&Path>,
    output: &SourceControlOperationOutput,
) -> Result<(), SourceControlError> {
    match (expected_checkout_destination, output.opened_path.as_deref()) {
        (Some(expected), Some(actual)) if expected == actual => Ok(()),
        (Some(_), Some(_)) => Err(SourceControlError::new(
            SourceControlErrorKind::MalformedResponse,
            "source-control provider returned a checkout path other than the selected destination",
        )),
        (Some(_), None) => Err(SourceControlError::new(
            SourceControlErrorKind::MalformedResponse,
            "source-control provider did not return the completed checkout path",
        )),
        (None, Some(_)) => Err(SourceControlError::new(
            SourceControlErrorKind::MalformedResponse,
            "source-control provider returned an unexpected path",
        )),
        (None, None) => Ok(()),
    }
}

fn execute_error_response(error: SourceControlError) -> proto::SourceControlExecuteResponse {
    proto::SourceControlExecuteResponse {
        message: None,
        opened_path: None,
        refresh_required: false,
        error: Some(error_to_proto(&error)),
    }
}

fn error_to_proto(error: &SourceControlError) -> proto::SourceControlError {
    let mut encoded = proto::SourceControlError {
        kind: proto::SourceControlErrorKind::ProviderUnavailable as i32,
        message: error.message.to_string(),
        capability: None,
        realm: None,
        username: None,
        fingerprint: None,
        trust_failures: Vec::new(),
        exit_code: None,
    };
    encoded.kind = match &error.kind {
        SourceControlErrorKind::ProviderUnavailable => {
            proto::SourceControlErrorKind::ProviderUnavailable
        }
        SourceControlErrorKind::RepositoryNotFound => {
            proto::SourceControlErrorKind::RepositoryNotFound
        }
        SourceControlErrorKind::UnsupportedOperation { capability } => {
            encoded.capability = Some(capability_to_wire(*capability).to_owned());
            proto::SourceControlErrorKind::UnsupportedOperation
        }
        SourceControlErrorKind::AuthenticationRequired { realm, username } => {
            encoded.realm = Some(realm.to_string());
            encoded.username = username.as_ref().map(ToString::to_string);
            proto::SourceControlErrorKind::AuthenticationRequired
        }
        SourceControlErrorKind::TrustRequired {
            realm,
            fingerprint,
            failures,
        } => {
            encoded.realm = Some(realm.to_string());
            encoded.fingerprint = Some(fingerprint.to_string());
            encoded.trust_failures = failures.iter().map(ToString::to_string).collect();
            proto::SourceControlErrorKind::TrustRequired
        }
        SourceControlErrorKind::Cancelled => proto::SourceControlErrorKind::Cancelled,
        SourceControlErrorKind::StaleState => proto::SourceControlErrorKind::StaleState,
        SourceControlErrorKind::CommandFailed { exit_code } => {
            encoded.exit_code = *exit_code;
            proto::SourceControlErrorKind::CommandFailed
        }
        SourceControlErrorKind::MalformedResponse => {
            proto::SourceControlErrorKind::MalformedResponse
        }
        SourceControlErrorKind::TransportFailure => proto::SourceControlErrorKind::TransportFailure,
        SourceControlErrorKind::InvalidRequest => proto::SourceControlErrorKind::InvalidRequest,
    } as i32;
    encoded
}

fn error_from_proto(error: proto::SourceControlError) -> SourceControlError {
    let kind = match proto::SourceControlErrorKind::from_i32(error.kind) {
        Some(proto::SourceControlErrorKind::ProviderUnavailable) => {
            SourceControlErrorKind::ProviderUnavailable
        }
        Some(proto::SourceControlErrorKind::RepositoryNotFound) => {
            SourceControlErrorKind::RepositoryNotFound
        }
        Some(proto::SourceControlErrorKind::UnsupportedOperation) => {
            let Some(capability) = error.capability.as_deref().and_then(capability_from_wire)
            else {
                return SourceControlError::new(
                    SourceControlErrorKind::MalformedResponse,
                    "remote unsupported-operation error omitted a valid capability",
                );
            };
            SourceControlErrorKind::UnsupportedOperation { capability }
        }
        Some(proto::SourceControlErrorKind::AuthenticationRequired) => {
            SourceControlErrorKind::AuthenticationRequired {
                realm: Arc::from(error.realm.unwrap_or_else(|| "unknown realm".to_owned())),
                username: error.username.map(Arc::from),
            }
        }
        Some(proto::SourceControlErrorKind::TrustRequired) => {
            SourceControlErrorKind::TrustRequired {
                realm: Arc::from(error.realm.unwrap_or_else(|| "unknown realm".to_owned())),
                fingerprint: Arc::from(
                    error
                        .fingerprint
                        .unwrap_or_else(|| "unknown fingerprint".to_owned()),
                ),
                failures: error.trust_failures.into_iter().map(Arc::from).collect(),
            }
        }
        Some(proto::SourceControlErrorKind::Cancelled) => SourceControlErrorKind::Cancelled,
        Some(proto::SourceControlErrorKind::StaleState) => SourceControlErrorKind::StaleState,
        Some(proto::SourceControlErrorKind::CommandFailed) => {
            SourceControlErrorKind::CommandFailed {
                exit_code: error.exit_code,
            }
        }
        Some(proto::SourceControlErrorKind::MalformedResponse) => {
            SourceControlErrorKind::MalformedResponse
        }
        Some(proto::SourceControlErrorKind::TransportFailure) => {
            SourceControlErrorKind::TransportFailure
        }
        Some(proto::SourceControlErrorKind::InvalidRequest) => {
            SourceControlErrorKind::InvalidRequest
        }
        None => SourceControlErrorKind::MalformedResponse,
    };
    SourceControlError::new(kind, error.message)
}

fn capability_to_wire(capability: SourceControlCapability) -> &'static str {
    match capability {
        SourceControlCapability::Discovery => "discovery",
        SourceControlCapability::Status => "status",
        SourceControlCapability::BaseText => "base_text",
        SourceControlCapability::Diff => "diff",
        SourceControlCapability::Commit => "commit",
        SourceControlCapability::Add => "add",
        SourceControlCapability::Delete => "delete",
        SourceControlCapability::Revert => "revert",
        SourceControlCapability::Resolve => "resolve",
        SourceControlCapability::Ignore => "ignore",
        SourceControlCapability::Update => "update",
        SourceControlCapability::Checkout => "checkout",
        SourceControlCapability::Switch => "switch",
        SourceControlCapability::History => "history",
        SourceControlCapability::Blame => "blame",
    }
}

fn capability_from_wire(capability: &str) -> Option<SourceControlCapability> {
    match capability {
        "discovery" => Some(SourceControlCapability::Discovery),
        "status" => Some(SourceControlCapability::Status),
        "base_text" => Some(SourceControlCapability::BaseText),
        "diff" => Some(SourceControlCapability::Diff),
        "commit" => Some(SourceControlCapability::Commit),
        "add" => Some(SourceControlCapability::Add),
        "delete" => Some(SourceControlCapability::Delete),
        "revert" => Some(SourceControlCapability::Revert),
        "resolve" => Some(SourceControlCapability::Resolve),
        "ignore" => Some(SourceControlCapability::Ignore),
        "update" => Some(SourceControlCapability::Update),
        "checkout" => Some(SourceControlCapability::Checkout),
        "switch" => Some(SourceControlCapability::Switch),
        "history" => Some(SourceControlCapability::History),
        "blame" => Some(SourceControlCapability::Blame),
        _ => None,
    }
}

fn remote_transport_error(error: anyhow::Error) -> SourceControlError {
    SourceControlError::new(
        SourceControlErrorKind::TransportFailure,
        format!(
            "remote source-control request failed; upgrade the remote Zed server if it does not support source-control providers: {error}"
        ),
    )
}

fn git_repository_key(repository_id: RepositoryId) -> SourceControlRepositoryKey {
    SourceControlRepositoryKey::new(
        SourceControlProviderId::git(),
        SourceControlRepositoryId::from_u64(repository_id.0),
    )
}

fn repository_display_name(path: &Path) -> Arc<str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(Arc::from)
        .unwrap_or_else(|| Arc::from(path.to_string_lossy().into_owned()))
}

fn git_status(
    status: FileStatus,
) -> (
    SourceControlContentStatus,
    SourceControlSelectionEligibility,
    bool,
) {
    match status {
        FileStatus::Untracked => (
            SourceControlContentStatus::Unversioned,
            SourceControlSelectionEligibility::Available,
            false,
        ),
        FileStatus::Ignored => (
            SourceControlContentStatus::Ignored,
            SourceControlSelectionEligibility::Unavailable,
            false,
        ),
        FileStatus::Unmerged(_) => (
            SourceControlContentStatus::Conflicted,
            SourceControlSelectionEligibility::Available,
            false,
        ),
        FileStatus::Tracked(TrackedStatus {
            index_status,
            worktree_status,
        }) => {
            let status = if worktree_status == StatusCode::Unmodified {
                index_status
            } else {
                worktree_status
            };
            let selection = if index_status == StatusCode::Unmodified {
                SourceControlSelectionEligibility::Available
            } else {
                SourceControlSelectionEligibility::IncludedByDefault
            };
            let copied =
                index_status == StatusCode::Copied || worktree_status == StatusCode::Copied;
            (git_status_code(status), selection, copied)
        }
    }
}

fn git_status_code(status: StatusCode) -> SourceControlContentStatus {
    match status {
        StatusCode::Modified | StatusCode::TypeChanged => SourceControlContentStatus::Modified,
        StatusCode::Added => SourceControlContentStatus::Added,
        StatusCode::Deleted => SourceControlContentStatus::Deleted,
        StatusCode::Renamed | StatusCode::Copied => SourceControlContentStatus::Replaced,
        StatusCode::Unmodified => SourceControlContentStatus::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_snapshot_round_trip_preserves_provider_metadata() -> Result<(), SourceControlError> {
        let key = SourceControlRepositoryKey::new(
            SourceControlProviderId::subversion(),
            SourceControlRepositoryId::new("uuid:/work/project")?,
        );
        let snapshot = SourceControlRepositorySnapshot {
            repository: SourceControlRepository {
                key,
                worktree_ids: vec![7],
                working_copy_path: Arc::from(Path::new("/work/project")),
                display_name: Arc::from("project"),
                summary: Some(Arc::from("r42 · ^/trunk")),
                capabilities: SourceControlCapabilities::SUBVERSION,
            },
            statuses: vec![SourceControlStatusEntry {
                path: SourceControlPath::new("src/main.rs")?,
                node_kind: SourceControlNodeKind::File,
                content_status: SourceControlContentStatus::Modified,
                property_status: SourceControlContentStatus::Normal,
                selection: SourceControlSelectionEligibility::IncludedByDefault,
                copied: false,
                switched: true,
                locked: false,
                tree_conflicted: false,
                out_of_date: true,
                added_lines: Some(2),
                deleted_lines: Some(1),
            }]
            .into(),
        };

        let encoded = repository_snapshot_to_proto(&snapshot, Arc::from("Subversion"));
        let (decoded, provider_name) = repository_snapshot_from_proto(encoded)?;

        assert_eq!(decoded, snapshot);
        assert_eq!(provider_name.as_ref(), "Subversion");
        Ok(())
    }

    #[test]
    fn remote_errors_round_trip_authentication_and_trust_details() {
        let errors = [
            SourceControlError::new(
                SourceControlErrorKind::AuthenticationRequired {
                    realm: Arc::from("repository"),
                    username: Some(Arc::from("alice")),
                },
                "authentication required",
            ),
            SourceControlError::new(
                SourceControlErrorKind::TrustRequired {
                    realm: Arc::from("TLS server"),
                    fingerprint: Arc::from("AA:BB"),
                    failures: vec![Arc::from("unknown-ca")],
                },
                "trust required",
            ),
        ];

        for error in errors {
            assert_eq!(error_from_proto(error_to_proto(&error)), error);
        }
    }

    #[test]
    fn remote_execution_options_preserve_secrets_without_debugging_them()
    -> Result<(), SourceControlError> {
        let options = SourceControlExecutionOptions {
            authentication: Some(SourceControlAuthentication {
                username: Arc::from("alice"),
                password: "top-secret".to_owned().into(),
            }),
            trust: Some(SourceControlTrustDecision {
                failures: vec![Arc::from("unknown-ca")],
            }),
        };

        let decoded = execution_options_from_proto(execution_options_to_proto(options))?;
        let authentication = decoded
            .authentication
            .as_ref()
            .ok_or_else(|| SourceControlError::invalid_request("authentication was not decoded"))?;
        assert_eq!(authentication.username.as_ref(), "alice");
        assert_eq!(authentication.password.as_str(), "top-secret");
        assert!(!format!("{decoded:?}").contains("top-secret"));
        assert_eq!(
            decoded
                .trust
                .as_ref()
                .map(|trust| trust.failures.as_slice()),
            Some([Arc::from("unknown-ca")].as_slice())
        );
        Ok(())
    }

    #[test]
    fn checkout_output_must_match_the_selected_destination() {
        let output = SourceControlOperationOutput {
            opened_path: Some(PathBuf::from("/different/path")),
            ..SourceControlOperationOutput::default()
        };

        assert!(matches!(
            validate_operation_output(Some(Path::new("/selected/path")), &output),
            Err(SourceControlError {
                kind: SourceControlErrorKind::MalformedResponse,
                ..
            })
        ));
    }
}
