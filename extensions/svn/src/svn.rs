mod extension_adapter;
mod xml;

pub use xml::{SvnInfo, parse_blame, parse_info, parse_log, parse_status};

use async_trait::async_trait;
use semver::Version;
use source_control::{
    SourceControlBlameLine, SourceControlCapabilities, SourceControlContentStatus,
    SourceControlError, SourceControlErrorKind, SourceControlExecutionOptions,
    SourceControlHistoryEntry, SourceControlOperation, SourceControlOperationOutput,
    SourceControlPath, SourceControlProvider, SourceControlProviderId, SourceControlRepository,
    SourceControlRepositoryId, SourceControlRepositoryKey, SourceControlRevision,
    SourceControlSelectionEligibility, SourceControlStatusEntry, SourceControlWorktree,
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};
use thiserror::Error;
use zeroize::Zeroize;

const MINIMUM_SVN_VERSION: Version = Version::new(1, 14, 0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SvnCommand {
    pub working_directory: Arc<Path>,
    pub arguments: Vec<String>,
    pub stdin: Option<Vec<u8>>,
    pub redact_stdin: bool,
}

impl SvnCommand {
    pub fn new(
        working_directory: impl Into<Arc<Path>>,
        arguments: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            working_directory: working_directory.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
            stdin: None,
            redact_stdin: false,
        }
    }
}

impl Drop for SvnCommand {
    fn drop(&mut self) {
        if self.redact_stdin
            && let Some(stdin) = &mut self.stdin
        {
            stdin.zeroize();
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SvnCommandOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait SvnTemporaryFile: Send {
    fn path(&self) -> &Path;
}

pub trait SvnCommandRunner: Send + Sync {
    fn run(&self, command: &SvnCommand) -> Result<SvnCommandOutput, SvnError>;

    fn write_temporary_file(
        &self,
        contents: &[u8],
        sensitive: bool,
    ) -> Result<Box<dyn SvnTemporaryFile>, SvnError>;
}

#[derive(Debug, Error)]
pub enum SvnError {
    #[error("Subversion is not installed or is not available on PATH")]
    ClientUnavailable,
    #[error(
        "Subversion {found} is unsupported; version 1.14 LTS or newer in the 1.x series is required"
    )]
    UnsupportedVersion { found: Arc<str> },
    #[error("Subversion command failed: {message}")]
    CommandFailed {
        exit_code: Option<i32>,
        message: Arc<str>,
    },
    #[error("Subversion returned malformed data: {0}")]
    MalformedResponse(Arc<str>),
    #[error("invalid Subversion request: {0}")]
    InvalidRequest(Arc<str>),
    #[error("Subversion working-copy state changed: {0}")]
    StaleState(Arc<str>),
    #[error("Subversion command runner failed: {0}")]
    Runner(Arc<str>),
}

impl SvnError {
    fn malformed_xml(error: impl std::fmt::Display) -> Self {
        Self::MalformedResponse(Arc::from(error.to_string()))
    }
}

pub struct SvnProvider<'a> {
    runner: Arc<dyn SvnCommandRunner + 'a>,
    repositories: Mutex<BTreeMap<SourceControlRepositoryKey, SvnRepositoryState>>,
}

#[derive(Clone)]
struct SvnRepositoryState {
    info: SvnInfo,
    execution_directory: Arc<Path>,
}

impl<'a> SvnProvider<'a> {
    pub fn new(runner: Arc<dyn SvnCommandRunner + 'a>) -> Self {
        Self {
            runner,
            repositories: Mutex::new(BTreeMap::default()),
        }
    }

    fn repository_state(
        &self,
        repository: &SourceControlRepository,
    ) -> Result<SvnRepositoryState, SourceControlError> {
        if repository.key.provider_id != SourceControlProviderId::subversion() {
            return Err(SourceControlError::new(
                SourceControlErrorKind::RepositoryNotFound,
                "the repository does not belong to the Subversion provider",
            ));
        }
        self.lock_repositories()
            .map_err(source_control_error)?
            .get(&repository.key)
            .cloned()
            .ok_or_else(|| {
                SourceControlError::new(
                    SourceControlErrorKind::RepositoryNotFound,
                    "the Subversion working copy is no longer available",
                )
            })
    }

    fn lock_repositories(
        &self,
    ) -> Result<MutexGuard<'_, BTreeMap<SourceControlRepositoryKey, SvnRepositoryState>>, SvnError>
    {
        self.repositories
            .lock()
            .map_err(|_| SvnError::Runner("Subversion repository state is unavailable".into()))
    }

    fn ensure_supported_version(&self, working_directory: &Path) -> Result<Version, SvnError> {
        let command = SvnCommand::new(working_directory, ["--version", "--quiet"]);
        let output = self.runner.run(&command).map_err(|error| match error {
            SvnError::Runner(_) | SvnError::ClientUnavailable => SvnError::ClientUnavailable,
            other => other,
        })?;
        let output = checked_output(output)?;
        let version_text = std::str::from_utf8(&output.stdout)
            .map_err(SvnError::malformed_xml)?
            .trim();
        let version = Version::parse(version_text).map_err(|_| SvnError::UnsupportedVersion {
            found: Arc::from(version_text),
        })?;
        if version.major != 1 || version < MINIMUM_SVN_VERSION {
            return Err(SvnError::UnsupportedVersion {
                found: Arc::from(version.to_string()),
            });
        }
        Ok(version)
    }

    fn run_checked(&self, command: &SvnCommand) -> Result<SvnCommandOutput, SvnError> {
        checked_output(self.runner.run(command)?)
    }

    fn run_checked_with_options(
        &self,
        command: SvnCommand,
        options: &SourceControlExecutionOptions,
    ) -> Result<SvnCommandOutput, SvnError> {
        let command = command_with_execution_options(command, options)?;
        self.run_checked(&command)
    }

    fn run_path_operation(
        &self,
        state: &SvnRepositoryState,
        operation_arguments: impl IntoIterator<Item = impl Into<String>>,
        paths: Vec<SourceControlPath>,
    ) -> Result<SourceControlOperationOutput, SvnError> {
        if paths.is_empty() {
            return Err(SvnError::InvalidRequest(
                "at least one working-copy path is required".into(),
            ));
        }
        let mut arguments = operation_arguments
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        arguments.push("--".to_owned());
        arguments.extend(
            paths
                .iter()
                .map(|path| working_copy_target(&state.info, path))
                .collect::<Result<Vec<_>, _>>()?,
        );
        self.run_checked(&SvnCommand::new(
            state.execution_directory.clone(),
            arguments,
        ))?;
        Ok(refresh_output())
    }

