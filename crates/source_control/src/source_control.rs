use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};
use thiserror::Error;
use zeroize::Zeroizing;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceControlProviderId(Arc<str>);

impl SourceControlProviderId {
    pub fn new(id: impl Into<Arc<str>>) -> Result<Self, SourceControlError> {
        let id = id.into();
        if id.is_empty()
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(SourceControlError::invalid_request(
                "provider IDs must contain only lowercase ASCII letters, digits, and hyphens",
            ));
        }
        Ok(Self(id))
    }

    pub fn git() -> Self {
        Self(Arc::from("git"))
    }

    pub fn subversion() -> Self {
        Self(Arc::from("subversion"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceControlProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceControlRepositoryId(Arc<str>);

impl SourceControlRepositoryId {
    pub fn new(id: impl Into<Arc<str>>) -> Result<Self, SourceControlError> {
        let id = id.into();
        if id.is_empty() {
            return Err(SourceControlError::invalid_request(
                "repository IDs must not be empty",
            ));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn from_u64(id: u64) -> Self {
        Self(Arc::from(id.to_string()))
    }
}

impl fmt::Display for SourceControlRepositoryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SourceControlRepositoryKey {
    pub provider_id: SourceControlProviderId,
    pub repository_id: SourceControlRepositoryId,
}

impl SourceControlRepositoryKey {
    pub fn new(
        provider_id: SourceControlProviderId,
        repository_id: SourceControlRepositoryId,
    ) -> Self {
        Self {
            provider_id,
            repository_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceControlPath(Arc<str>);

impl SourceControlPath {
    pub fn new(path: impl Into<Arc<str>>) -> Result<Self, SourceControlError> {
        let path = path.into();
        if path.is_empty() {
            return Ok(Self(path));
        }
        if path.starts_with('/')
            || path.starts_with('\\')
            || path.contains('\\')
            || path
                .split('/')
                .any(|component| component.is_empty() || component == "." || component == "..")
        {
            return Err(SourceControlError::invalid_request(
                "source-control paths must be normalized repository-relative paths",
            ));
        }
        Ok(Self(path))
    }

    pub fn root() -> Self {
        Self(Arc::from(""))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SourceControlPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceControlCapability {
    Discovery,
    Status,
    BaseText,
    Diff,
    Commit,
    Add,
    Delete,
    Revert,
    Resolve,
    Ignore,
    Update,
    Checkout,
    Switch,
    History,
    Blame,
}

impl fmt::Display for SourceControlCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Discovery => "discovery",
            Self::Status => "status",
            Self::BaseText => "base text",
            Self::Diff => "diff",
            Self::Commit => "commit",
            Self::Add => "add",
            Self::Delete => "delete",
            Self::Revert => "revert",
            Self::Resolve => "resolve",
            Self::Ignore => "ignore",
            Self::Update => "update",
            Self::Checkout => "checkout",
            Self::Switch => "switch",
            Self::History => "history",
            Self::Blame => "blame",
        };
        formatter.write_str(name)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlCapabilities {
    pub discovery: bool,
    pub status: bool,
    pub base_text: bool,
    pub diff: bool,
    pub commit: bool,
    pub add: bool,
    pub delete: bool,
    pub revert: bool,
    pub resolve: bool,
    pub ignore: bool,
    pub update: bool,
    pub checkout: bool,
    pub switch: bool,
    pub history: bool,
    pub blame: bool,
}

impl SourceControlCapabilities {
    pub const NONE: Self = Self {
        discovery: false,
        status: false,
        base_text: false,
        diff: false,
        commit: false,
        add: false,
        delete: false,
        revert: false,
        resolve: false,
        ignore: false,
        update: false,
        checkout: false,
        switch: false,
        history: false,
        blame: false,
    };

    pub const GIT: Self = Self {
        discovery: true,
        status: true,
        base_text: true,
        diff: true,
        commit: true,
        add: true,
        delete: true,
        revert: true,
        resolve: true,
        ignore: true,
        update: true,
        checkout: true,
        switch: true,
        history: true,
        blame: true,
    };

    pub const SUBVERSION: Self = Self {
        discovery: true,
        status: true,
        base_text: true,
        diff: true,
        commit: true,
        add: true,
        delete: true,
        revert: true,
        resolve: true,
        ignore: true,
        update: true,
        checkout: true,
        switch: true,
        history: true,
        blame: true,
    };

    pub fn supports(self, capability: SourceControlCapability) -> bool {
        match capability {
            SourceControlCapability::Discovery => self.discovery,
            SourceControlCapability::Status => self.status,
            SourceControlCapability::BaseText => self.base_text,
            SourceControlCapability::Diff => self.diff,
            SourceControlCapability::Commit => self.commit,
            SourceControlCapability::Add => self.add,
            SourceControlCapability::Delete => self.delete,
            SourceControlCapability::Revert => self.revert,
            SourceControlCapability::Resolve => self.resolve,
            SourceControlCapability::Ignore => self.ignore,
            SourceControlCapability::Update => self.update,
            SourceControlCapability::Checkout => self.checkout,
            SourceControlCapability::Switch => self.switch,
            SourceControlCapability::History => self.history,
            SourceControlCapability::Blame => self.blame,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceControlWorktree {
    pub id: u64,
    pub root_path: Arc<Path>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlRepository {
    pub key: SourceControlRepositoryKey,
    pub worktree_ids: Vec<u64>,
    pub working_copy_path: Arc<Path>,
    pub display_name: Arc<str>,
    pub summary: Option<Arc<str>>,
    pub capabilities: SourceControlCapabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceControlNodeKind {
    File,
    Directory,
    SymbolicLink,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceControlContentStatus {
    Normal,
    Modified,
    Added,
    Deleted,
    Missing,
    Replaced,
    Conflicted,
    Obstructed,
    Unversioned,
    Ignored,
    External,
}

impl SourceControlContentStatus {
    pub fn is_changed(self) -> bool {
        !matches!(self, Self::Normal | Self::Ignored | Self::External)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceControlSelectionEligibility {
    IncludedByDefault,
    Available,
    RequiresAdd,
    RequiresDelete,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlStatusEntry {
    pub path: SourceControlPath,
    pub node_kind: SourceControlNodeKind,
    pub content_status: SourceControlContentStatus,
    pub property_status: SourceControlContentStatus,
    pub selection: SourceControlSelectionEligibility,
    pub copied: bool,
    pub switched: bool,
    pub locked: bool,
    pub tree_conflicted: bool,
    pub out_of_date: bool,
    pub added_lines: Option<u32>,
    pub deleted_lines: Option<u32>,
}

impl SourceControlStatusEntry {
    pub fn is_changed(&self) -> bool {
        self.content_status.is_changed() || self.property_status.is_changed()
    }

    pub fn is_conflicted(&self) -> bool {
        self.content_status == SourceControlContentStatus::Conflicted
            || self.property_status == SourceControlContentStatus::Conflicted
            || self.tree_conflicted
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlRevision {
    pub canonical: Arc<str>,
    pub display: Arc<str>,
}

impl SourceControlRevision {
    pub fn new(canonical: impl Into<Arc<str>>, display: impl Into<Arc<str>>) -> Self {
        Self {
            canonical: canonical.into(),
            display: display.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlHistoryEntry {
    pub revision: SourceControlRevision,
    pub author: Option<Arc<str>>,
    pub timestamp: Option<Arc<str>>,
    pub message: Arc<str>,
    pub changed_paths: Vec<SourceControlPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceControlBlameLine {
    pub line_number: u32,
    pub revision: Option<SourceControlRevision>,
    pub author: Option<Arc<str>>,
    pub timestamp: Option<Arc<str>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceControlOperation {
    Commit {
        paths: Vec<SourceControlPath>,
        message: String,
    },
    Add {
        paths: Vec<SourceControlPath>,
    },
    Delete {
        paths: Vec<SourceControlPath>,
    },
    Revert {
        paths: Vec<SourceControlPath>,
    },
    Resolve {
        paths: Vec<SourceControlPath>,
    },
    Ignore {
        directory: SourceControlPath,
        pattern: String,
    },
    Update,
    Checkout {
        url: String,
        destination: PathBuf,
        revision: Option<SourceControlRevision>,
    },
    Switch {
        url: String,
        revision: Option<SourceControlRevision>,
    },
}

impl SourceControlOperation {
    pub fn required_capability(&self) -> SourceControlCapability {
        match self {
            Self::Commit { .. } => SourceControlCapability::Commit,
            Self::Add { .. } => SourceControlCapability::Add,
            Self::Delete { .. } => SourceControlCapability::Delete,
            Self::Revert { .. } => SourceControlCapability::Revert,
            Self::Resolve { .. } => SourceControlCapability::Resolve,
            Self::Ignore { .. } => SourceControlCapability::Ignore,
            Self::Update => SourceControlCapability::Update,
            Self::Checkout { .. } => SourceControlCapability::Checkout,
            Self::Switch { .. } => SourceControlCapability::Switch,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceControlOperationOutput {
    pub message: Option<Arc<str>>,
    pub opened_path: Option<PathBuf>,
    pub refresh_required: bool,
}

#[derive(Clone, Default)]
pub struct SourceControlExecutionOptions {
    pub authentication: Option<SourceControlAuthentication>,
    pub trust: Option<SourceControlTrustDecision>,
}

impl fmt::Debug for SourceControlExecutionOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceControlExecutionOptions")
            .field("authentication", &self.authentication)
            .field("trust", &self.trust)
            .finish()
    }
}

#[derive(Clone)]
pub struct SourceControlAuthentication {
    pub username: Arc<str>,
    pub password: Zeroizing<String>,
}

impl fmt::Debug for SourceControlAuthentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceControlAuthentication")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceControlTrustDecision {
    pub failures: Vec<Arc<str>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct SourceControlError {
    pub kind: SourceControlErrorKind,
    pub message: Arc<str>,
}

impl SourceControlError {
    pub fn new(kind: SourceControlErrorKind, message: impl Into<Arc<str>>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<Arc<str>>) -> Self {
        Self::new(SourceControlErrorKind::InvalidRequest, message)
    }

    pub fn unsupported(capability: SourceControlCapability) -> Self {
        Self::new(
            SourceControlErrorKind::UnsupportedOperation { capability },
            format!("the source-control provider does not support {capability}"),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceControlErrorKind {
    ProviderUnavailable,
    RepositoryNotFound,
    UnsupportedOperation {
        capability: SourceControlCapability,
    },
    AuthenticationRequired {
        realm: Arc<str>,
        username: Option<Arc<str>>,
    },
    TrustRequired {
        realm: Arc<str>,
        fingerprint: Arc<str>,
        failures: Vec<Arc<str>>,
    },
    Cancelled,
    StaleState,
    CommandFailed {
        exit_code: Option<i32>,
    },
    MalformedResponse,
    TransportFailure,
    InvalidRequest,
}

#[async_trait]
pub trait SourceControlProvider: Send + Sync {
    fn id(&self) -> SourceControlProviderId;
    fn display_name(&self) -> Arc<str>;
    fn capabilities(&self) -> SourceControlCapabilities;

    async fn discover(
        &self,
        worktrees: &[SourceControlWorktree],
    ) -> Result<Vec<SourceControlRepository>, SourceControlError>;

    async fn status(
        &self,
        _repository: &SourceControlRepository,
    ) -> Result<Vec<SourceControlStatusEntry>, SourceControlError> {
        Err(SourceControlError::unsupported(
            SourceControlCapability::Status,
        ))
    }

    async fn load_text(
        &self,
        _repository: &SourceControlRepository,
        _path: &SourceControlPath,
        _revision: Option<&SourceControlRevision>,
    ) -> Result<Option<Vec<u8>>, SourceControlError> {
        Err(SourceControlError::unsupported(
            SourceControlCapability::BaseText,
        ))
    }

    async fn diff(
        &self,
        _repository: &SourceControlRepository,
        _path: &SourceControlPath,
    ) -> Result<Option<Arc<str>>, SourceControlError> {
        Err(SourceControlError::unsupported(
            SourceControlCapability::Diff,
        ))
    }

    async fn history(
        &self,
        _repository: &SourceControlRepository,
        _path: Option<&SourceControlPath>,
        _limit: usize,
    ) -> Result<Vec<SourceControlHistoryEntry>, SourceControlError> {
        Err(SourceControlError::unsupported(
            SourceControlCapability::History,
        ))
    }

    async fn blame(
        &self,
        _repository: &SourceControlRepository,
        _path: &SourceControlPath,
    ) -> Result<Vec<SourceControlBlameLine>, SourceControlError> {
        Err(SourceControlError::unsupported(
            SourceControlCapability::Blame,
        ))
    }

    async fn execute(
        &self,
        _repository: &SourceControlRepository,
        operation: SourceControlOperation,
    ) -> Result<SourceControlOperationOutput, SourceControlError> {
        Err(SourceControlError::unsupported(
            operation.required_capability(),
        ))
    }

    async fn execute_with_options(
        &self,
        repository: &SourceControlRepository,
        operation: SourceControlOperation,
        _options: SourceControlExecutionOptions,
    ) -> Result<SourceControlOperationOutput, SourceControlError> {
        self.execute(repository, operation).await
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceControlRefreshOutcome {
    pub repositories: Vec<SourceControlRepository>,
    pub errors: Vec<(SourceControlProviderId, SourceControlError)>,
}

#[derive(Default)]
pub struct SourceControlRegistry {
    providers: BTreeMap<SourceControlProviderId, Arc<dyn SourceControlProvider>>,
    repositories: BTreeMap<SourceControlRepositoryKey, SourceControlRepository>,
    active_repository: Option<SourceControlRepositoryKey>,
}

impl SourceControlRegistry {
    pub fn register_provider(
        &mut self,
        provider: Arc<dyn SourceControlProvider>,
    ) -> Result<(), SourceControlError> {
        let provider_id = provider.id();
        if self.providers.contains_key(&provider_id) {
            return Err(SourceControlError::invalid_request(format!(
                "source-control provider {provider_id} is already registered"
            )));
        }
        self.providers.insert(provider_id, provider);
        Ok(())
    }

    pub fn unregister_provider(&mut self, provider_id: &SourceControlProviderId) {
        self.providers.remove(provider_id);
        self.repositories
            .retain(|key, _| &key.provider_id != provider_id);
        if self
            .active_repository
            .as_ref()
            .is_some_and(|key| &key.provider_id == provider_id)
        {
            self.active_repository = self.default_active_repository();
        }
    }

    pub async fn refresh(
        &mut self,
        worktrees: &[SourceControlWorktree],
    ) -> SourceControlRefreshOutcome {
        let mut repositories = BTreeMap::new();
        let mut errors = Vec::new();

        for (provider_id, provider) in &self.providers {
            if !provider
                .capabilities()
                .supports(SourceControlCapability::Discovery)
            {
                continue;
            }
            match provider.discover(worktrees).await {
                Ok(discovered) => {
                    for mut repository in discovered {
                        repository.key.provider_id = provider_id.clone();
                        repository.capabilities = provider.capabilities();
                        repositories.insert(repository.key.clone(), repository);
                    }
                }
                Err(error) => errors.push((provider_id.clone(), error)),
            }
        }

        self.repositories = repositories;
        if self
            .active_repository
            .as_ref()
            .is_none_or(|key| !self.repositories.contains_key(key))
        {
            self.active_repository = self.default_active_repository();
        }

        SourceControlRefreshOutcome {
            repositories: self.repositories.values().cloned().collect(),
            errors,
        }
    }

    pub fn repositories(&self) -> impl Iterator<Item = &SourceControlRepository> {
        self.repositories.values()
    }

    pub fn active_repository(&self) -> Option<&SourceControlRepository> {
        self.active_repository
            .as_ref()
            .and_then(|key| self.repositories.get(key))
    }

    pub fn set_active_repository(
        &mut self,
        key: &SourceControlRepositoryKey,
    ) -> Result<(), SourceControlError> {
        if !self.repositories.contains_key(key) {
            return Err(SourceControlError::new(
                SourceControlErrorKind::RepositoryNotFound,
                "the selected source-control repository no longer exists",
            ));
        }
        self.active_repository = Some(key.clone());
        Ok(())
    }

    pub async fn status(
        &self,
        key: &SourceControlRepositoryKey,
    ) -> Result<Vec<SourceControlStatusEntry>, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, SourceControlCapability::Status)?;
        provider.status(repository).await
    }

    pub async fn load_text(
        &self,
        key: &SourceControlRepositoryKey,
        path: &SourceControlPath,
        revision: Option<&SourceControlRevision>,
    ) -> Result<Option<Vec<u8>>, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, SourceControlCapability::BaseText)?;
        provider.load_text(repository, path, revision).await
    }

    pub async fn history(
        &self,
        key: &SourceControlRepositoryKey,
        path: Option<&SourceControlPath>,
        limit: usize,
    ) -> Result<Vec<SourceControlHistoryEntry>, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, SourceControlCapability::History)?;
        provider.history(repository, path, limit).await
    }

    pub async fn diff(
        &self,
        key: &SourceControlRepositoryKey,
        path: &SourceControlPath,
    ) -> Result<Option<Arc<str>>, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, SourceControlCapability::Diff)?;
        provider.diff(repository, path).await
    }

    pub async fn blame(
        &self,
        key: &SourceControlRepositoryKey,
        path: &SourceControlPath,
    ) -> Result<Vec<SourceControlBlameLine>, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, SourceControlCapability::Blame)?;
        provider.blame(repository, path).await
    }

    pub async fn execute(
        &self,
        key: &SourceControlRepositoryKey,
        operation: SourceControlOperation,
    ) -> Result<SourceControlOperationOutput, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, operation.required_capability())?;
        provider.execute(repository, operation).await
    }

    pub async fn execute_with_options(
        &self,
        key: &SourceControlRepositoryKey,
        operation: SourceControlOperation,
        options: SourceControlExecutionOptions,
    ) -> Result<SourceControlOperationOutput, SourceControlError> {
        let (provider, repository) = self.provider_and_repository(key)?;
        self.ensure_capability(repository, operation.required_capability())?;
        provider
            .execute_with_options(repository, operation, options)
            .await
    }

    fn provider_and_repository(
        &self,
        key: &SourceControlRepositoryKey,
    ) -> Result<(&Arc<dyn SourceControlProvider>, &SourceControlRepository), SourceControlError>
    {
        let repository = self.repositories.get(key).ok_or_else(|| {
            SourceControlError::new(
                SourceControlErrorKind::RepositoryNotFound,
                "the source-control repository no longer exists",
            )
        })?;
        let provider = self.providers.get(&key.provider_id).ok_or_else(|| {
            SourceControlError::new(
                SourceControlErrorKind::ProviderUnavailable,
                "the source-control provider is no longer available",
            )
        })?;
        Ok((provider, repository))
    }

    fn ensure_capability(
        &self,
        repository: &SourceControlRepository,
        capability: SourceControlCapability,
    ) -> Result<(), SourceControlError> {
        if repository.capabilities.supports(capability) {
            Ok(())
        } else {
            Err(SourceControlError::unsupported(capability))
        }
    }

    fn default_active_repository(&self) -> Option<SourceControlRepositoryKey> {
        self.repositories
            .keys()
            .find(|key| key.provider_id == SourceControlProviderId::git())
            .or_else(|| self.repositories.keys().next())
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;

    struct FakeProvider {
        id: SourceControlProviderId,
        repositories: Vec<SourceControlRepository>,
        capabilities: SourceControlCapabilities,
    }

    impl FakeProvider {
        fn new(id: SourceControlProviderId, root: &Path) -> Self {
            let repository_id = SourceControlRepositoryId::new("repository").expect("valid ID");
            Self {
                repositories: vec![SourceControlRepository {
                    key: SourceControlRepositoryKey::new(id.clone(), repository_id),
                    worktree_ids: vec![1],
                    working_copy_path: Arc::from(root),
                    display_name: Arc::from("Repository"),
                    summary: None,
                    capabilities: SourceControlCapabilities::NONE,
                }],
                id,
                capabilities: SourceControlCapabilities {
                    discovery: true,
                    status: true,
                    ..SourceControlCapabilities::NONE
                },
            }
        }
    }

    #[async_trait]
    impl SourceControlProvider for FakeProvider {
        fn id(&self) -> SourceControlProviderId {
            self.id.clone()
        }

        fn display_name(&self) -> Arc<str> {
            Arc::from("Fake")
        }

        fn capabilities(&self) -> SourceControlCapabilities {
            self.capabilities
        }

        async fn discover(
            &self,
            _worktrees: &[SourceControlWorktree],
        ) -> Result<Vec<SourceControlRepository>, SourceControlError> {
            Ok(self.repositories.clone())
        }

        async fn status(
            &self,
            _repository: &SourceControlRepository,
        ) -> Result<Vec<SourceControlStatusEntry>, SourceControlError> {
            Ok(Vec::new())
        }
    }

    fn key(provider_id: SourceControlProviderId) -> SourceControlRepositoryKey {
        SourceControlRepositoryKey::new(
            provider_id,
            SourceControlRepositoryId::new("repository").expect("valid ID"),
        )
    }

    #[test]
    fn overlapping_providers_are_preserved_and_git_is_preferred() {
        block_on(async {
            let root = Path::new("/project");
            let mut registry = SourceControlRegistry::default();
            registry
                .register_provider(Arc::new(FakeProvider::new(
                    SourceControlProviderId::subversion(),
                    root,
                )))
                .expect("register SVN provider");
            registry
                .register_provider(Arc::new(FakeProvider::new(
                    SourceControlProviderId::git(),
                    root,
                )))
                .expect("register Git provider");

            let outcome = registry.refresh(&[]).await;

            assert_eq!(outcome.repositories.len(), 2);
            assert_eq!(
                registry
                    .active_repository()
                    .map(|repository| repository.key.provider_id.clone()),
                Some(SourceControlProviderId::git())
            );

            registry
                .set_active_repository(&key(SourceControlProviderId::subversion()))
                .expect("activate SVN repository");
            assert_eq!(
                registry
                    .active_repository()
                    .map(|repository| repository.key.provider_id.clone()),
                Some(SourceControlProviderId::subversion())
            );
        });
    }

    #[test]
    fn operations_are_rejected_before_dispatch_when_capability_is_absent() {
        block_on(async {
            let mut registry = SourceControlRegistry::default();
            registry
                .register_provider(Arc::new(FakeProvider::new(
                    SourceControlProviderId::subversion(),
                    Path::new("/project"),
                )))
                .expect("register provider");
            registry.refresh(&[]).await;

            let error = registry
                .execute(
                    &key(SourceControlProviderId::subversion()),
                    SourceControlOperation::Update,
                )
                .await
                .expect_err("update should be unavailable");
            assert_eq!(
                error.kind,
                SourceControlErrorKind::UnsupportedOperation {
                    capability: SourceControlCapability::Update
                }
            );
        });
    }

    #[test]
    fn repository_paths_reject_escape_and_platform_separators() {
        assert!(SourceControlPath::new("src/main.rs").is_ok());
        assert!(SourceControlPath::new("../secret").is_err());
        assert!(SourceControlPath::new("/absolute").is_err());
        assert!(SourceControlPath::new("src\\main.rs").is_err());
        assert!(SourceControlPath::new("src//main.rs").is_err());
    }

    #[test]
    fn duplicate_provider_registration_is_rejected() {
        let root = Path::new("/project");
        let mut registry = SourceControlRegistry::default();
        registry
            .register_provider(Arc::new(FakeProvider::new(
                SourceControlProviderId::git(),
                root,
            )))
            .expect("register provider");

        let error = registry
            .register_provider(Arc::new(FakeProvider::new(
                SourceControlProviderId::git(),
                root,
            )))
            .expect_err("duplicate registration should fail");
        assert_eq!(error.kind, SourceControlErrorKind::InvalidRequest);
    }

    #[test]
    fn authentication_debug_output_redacts_passwords() {
        let options = SourceControlExecutionOptions {
            authentication: Some(SourceControlAuthentication {
                username: Arc::from("alice"),
                password: "top-secret".to_owned().into(),
            }),
            trust: None,
        };

        let debug_output = format!("{options:?}");
        assert!(debug_output.contains("alice"));
        assert!(debug_output.contains("[REDACTED]"));
        assert!(!debug_output.contains("top-secret"));
    }
}