    fn commit(
        &self,
        state: &SvnRepositoryState,
        paths: Vec<SourceControlPath>,
        message: String,
        options: &SourceControlExecutionOptions,
    ) -> Result<SourceControlOperationOutput, SvnError> {
        if paths.is_empty() {
            return Err(SvnError::InvalidRequest(
                "at least one path must be selected for commit".into(),
            ));
        }
        let target_contents = paths
            .iter()
            .map(|path| working_copy_target(&state.info, path))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        let targets_file = self
            .runner
            .write_temporary_file(target_contents.as_bytes(), false)?;
        let message_file = self.runner.write_temporary_file(message.as_bytes(), true)?;
        let targets_path = utf8_path(targets_file.path())?;
        let message_path = utf8_path(message_file.path())?;
        self.run_checked_with_options(
            SvnCommand::new(
                state.execution_directory.clone(),
                [
                    "commit".to_owned(),
                    "--non-interactive".to_owned(),
                    "--encoding".to_owned(),
                    "UTF-8".to_owned(),
                    "--targets".to_owned(),
                    targets_path,
                    "--file".to_owned(),
                    message_path,
                ],
            ),
            options,
        )?;
        Ok(refresh_output())
    }

    fn set_ignore(
        &self,
        state: &SvnRepositoryState,
        directory: SourceControlPath,
        pattern: String,
    ) -> Result<SourceControlOperationOutput, SvnError> {
        if pattern.is_empty() || pattern.contains(['\n', '\r']) {
            return Err(SvnError::InvalidRequest(
                "an ignore pattern must be one non-empty line".into(),
            ));
        }
        let directory = if directory.as_str().is_empty() {
            working_copy_root_target(&state.info)?
        } else {
            working_copy_target(&state.info, &directory)?
        };
        let get_command = SvnCommand::new(
            state.execution_directory.clone(),
            [
                "propget".to_owned(),
                "svn:ignore".to_owned(),
                "--strict".to_owned(),
                "--non-interactive".to_owned(),
                "--".to_owned(),
                directory.clone(),
            ],
        );
        let current_output = self.runner.run(&get_command)?;
        let current = if current_output.status == Some(0) {
            String::from_utf8(current_output.stdout).map_err(|_| {
                SvnError::MalformedResponse("svn:ignore contains non-UTF-8 data".into())
            })?
        } else if output_contains_code(&current_output, "E200017")
            || output_contains_code(&current_output, "W200017")
        {
            String::new()
        } else {
            return Err(command_failed(current_output));
        };
        if current.lines().any(|line| line == pattern) {
            return Ok(SourceControlOperationOutput {
                message: Some("The path is already ignored".into()),
                ..SourceControlOperationOutput::default()
            });
        }

        let mut updated = current.trim_end_matches(['\n', '\r']).to_owned();
        if !updated.is_empty() {
            updated.push('\n');
        }
        updated.push_str(&pattern);
        updated.push('\n');
        let property_file = self
            .runner
            .write_temporary_file(updated.as_bytes(), false)?;
        let property_path = utf8_path(property_file.path())?;
        self.run_checked(&SvnCommand::new(
            state.execution_directory.clone(),
            [
                "propset".to_owned(),
                "svn:ignore".to_owned(),
                "--file".to_owned(),
                property_path,
                "--non-interactive".to_owned(),
                "--".to_owned(),
                directory,
            ],
        ))?;
        Ok(refresh_output())
    }

    fn populate_node_kinds(
        &self,
        state: &SvnRepositoryState,
        statuses: &mut [SourceControlStatusEntry],
    ) -> Result<(), SvnError> {
        const TARGETS_PER_COMMAND: usize = 64;

        let tracked_paths = statuses
            .iter()
            .filter(|status| {
                !matches!(
                    status.content_status,
                    SourceControlContentStatus::Unversioned
                        | SourceControlContentStatus::Ignored
                        | SourceControlContentStatus::External
                )
            })
            .map(|status| status.path.clone())
            .collect::<Vec<_>>();

        let mut kinds = BTreeMap::new();
        for paths in tracked_paths.chunks(TARGETS_PER_COMMAND) {
            let mut arguments = vec![
                "info".to_owned(),
                "--xml".to_owned(),
                "--non-interactive".to_owned(),
                "--".to_owned(),
            ];
            arguments.extend(
                paths
                    .iter()
                    .map(|path| working_copy_target(&state.info, path))
                    .collect::<Result<Vec<_>, _>>()?,
            );
            let output = self.runner.run(&SvnCommand::new(
                state.execution_directory.clone(),
                arguments,
            ))?;
            if output.stdout.is_empty() {
                continue;
            }
            for info in parse_info_output(&output)? {
                if let Ok(path) =
                    normalize_working_copy_path(&info.path, &state.info, &state.execution_directory)
                {
                    kinds.insert(path, info.node_kind);
                }
            }
        }

        for status in statuses {
            if let Some(kind) = kinds.get(&status.path) {
                status.node_kind = *kind;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SourceControlProvider for SvnProvider<'_> {
    fn id(&self) -> SourceControlProviderId {
        SourceControlProviderId::subversion()
    }

    fn display_name(&self) -> Arc<str> {
        Arc::from("Subversion")
    }

    fn capabilities(&self) -> SourceControlCapabilities {
        SourceControlCapabilities::SUBVERSION
    }

    async fn discover(
        &self,
        worktrees: &[SourceControlWorktree],
    ) -> Result<Vec<SourceControlRepository>, SourceControlError> {
        let Some(first_worktree) = worktrees.first() else {
            return Ok(Vec::new());
        };
        self.ensure_supported_version(&first_worktree.root_path)
            .map_err(source_control_error)?;

        let mut repositories = BTreeMap::<
            SourceControlRepositoryKey,
            (SourceControlRepository, SvnRepositoryState),
        >::new();
        for worktree in worktrees {
            let command = SvnCommand::new(
                worktree.root_path.clone(),
                ["info", "--xml", "--non-interactive", "--", "."],
            );
            let output = self.runner.run(&command).map_err(source_control_error)?;
            if output.status != Some(0) {
                if is_not_working_copy(&output) {
                    continue;
                }
                return Err(source_control_error(command_failed(output)));
            }
            for info in parse_info_output(&output).map_err(source_control_error)? {
                let repository_id = SourceControlRepositoryId::new(format!(
                    "{}:{}",
                    info.repository_uuid,
                    info.working_copy_root.to_string_lossy()
                ))?;
                let key = SourceControlRepositoryKey::new(self.id(), repository_id);
                if let Some((repository, _)) = repositories.get_mut(&key) {
                    if !repository.worktree_ids.contains(&worktree.id) {
                        repository.worktree_ids.push(worktree.id);
                        repository.worktree_ids.sort_unstable();
                    }
                    continue;
                }
                let display_name = repository_display_name(&info.working_copy_root);
                let revision = info
                    .revision
                    .map(|revision| format!("r{revision}"))
                    .unwrap_or_else(|| "uncommitted".to_owned());
                let summary = Arc::<str>::from(format!("{revision} · {}", info.relative_url));
                repositories.insert(
                    key.clone(),
                    (
                        SourceControlRepository {
                            key,
                            worktree_ids: vec![worktree.id],
                            working_copy_path: info.working_copy_root.clone(),
                            display_name,
                            summary: Some(summary),
                            capabilities: self.capabilities(),
                        },
                        SvnRepositoryState {
                            info,
                            execution_directory: worktree.root_path.clone(),
                        },
                    ),
                );
            }
        }

        let mut metadata = self.lock_repositories().map_err(source_control_error)?;
        metadata.clear();
        for (key, (_, state)) in &repositories {
            metadata.insert(key.clone(), state.clone());
        }
        Ok(repositories
            .into_values()
            .map(|(repository, _)| repository)
            .collect())
    }

    async fn status(
        &self,
        repository: &SourceControlRepository,
    ) -> Result<Vec<SourceControlStatusEntry>, SourceControlError> {
        let state = self.repository_state(repository)?;
        let target = working_copy_root_target(&state.info).map_err(source_control_error)?;
        let output = self
            .run_checked(&SvnCommand::new(
                state.execution_directory.clone(),
                ["status", "--xml", "--ignore-externals", "--", &target],
            ))
            .map_err(source_control_error)?;
        let xml = output_utf8(&output).map_err(source_control_error)?;
        let mut statuses =
            parse_status(xml, &state.info.working_copy_root).map_err(source_control_error)?;
        self.populate_node_kinds(&state, &mut statuses)
            .map_err(source_control_error)?;
        Ok(statuses)
    }

    async fn load_text(
        &self,
        repository: &SourceControlRepository,
        path: &SourceControlPath,
        revision: Option<&SourceControlRevision>,
    ) -> Result<Option<Vec<u8>>, SourceControlError> {
        let state = self.repository_state(repository)?;
        let revision = revision
            .map(|revision| revision.canonical.as_ref())
            .unwrap_or("BASE");
        let target = working_copy_target(&state.info, path).map_err(source_control_error)?;
        let command = SvnCommand::new(
            state.execution_directory,
            [
                "cat".to_owned(),
                "--non-interactive".to_owned(),
                "-r".to_owned(),
                revision.to_owned(),
                "--".to_owned(),
                target,
            ],
        );
        let output = self.runner.run(&command).map_err(source_control_error)?;
        if output.status == Some(0) {
            Ok(Some(output.stdout))
        } else if output_contains_code(&output, "E200009")
            || output_contains_code(&output, "E155010")
            || output_contains_code(&output, "W160017")
        {
            Ok(None)
        } else {
            Err(source_control_error(command_failed(output)))
        }
    }

    async fn diff(
        &self,
        repository: &SourceControlRepository,
        path: &SourceControlPath,
    ) -> Result<Option<Arc<str>>, SourceControlError> {
        let state = self.repository_state(repository)?;
        let target = working_copy_target(&state.info, path).map_err(source_control_error)?;
        let output = self
            .run_checked(&SvnCommand::new(
                state.execution_directory,
                ["diff", "--non-interactive", "--", &target],
            ))
            .map_err(source_control_error)?;
        let diff = output_utf8(&output).map_err(source_control_error)?;
        if diff.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Arc::from(diff)))
        }
    }

    async fn history(
        &self,
        repository: &SourceControlRepository,
        path: Option<&SourceControlPath>,
        limit: usize,
    ) -> Result<Vec<SourceControlHistoryEntry>, SourceControlError> {
        let state = self.repository_state(repository)?;
        let target = path
            .map(|path| working_copy_target(&state.info, path))
            .transpose()
            .map_err(source_control_error)?;
        let mut arguments = vec![
            "log".to_owned(),
            "--xml".to_owned(),
            "--verbose".to_owned(),
            "--non-interactive".to_owned(),
            "--limit".to_owned(),
            limit.clamp(1, 1000).to_string(),
            "--".to_owned(),
        ];
        arguments.push(match target {
            Some(target) => target,
            None => working_copy_root_target(&state.info).map_err(source_control_error)?,
        });
        let output = self
            .run_checked(&SvnCommand::new(state.execution_directory, arguments))
            .map_err(source_control_error)?;
        parse_log(output_utf8(&output).map_err(source_control_error)?).map_err(source_control_error)
    }

    async fn blame(
        &self,
        repository: &SourceControlRepository,
        path: &SourceControlPath,
    ) -> Result<Vec<SourceControlBlameLine>, SourceControlError> {
        let state = self.repository_state(repository)?;
        let target = working_copy_target(&state.info, path).map_err(source_control_error)?;
        let output = self
            .run_checked(&SvnCommand::new(
                state.execution_directory,
                ["blame", "--xml", "--non-interactive", "--", &target],
            ))
            .map_err(source_control_error)?;
        parse_blame(output_utf8(&output).map_err(source_control_error)?)
            .map_err(source_control_error)
    }

    async fn execute(
        &self,
        repository: &SourceControlRepository,
        operation: SourceControlOperation,
    ) -> Result<SourceControlOperationOutput, SourceControlError> {
        self.execute_with_options(
            repository,
            operation,
            SourceControlExecutionOptions::default(),
        )
        .await
    }

    async fn execute_with_options(
        &self,
        repository: &SourceControlRepository,
        operation: SourceControlOperation,
        options: SourceControlExecutionOptions,
    ) -> Result<SourceControlOperationOutput, SourceControlError> {
        let state = self.repository_state(repository)?;
        if operation_requires_fresh_status(&operation) {
            let statuses = self.status(repository).await?;
            validate_operation_state(&operation, &statuses).map_err(source_control_error)?;
        }
        let result = (|| -> Result<SourceControlOperationOutput, SvnError> {
            match operation {
                SourceControlOperation::Commit { paths, message } => {
                    self.commit(&state, paths, message, &options)
                }
                SourceControlOperation::Add { paths } => self.run_path_operation(
                    &state,
                    ["add", "--parents", "--non-interactive"],
                    paths,
                ),
                SourceControlOperation::Delete { paths } => self.run_path_operation(
                    &state,
                    ["delete", "--force", "--non-interactive"],
                    paths,
                ),
                SourceControlOperation::Revert { paths } => self.run_path_operation(
                    &state,
                    ["revert", "--depth", "empty", "--non-interactive"],
                    paths,
                ),
                SourceControlOperation::Resolve { paths } => self.run_path_operation(
                    &state,
                    ["resolve", "--accept", "working", "--non-interactive"],
                    paths,
                ),
                SourceControlOperation::Ignore { directory, pattern } => {
                    self.set_ignore(&state, directory, pattern)
                }
                SourceControlOperation::Update => {
                    let target = working_copy_root_target(&state.info)?;
                    self.run_checked_with_options(
                        SvnCommand::new(
                            state.execution_directory.clone(),
                            [
                                "update",
                                "--accept",
                                "postpone",
                                "--ignore-externals",
                                "--non-interactive",
                                "--",
                                &target,
                            ],
                        ),
                        &options,
                    )?;
                    Ok(refresh_output())
                }
                SourceControlOperation::Checkout {
                    url,
                    destination,
                    revision,
                } => {
                    validate_repository_url(&url)?;
                    let destination_argument = utf8_path(&destination)?;
                    let mut arguments = vec![
                        "checkout".to_owned(),
                        "--ignore-externals".to_owned(),
                        "--non-interactive".to_owned(),
                    ];
                    if let Some(revision) = revision {
                        arguments.extend(["-r".to_owned(), revision.canonical.to_string()]);
                    }
                    arguments.extend(["--".to_owned(), protect_peg(&url), destination_argument]);
                    self.run_checked_with_options(
                        SvnCommand::new(state.execution_directory.clone(), arguments),
                        &options,
                    )?;
                    Ok(SourceControlOperationOutput {
                        opened_path: Some(destination),
                        refresh_required: true,
                        ..SourceControlOperationOutput::default()
                    })
                }
                SourceControlOperation::Switch { url, revision } => {
                    validate_repository_url(&url)?;
                    let mut arguments = vec![
                        "switch".to_owned(),
                        "--accept".to_owned(),
                        "postpone".to_owned(),
                        "--ignore-externals".to_owned(),
                        "--non-interactive".to_owned(),
                    ];
                    if let Some(revision) = revision {
                        arguments.extend(["-r".to_owned(), revision.canonical.to_string()]);
                    }
                    arguments.extend([
                        "--".to_owned(),
                        protect_peg(&url),
                        working_copy_root_target(&state.info)?,
                    ]);
                    self.run_checked_with_options(
                        SvnCommand::new(state.execution_directory, arguments),
                        &options,
                    )?;
                    Ok(refresh_output())
                }
            }
        })();
        result.map_err(source_control_error)
    }
}

fn operation_requires_fresh_status(operation: &SourceControlOperation) -> bool {
    matches!(
        operation,
        SourceControlOperation::Commit { .. }
            | SourceControlOperation::Add { .. }
            | SourceControlOperation::Delete { .. }
            | SourceControlOperation::Revert { .. }
            | SourceControlOperation::Resolve { .. }
    )
}

fn validate_operation_state(
    operation: &SourceControlOperation,
    statuses: &[SourceControlStatusEntry],
) -> Result<(), SvnError> {
    let (paths, expected_state, is_valid): (
        &[SourceControlPath],
        &str,
        fn(&SourceControlStatusEntry) -> bool,
    ) = match operation {
        SourceControlOperation::Commit { paths, message } => {
            if message.trim().is_empty() {
                return Err(SvnError::InvalidRequest(
                    "a Subversion commit message must not be empty".into(),
                ));
            }
            (paths, "a committable tracked change", |status| {
                matches!(
                    status.selection,
                    SourceControlSelectionEligibility::IncludedByDefault
                        | SourceControlSelectionEligibility::Available
                ) && !status.is_conflicted()
            })
        }
        SourceControlOperation::Add { paths } => (paths, "an unversioned path", |status| {
            status.selection == SourceControlSelectionEligibility::RequiresAdd
        }),
        SourceControlOperation::Delete { paths } => (paths, "a missing versioned path", |status| {
            status.selection == SourceControlSelectionEligibility::RequiresDelete
        }),
        SourceControlOperation::Revert { paths } => (paths, "a changed versioned path", |status| {
            status.is_changed()
                && !matches!(
                    status.content_status,
                    SourceControlContentStatus::Unversioned
                        | SourceControlContentStatus::Ignored
                        | SourceControlContentStatus::External
                )
        }),
        SourceControlOperation::Resolve { paths } => {
            (paths, "a conflicted path", |status| status.is_conflicted())
        }
        SourceControlOperation::Ignore { .. }
        | SourceControlOperation::Update
        | SourceControlOperation::Checkout { .. }
        | SourceControlOperation::Switch { .. } => return Ok(()),
    };

    if paths.is_empty() {
        return Err(SvnError::InvalidRequest(
            "at least one working-copy path is required".into(),
        ));
    }
    for path in paths {
        let Some(status) = statuses.iter().find(|status| status.path == *path) else {
            return Err(stale_path_error(path, expected_state));
        };
        if !is_valid(status) {
            return Err(stale_path_error(path, expected_state));
        }
    }
    Ok(())
}

fn stale_path_error(path: &SourceControlPath, expected_state: &str) -> SvnError {
    SvnError::StaleState(Arc::from(format!(
        "{} is no longer {}; refresh Source Control and try again",
        path.as_str(),
        expected_state,
    )))
}

fn command_with_execution_options(
    mut command: SvnCommand,
    options: &SourceControlExecutionOptions,
) -> Result<SvnCommand, SvnError> {
    let mut arguments = Vec::new();
    if let Some(authentication) = &options.authentication {
        if authentication.username.is_empty()
            || authentication.username.contains(['\n', '\r', '\0'])
            || authentication.password.contains(['\n', '\r', '\0'])
        {
            return Err(SvnError::InvalidRequest(
                "Subversion credentials contain unsupported characters".into(),
            ));
        }
        arguments.extend([
            "--username".to_owned(),
            authentication.username.to_string(),
            "--password-from-stdin".to_owned(),
            "--no-auth-cache".to_owned(),
        ]);
        let mut stdin = authentication.password.as_bytes().to_vec();
        stdin.push(b'\n');
        command.stdin = Some(stdin);
        command.redact_stdin = true;
    }

    if let Some(trust) = &options.trust {
        if trust.failures.is_empty()
            || trust.failures.iter().any(|failure| {
                !matches!(
                    failure.as_ref(),
                    "unknown-ca" | "cn-mismatch" | "expired" | "not-yet-valid" | "other"
                )
            })
        {
            return Err(SvnError::InvalidRequest(
                "Subversion certificate trust decision contains an unknown failure".into(),
            ));
        }
        arguments.extend([
            "--trust-server-cert-failures".to_owned(),
            trust
                .failures
                .iter()
                .map(AsRef::<str>::as_ref)
                .collect::<Vec<_>>()
                .join(","),
        ]);
        if !arguments
            .iter()
            .any(|argument| argument == "--no-auth-cache")
        {
            arguments.push("--no-auth-cache".to_owned());
        }
    }

    let insertion_index = command
        .arguments
        .iter()
        .position(|argument| argument == "--")
        .unwrap_or(command.arguments.len());
    command
        .arguments
        .splice(insertion_index..insertion_index, arguments);
    Ok(command)
}

fn checked_output(output: SvnCommandOutput) -> Result<SvnCommandOutput, SvnError> {
    if output.status == Some(0) {
        Ok(output)
    } else {
        Err(command_failed(output))
    }
}

fn command_failed(output: SvnCommandOutput) -> SvnError {
    let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    SvnError::CommandFailed {
        exit_code: output.status,
        message: if message.is_empty() {
            Arc::from("the svn process exited unsuccessfully")
        } else {
            Arc::from(message)
        },
    }
}

fn parse_info_output(output: &SvnCommandOutput) -> Result<Vec<SvnInfo>, SvnError> {
    parse_info(output_utf8(output)?)
}

fn output_utf8(output: &SvnCommandOutput) -> Result<&str, SvnError> {
    std::str::from_utf8(&output.stdout)
        .map_err(|_| SvnError::MalformedResponse("svn returned non-UTF-8 structured output".into()))
}

fn output_contains_code(output: &SvnCommandOutput, code: &str) -> bool {
    String::from_utf8_lossy(&output.stderr).contains(code)
}

fn is_not_working_copy(output: &SvnCommandOutput) -> bool {
    output_contains_code(output, "E155007") || output_contains_code(output, "W155007")
}

fn source_control_error(error: SvnError) -> SourceControlError {
    match error {
        SvnError::ClientUnavailable | SvnError::UnsupportedVersion { .. } => {
            SourceControlError::new(
                SourceControlErrorKind::ProviderUnavailable,
                error.to_string(),
            )
        }
        SvnError::CommandFailed { exit_code, message } => {
            let message_text = message.as_ref();
            if message_text.contains("E170001")
                || message_text
                    .to_ascii_lowercase()
                    .contains("authentication failed")
            {
                SourceControlError::new(
                    SourceControlErrorKind::AuthenticationRequired {
                        realm: extract_field(message_text, "Authentication realm:")
                            .unwrap_or_else(|| Arc::from("Subversion repository")),
                        username: None,
                    },
                    "Subversion authentication is required",
                )
            } else if message_text.contains("E230001")
                || message_text.contains("Server SSL certificate verification failed")
            {
                SourceControlError::new(
                    SourceControlErrorKind::TrustRequired {
                        realm: Arc::from("Subversion TLS server"),
                        fingerprint: extract_field(message_text, "Fingerprint:")
                            .unwrap_or_else(|| Arc::from("unknown")),
                        failures: certificate_failures(message_text),
                    },
                    "The Subversion server certificate requires approval",
                )
            } else if message_text.to_ascii_lowercase().contains("cancelled") {
                SourceControlError::new(SourceControlErrorKind::Cancelled, "operation cancelled")
            } else {
                SourceControlError::new(
                    SourceControlErrorKind::CommandFailed { exit_code },
                    message,
                )
            }
        }
        SvnError::MalformedResponse(message) => {
            SourceControlError::new(SourceControlErrorKind::MalformedResponse, message)
        }
        SvnError::InvalidRequest(message) => {
            SourceControlError::new(SourceControlErrorKind::InvalidRequest, message)
        }
        SvnError::StaleState(message) => {
            SourceControlError::new(SourceControlErrorKind::StaleState, message)
        }
        SvnError::Runner(message) => {
            SourceControlError::new(SourceControlErrorKind::TransportFailure, message)
        }
    }
}

fn extract_field(message: &str, field: &str) -> Option<Arc<str>> {
    message.lines().find_map(|line| {
        line.find(field)
            .map(|index| line[index + field.len()..].trim())
            .filter(|value| !value.is_empty())
            .map(Arc::from)
    })
}

fn certificate_failures(message: &str) -> Vec<Arc<str>> {
    let message = message.to_ascii_lowercase();
    let mut failures = Vec::new();
    if message.contains("not issued by a trusted authority")
        || message.contains("unknown certificate issuer")
        || message.contains("unknown ca")
    {
        failures.push(Arc::from("unknown-ca"));
    }
    if message.contains("hostname does not match") || message.contains("cn mismatch") {
        failures.push(Arc::from("cn-mismatch"));
    }
    if message.contains("not yet valid") {
        failures.push(Arc::from("not-yet-valid"));
    } else if message.contains("expired") {
        failures.push(Arc::from("expired"));
    }
    if failures.is_empty() {
        failures.push(Arc::from("other"));
    }
    failures
}

fn refresh_output() -> SourceControlOperationOutput {
    SourceControlOperationOutput {
        refresh_required: true,
        ..SourceControlOperationOutput::default()
    }
}

#[cfg(test)]
fn svn_target(path: &SourceControlPath) -> Result<String, SvnError> {
    if path.as_str().is_empty() {
        return Ok(".".to_owned());
    }
    if path.as_str().contains('\n') || path.as_str().contains('\r') {
        return Err(SvnError::InvalidRequest(
            "Subversion target paths cannot contain a newline".into(),
        ));
    }
    Ok(protect_peg(path.as_str()))
}

fn working_copy_root_target(info: &SvnInfo) -> Result<String, SvnError> {
    utf8_path(&info.working_copy_root).map(|path| protect_peg(&path))
}

fn working_copy_target(info: &SvnInfo, path: &SourceControlPath) -> Result<String, SvnError> {
    if path.as_str().contains(['\n', '\r']) {
        return Err(SvnError::InvalidRequest(
            "Subversion target paths cannot contain a newline".into(),
        ));
    }
    if path.as_str().is_empty() {
        return working_copy_root_target(info);
    }
    utf8_path(&info.working_copy_root.join(path.as_str())).map(|path| protect_peg(&path))
}

fn normalize_working_copy_path(
    path: &Path,
    info: &SvnInfo,
    execution_directory: &Path,
) -> Result<SourceControlPath, SvnError> {
    let absolute_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        execution_directory.join(path)
    };
    let relative_path = absolute_path
        .strip_prefix(&info.working_copy_root)
        .map_err(|_| {
            SvnError::MalformedResponse("svn info returned a path outside the working copy".into())
        })?;
    let path = relative_path
        .components()
        .filter(|component| *component != std::path::Component::CurDir)
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            SvnError::MalformedResponse("svn returned a non-UTF-8 working-copy path".into())
        })?
        .join("/");
    SourceControlPath::new(path).map_err(|error| SvnError::MalformedResponse(error.message))
}

fn protect_peg(target: &str) -> String {
    if target.contains('@') {
        format!("{target}@")
    } else {
        target.to_owned()
    }
}

fn validate_repository_url(url: &str) -> Result<(), SvnError> {
    let Some((_, remainder)) = url.split_once("://") else {
        return Err(SvnError::InvalidRequest(
            "a repository URL must include a scheme".into(),
        ));
    };
    let authority = remainder.split('/').next().unwrap_or(remainder);
    if authority.contains('@') {
        return Err(SvnError::InvalidRequest(
            "repository URLs containing credentials are not allowed".into(),
        ));
    }
    Ok(())
}

fn utf8_path(path: &Path) -> Result<String, SvnError> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| SvnError::InvalidRequest("Subversion requires a UTF-8 path".into()))
}

fn repository_display_name(path: &Path) -> Arc<str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(Arc::from)
        .unwrap_or_else(|| Arc::from(path.to_string_lossy().into_owned()))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::{
        fs,
        io::Write,
        net::TcpListener,
        path::PathBuf,
        process::{Child, Command, Stdio},
        thread,
        time::Duration,
    };
    use tempfile::{NamedTempFile, TempDir};

    struct NativeRunner {
        binary: PathBuf,
    }

    struct NativeTemporaryFile {
        file: NamedTempFile,
    }

    struct ChildGuard(Option<Child>);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    impl SvnTemporaryFile for NativeTemporaryFile {
        fn path(&self) -> &Path {
            self.file.path()
        }
    }

    impl SvnCommandRunner for NativeRunner {
        fn run(&self, command: &SvnCommand) -> Result<SvnCommandOutput, SvnError> {
            let mut process = Command::new(&self.binary);
            process
                .current_dir(&command.working_directory)
                .args(&command.arguments)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if command.stdin.is_some() {
                process.stdin(Stdio::piped());
            }
            let mut child = process
                .spawn()
                .map_err(|error| SvnError::Runner(Arc::from(error.to_string())))?;
            if let Some(stdin) = &command.stdin {
                let mut child_stdin = child.stdin.take().ok_or_else(|| {
                    SvnError::Runner("failed to open Subversion standard input".into())
                })?;
                child_stdin
                    .write_all(stdin)
                    .map_err(|error| SvnError::Runner(Arc::from(error.to_string())))?;
            }
            let output = child
                .wait_with_output()
                .map_err(|error| SvnError::Runner(Arc::from(error.to_string())))?;
            Ok(SvnCommandOutput {
                status: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }

        fn write_temporary_file(
            &self,
            contents: &[u8],
            _sensitive: bool,
        ) -> Result<Box<dyn SvnTemporaryFile>, SvnError> {
            let mut file = NamedTempFile::new()
                .map_err(|error| SvnError::Runner(Arc::from(error.to_string())))?;
            file.write_all(contents)
                .map_err(|error| SvnError::Runner(Arc::from(error.to_string())))?;
            Ok(Box::new(NativeTemporaryFile { file }))
        }
    }

    #[test]
    fn protects_peg_revisions_and_rejects_credential_urls() -> Result<(), SvnError> {
        let path = SourceControlPath::new("src/user@example.rs")
            .map_err(|error| SvnError::InvalidRequest(error.message))?;
        assert_eq!(svn_target(&path)?, "src/user@example.rs@");
        assert!(validate_repository_url("https://user:secret@example.test/svn").is_err());
        assert!(validate_repository_url("https://example.test/svn/project").is_ok());
        Ok(())
    }

    #[test]
    fn rejects_path_operations_when_status_is_stale() -> Result<(), SourceControlError> {
        let path = SourceControlPath::new("missing.txt")?;
        let statuses = vec![SourceControlStatusEntry {
            path: path.clone(),
            node_kind: source_control::SourceControlNodeKind::File,
            content_status: SourceControlContentStatus::Modified,
            property_status: SourceControlContentStatus::Normal,
            selection: SourceControlSelectionEligibility::IncludedByDefault,
            copied: false,
            switched: false,
            locked: false,
            tree_conflicted: false,
            out_of_date: false,
            added_lines: None,
            deleted_lines: None,
        }];

        assert!(matches!(
            validate_operation_state(
                &SourceControlOperation::Delete { paths: vec![path] },
                &statuses,
            ),
            Err(SvnError::StaleState(_))
        ));
        Ok(())
    }

    #[test]
    fn credentials_use_redacted_stdin_and_certificate_trust_is_temporary() -> Result<(), SvnError> {
        let command = SvnCommand::new(
            Path::new("/working-copy"),
            ["update", "--non-interactive", "--", "."],
        );
        let command = command_with_execution_options(
            command,
            &SourceControlExecutionOptions {
                authentication: Some(source_control::SourceControlAuthentication {
                    username: Arc::from("alice"),
                    password: "top-secret".to_owned().into(),
                }),
                trust: Some(source_control::SourceControlTrustDecision {
                    failures: vec![Arc::from("unknown-ca")],
                }),
            },
        )?;

        assert!(command.redact_stdin);
        assert_eq!(command.stdin.as_deref(), Some(b"top-secret\n".as_slice()));
        assert!(
            !command
                .arguments
                .iter()
                .any(|argument| argument == "top-secret")
        );
        assert!(
            command
                .arguments
                .windows(2)
                .any(|arguments| arguments == ["--username", "alice"])
        );
        assert!(
            command
                .arguments
                .iter()
                .any(|argument| argument == "--password-from-stdin")
        );
        assert!(
            command
                .arguments
                .iter()
                .any(|argument| argument == "--no-auth-cache")
        );
        assert!(
            command
                .arguments
                .windows(2)
                .any(|arguments| arguments == ["--trust-server-cert-failures", "unknown-ca"])
        );
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn authenticated_svnserve_retry_uses_password_stdin() -> Result<(), Box<dyn std::error::Error>>
    {
        if !command_available("svn")
            || !command_available("svnadmin")
            || !command_available("svnserve")
        {
            return Ok(());
        }

        let fixture = TempDir::new()?;
        let repository_path = fixture.path().join("authenticated-repository");
        run_fixture_command(
            "svnadmin",
            &["create", utf8_path(&repository_path)?.as_str()],
            fixture.path(),
        )?;
        let file_url = format!("file://{}", repository_path.display());
        run_fixture_command(
            "svn",
            &[
                "mkdir",
                &format!("{file_url}/trunk"),
                "--non-interactive",
                "-m",
                "Create trunk",
            ],
            fixture.path(),
        )?;
        fs::write(
            repository_path.join("conf/svnserve.conf"),
            "[general]\nanon-access = none\nauth-access = write\npassword-db = passwd\nrealm = zed-svn-test\n",
        )?;
        fs::write(
            repository_path.join("conf/passwd"),
            "[users]\nalice = top-secret\n",
        )?;

        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let checkout_path = fixture.path().join("authenticated-checkout");
        let svn_url = format!("svn://127.0.0.1:{port}/trunk");
        let command = || {
            SvnCommand::new(
                fixture.path(),
                [
                    "checkout".to_owned(),
                    "--non-interactive".to_owned(),
                    "--".to_owned(),
                    svn_url.clone(),
                    checkout_path.to_string_lossy().into_owned(),
                ],
            )
        };

        let server = start_svnserve_once(&repository_path, port)?;
        let runner = NativeRunner {
            binary: PathBuf::from("svn"),
        };
        let unauthenticated = runner.run(&command())?;
        assert_ne!(unauthenticated.status, Some(0));
        assert!(matches!(
            source_control_error(command_failed(unauthenticated)).kind,
            SourceControlErrorKind::AuthenticationRequired { .. }
        ));
        drop(server);

        let _server = start_svnserve_once(&repository_path, port)?;
        let authenticated = command_with_execution_options(
            command(),
            &SourceControlExecutionOptions {
                authentication: Some(source_control::SourceControlAuthentication {
                    username: Arc::from("alice"),
                    password: "top-secret".to_owned().into(),
                }),
                trust: None,
            },
        )?;
        checked_output(runner.run(&authenticated)?)?;
        assert!(checkout_path.join(".svn").is_dir());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn local_repository_end_to_end() -> Result<(), Box<dyn std::error::Error>> {
        if !command_available("svn") || !command_available("svnadmin") {
            return Ok(());
        }
        let fixture = TempDir::new()?;
        let repository_path = fixture.path().join("repository");
        let working_copy_path = fixture.path().join("working-copy");
        run_fixture_command(
            "svnadmin",
            &["create", utf8_path(&repository_path)?.as_str()],
            fixture.path(),
        )?;
        let repository_url = format!("file://{}", repository_path.display());
        run_fixture_command(
            "svn",
            &[
                "mkdir",
                &format!("{repository_url}/trunk"),
                "--non-interactive",
                "-m",
                "Create trunk",
            ],
            fixture.path(),
        )?;
        run_fixture_command(
            "svn",
            &[
                "checkout",
                &format!("{repository_url}/trunk"),
                utf8_path(&working_copy_path)?.as_str(),
                "--non-interactive",
            ],
            fixture.path(),
        )?;
        fs::create_dir_all(working_copy_path.join("src"))?;
        fs::write(working_copy_path.join("src/tracked.txt"), "first\n")?;
        run_fixture_command(
            "svn",
            &["add", "src", "--non-interactive"],
            &working_copy_path,
        )?;
        run_fixture_command(
            "svn",
            &["commit", "-m", "Initial content", "--non-interactive"],
            &working_copy_path,
        )?;

        let provider = SvnProvider::new(Arc::new(NativeRunner {
            binary: PathBuf::from("svn"),
        }));
        block_on(async {
            let repositories = provider
                .discover(&[SourceControlWorktree {
                    id: 7,
                    root_path: Arc::from(working_copy_path.join("src")),
                }])
                .await?;
            let repository = repositories
                .first()
                .ok_or("Subversion working copy was not discovered")?;
            assert_eq!(repository.worktree_ids, vec![7]);

            fs::write(working_copy_path.join("src/tracked.txt"), "second\n")?;
            fs::write(
                working_copy_path.join("src/added @räksmörgås.txt"),
                "added\n",
            )?;
            run_fixture_command(
                "svn",
                &["propset", "custom:review", "ready", "src/tracked.txt"],
                &working_copy_path,
            )?;
            let statuses = provider.status(repository).await?;
            assert!(statuses.iter().any(|entry| {
                entry.path.as_str() == "src/tracked.txt"
                    && entry.content_status == source_control::SourceControlContentStatus::Modified
                    && entry.node_kind == source_control::SourceControlNodeKind::File
            }));
            assert!(statuses.iter().any(|entry| {
                entry.path.as_str() == "src/added @räksmörgås.txt"
                    && entry.selection
                        == source_control::SourceControlSelectionEligibility::RequiresAdd
            }));

            let tracked_path = SourceControlPath::new("src/tracked.txt")?;
            let diff = provider
                .diff(repository, &tracked_path)
                .await?
                .ok_or("missing working-copy diff")?;
            assert!(diff.contains("-first"));
            assert!(diff.contains("+second"));
            assert!(diff.contains("custom:review"));
            let base_text = provider
                .load_text(repository, &tracked_path, None)
                .await?
                .ok_or("missing BASE text")?;
            assert_eq!(base_text, b"first\n");

            let added_path = SourceControlPath::new("src/added @räksmörgås.txt")?;
            provider
                .execute(
                    repository,
                    SourceControlOperation::Add {
                        paths: vec![added_path.clone()],
                    },
                )
                .await?;
            provider
                .execute(
                    repository,
                    SourceControlOperation::Commit {
                        paths: vec![tracked_path.clone(), added_path],
                        message: "Commit selected paths".to_owned(),
                    },
                )
                .await?;
            assert!(provider.status(repository).await?.is_empty());

            let history = provider
                .history(repository, Some(&tracked_path), 10)
                .await?;
            assert!(
                history
                    .iter()
                    .any(|entry| entry.message.as_ref() == "Commit selected paths")
            );
            let blame = provider.blame(repository, &tracked_path).await?;
            assert_eq!(blame.len(), 1);
            assert!(blame[0].revision.is_some());

            fs::write(
                working_copy_path.join("src/tracked.txt"),
                "local-only line\nsecond\n",
            )?;
            let blame_with_local_edits = provider.blame(repository, &tracked_path).await?;
            assert_eq!(
                blame_with_local_edits.len(),
                2,
                "svn blame should use the working-copy line layout"
            );
            assert!(
                blame_with_local_edits[0].revision.is_none(),
                "locally added lines should have no revision"
            );
            assert!(
                blame_with_local_edits[1].revision.is_some(),
                "unchanged working-copy lines should retain their revision"
            );

            fs::write(working_copy_path.join("src/tracked.txt"), "discard me\n")?;
            provider
                .execute(
                    repository,
                    SourceControlOperation::Revert {
                        paths: vec![tracked_path.clone()],
                    },
                )
                .await?;
            assert_eq!(
                fs::read_to_string(working_copy_path.join("src/tracked.txt"))?,
                "second\n"
            );

            fs::write(working_copy_path.join("src/generated.tmp"), "generated\n")?;
            let source_directory = SourceControlPath::new("src")?;
            provider
                .execute(
                    repository,
                    SourceControlOperation::Ignore {
                        directory: source_directory.clone(),
                        pattern: "generated.tmp".to_owned(),
                    },
                )
                .await?;
            assert!(
                !provider
                    .status(repository)
                    .await?
                    .iter()
                    .any(|status| status.path.as_str() == "src/generated.tmp"),
                "normal status should hide ignored paths"
            );
            let ignored_output = Command::new("svn")
                .current_dir(&working_copy_path)
                .args(["status", "--no-ignore", "--", "src/generated.tmp"])
                .output()?;
            assert!(ignored_output.status.success());
            let ignored_stdout = String::from_utf8(ignored_output.stdout)?;
            assert!(ignored_stdout.lines().any(|line| {
                line.starts_with('I') && line.trim_end().ends_with("src/generated.tmp")
            }));
            let ignore_diff = provider
                .diff(repository, &source_directory)
                .await?
                .ok_or("missing svn:ignore diff")?;
            assert!(ignore_diff.contains("svn:ignore"));
            assert!(ignore_diff.contains("generated.tmp"));
            fs::write(
                working_copy_path.join("src/tracked.txt"),
                "preserve this child change\n",
            )?;
            provider
                .execute(
                    repository,
                    SourceControlOperation::Revert {
                        paths: vec![source_directory],
                    },
                )
                .await?;
            assert_eq!(
                fs::read_to_string(working_copy_path.join("src/tracked.txt"))?,
                "preserve this child change\n"
            );
            provider
                .execute(
                    repository,
                    SourceControlOperation::Revert {
                        paths: vec![tracked_path.clone()],
                    },
                )
                .await?;

            provider
                .execute(repository, SourceControlOperation::Update)
                .await?;

            let branches_url = format!("{repository_url}/branches");
            run_fixture_command(
                "svn",
                &[
                    "mkdir",
                    &branches_url,
                    "--non-interactive",
                    "-m",
                    "Create branches",
                ],
                fixture.path(),
            )?;
            let alternate_url = format!("{branches_url}/alternate");
            run_fixture_command(
                "svn",
                &[
                    "copy",
                    &format!("{repository_url}/trunk"),
                    &alternate_url,
                    "--non-interactive",
                    "-m",
                    "Create alternate branch",
                ],
                fixture.path(),
            )?;

            fs::remove_file(working_copy_path.join("src/tracked.txt"))?;
            let missing_base_text = provider
                .load_text(repository, &tracked_path, None)
                .await?
                .ok_or("missing BASE text for deleted working-copy file")?;
            assert_eq!(missing_base_text, b"second\n");
            assert!(provider.status(repository).await?.iter().any(|status| {
                status.path == tracked_path
                    && status.selection == SourceControlSelectionEligibility::RequiresDelete
            }));
            provider
                .execute(
                    repository,
                    SourceControlOperation::Delete {
                        paths: vec![tracked_path.clone()],
                    },
                )
                .await?;
            provider
                .execute(
                    repository,
                    SourceControlOperation::Commit {
                        paths: vec![tracked_path],
                        message: "Delete the missing path".to_owned(),
                    },
                )
                .await?;

            provider
                .execute(
                    repository,
                    SourceControlOperation::Switch {
                        url: alternate_url,
                        revision: None,
                    },
                )
                .await?;
            assert!(working_copy_path.join("src/tracked.txt").is_file());

            let checkout_path = fixture.path().join("second-checkout");
            provider
                .execute(
                    repository,
                    SourceControlOperation::Checkout {
                        url: format!("{repository_url}/trunk"),
                        destination: checkout_path.clone(),
                        revision: None,
                    },
                )
                .await?;
            assert!(checkout_path.join(".svn").is_dir());
            Ok::<(), Box<dyn std::error::Error>>(())
        })?;
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn update_postpones_conflicts_until_explicit_resolution()
    -> Result<(), Box<dyn std::error::Error>> {
        if !command_available("svn") || !command_available("svnadmin") {
            return Ok(());
        }
        let fixture = TempDir::new()?;
        let repository_path = fixture.path().join("conflict-repository");
        let local_working_copy = fixture.path().join("local-working-copy");
        let remote_working_copy = fixture.path().join("remote-working-copy");
        run_fixture_command(
            "svnadmin",
            &["create", utf8_path(&repository_path)?.as_str()],
            fixture.path(),
        )?;
        let trunk_url = format!("file://{}/trunk", repository_path.display());
        run_fixture_command(
            "svn",
            &[
                "mkdir",
                &trunk_url,
                "--non-interactive",
                "-m",
                "Create trunk",
            ],
            fixture.path(),
        )?;
        run_fixture_command(
            "svn",
            &[
                "checkout",
                &trunk_url,
                utf8_path(&local_working_copy)?.as_str(),
                "--non-interactive",
            ],
            fixture.path(),
        )?;
        fs::write(local_working_copy.join("conflict.txt"), "initial\n")?;
        run_fixture_command(
            "svn",
            &["add", "conflict.txt", "--non-interactive"],
            &local_working_copy,
        )?;
        run_fixture_command(
            "svn",
            &["commit", "-m", "Initial content", "--non-interactive"],
            &local_working_copy,
        )?;
        run_fixture_command(
            "svn",
            &[
                "checkout",
                &trunk_url,
                utf8_path(&remote_working_copy)?.as_str(),
                "--non-interactive",
            ],
            fixture.path(),
        )?;

        let provider = SvnProvider::new(Arc::new(NativeRunner {
            binary: PathBuf::from("svn"),
        }));
        block_on(async {
            let repositories = provider
                .discover(&[SourceControlWorktree {
                    id: 8,
                    root_path: Arc::from(local_working_copy.as_path()),
                }])
                .await?;
            let repository = repositories
                .first()
                .ok_or("Subversion working copy was not discovered")?;
            let path = SourceControlPath::new("conflict.txt")?;

            fs::write(local_working_copy.join("conflict.txt"), "local\n")?;
            fs::write(remote_working_copy.join("conflict.txt"), "remote\n")?;
            run_fixture_command(
                "svn",
                &["commit", "-m", "Remote change", "--non-interactive"],
                &remote_working_copy,
            )?;

            provider
                .execute(repository, SourceControlOperation::Update)
                .await?;
            let statuses = provider.status(repository).await?;
            assert!(statuses.iter().any(|status| {
                status.path == path
                    && status.content_status == SourceControlContentStatus::Conflicted
            }));
            assert!(
                fs::read_to_string(local_working_copy.join("conflict.txt"))?.contains("<<<<<<<")
            );

            provider
                .execute(
                    repository,
                    SourceControlOperation::Resolve {
                        paths: vec![path.clone()],
                    },
                )
                .await?;
            assert!(provider.status(repository).await?.iter().any(|status| {
                status.path == path
                    && status.content_status == SourceControlContentStatus::Modified
                    && !status.is_conflicted()
            }));
            provider
                .execute(
                    repository,
                    SourceControlOperation::Revert { paths: vec![path] },
                )
                .await?;
            assert_eq!(
                fs::read_to_string(local_working_copy.join("conflict.txt"))?,
                "remote\n"
            );
            Ok::<(), Box<dyn std::error::Error>>(())
        })?;
        Ok(())
    }

    fn command_available(program: &str) -> bool {
        Command::new(program)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn start_svnserve_once(
        repository_path: &Path,
        port: u16,
    ) -> Result<ChildGuard, Box<dyn std::error::Error>> {
        let child = Command::new("svnserve")
            .args([
                "--listen-once",
                "--listen-host",
                "127.0.0.1",
                "--listen-port",
                &port.to_string(),
                "--root",
                utf8_path(repository_path)?.as_str(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        thread::sleep(Duration::from_millis(100));
        Ok(ChildGuard(Some(child)))
    }

    fn run_fixture_command(
        program: &str,
        arguments: &[&str],
        working_directory: &Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let output = Command::new(program)
            .current_dir(working_directory)
            .args(arguments)
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "{program} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        Ok(())
    }
}
