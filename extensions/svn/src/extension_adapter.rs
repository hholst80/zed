use crate::{
    SvnCommand, SvnCommandOutput, SvnCommandRunner, SvnError, SvnProvider, SvnTemporaryFile,
};
use futures::executor::block_on;
use source_control as core_sc;
use source_control::SourceControlProvider as _;
use std::{
    fs::OpenOptions,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use zed_extension_api::{self as zed, process::ScopedCommand};

const PROVIDER_ID: &str = "subversion";
static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ZedSvnRunner<'a> {
    worktree: &'a zed::Worktree,
}

impl<'a> ZedSvnRunner<'a> {
    fn new(worktree: &'a zed::Worktree) -> Result<Self, SvnError> {
        worktree.which("svn").ok_or(SvnError::ClientUnavailable)?;
        Ok(Self { worktree })
    }
}

impl SvnCommandRunner for ZedSvnRunner<'_> {
    fn run(&self, command: &SvnCommand) -> Result<SvnCommandOutput, SvnError> {
        let mut scoped_command = ScopedCommand::new("svn")
            .args(command.arguments.clone())
            .current_dir(command.working_directory.to_string_lossy().into_owned());
        if let Some(stdin) = &command.stdin {
            scoped_command = scoped_command.stdin(stdin.clone(), command.redact_stdin);
        }
        let output = self
            .worktree
            .run_command(&scoped_command)
            .map_err(|error| SvnError::Runner(Arc::from(error)))?;
        Ok(SvnCommandOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn write_temporary_file(
        &self,
        contents: &[u8],
        sensitive: bool,
    ) -> Result<Box<dyn SvnTemporaryFile>, SvnError> {
        const MAX_ATTEMPTS: usize = 32;

        let directory = std::env::current_dir()
            .map_err(|error| SvnError::Runner(Arc::from(error.to_string())))?;
        for _ in 0..MAX_ATTEMPTS {
            let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".zed-svn-{sequence:016x}.tmp"));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
                        let _ = std::fs::remove_file(&path);
                        return Err(SvnError::Runner(Arc::from(error.to_string())));
                    }
                    return Ok(Box::new(ZedTemporaryFile { path, sensitive }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(SvnError::Runner(Arc::from(error.to_string()))),
            }
        }
        Err(SvnError::Runner(Arc::from(
            "failed to allocate a unique Subversion temporary file",
        )))
    }
}

struct ZedTemporaryFile {
    path: PathBuf,
    sensitive: bool,
}

impl SvnTemporaryFile for ZedTemporaryFile {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ZedTemporaryFile {
    fn drop(&mut self) {
        if self.sensitive
            && let Ok(metadata) = std::fs::metadata(&self.path)
        {
            let zeros = vec![0; metadata.len().min(1024 * 1024) as usize];
            if let Ok(mut file) = OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&self.path)
            {
                let _ = file.write_all(&zeros);
                let _ = file.sync_all();
            }
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

struct SvnExtension;

impl SvnExtension {
    fn provider<'a>(
        worktree: &'a zed::Worktree,
    ) -> Result<SvnProvider<'a>, zed::SourceControlProviderError> {
        let runner = ZedSvnRunner::new(worktree).map_err(svn_error_to_zed)?;
        Ok(SvnProvider::new(Arc::new(runner)))
    }

    fn repository<'a>(
        provider: &SvnProvider<'a>,
        repository_id: &str,
        worktree: &zed::Worktree,
    ) -> Result<core_sc::SourceControlRepository, zed::SourceControlProviderError> {
        let worktree = core_sc::SourceControlWorktree {
            id: worktree.id(),
            root_path: Arc::from(PathBuf::from(worktree.root_path())),
        };
        block_on(provider.discover(&[worktree]))
            .map_err(core_error_to_zed)?
            .into_iter()
            .find(|repository| repository.key.repository_id.as_str() == repository_id)
            .ok_or_else(|| zed::SourceControlProviderError {
                kind: zed::SourceControlErrorKind::RepositoryNotFound,
                message: "the Subversion working copy is no longer available".to_owned(),
            })
    }

    fn validate_provider(provider_id: &str) -> Result<(), zed::SourceControlProviderError> {
        if provider_id == PROVIDER_ID {
            Ok(())
        } else {
            Err(zed::SourceControlProviderError {
                kind: zed::SourceControlErrorKind::ProviderUnavailable,
                message: format!("unknown source-control provider {provider_id:?}"),
            })
        }
    }
}

impl zed::Extension for SvnExtension {
    fn new() -> Self {
        Self
    }

    fn source_control_discover(
        &mut self,
        provider_id: &str,
        worktree: &zed::Worktree,
    ) -> Result<Vec<zed::SourceControlRepository>, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let worktree = core_sc::SourceControlWorktree {
            id: worktree.id(),
            root_path: Arc::from(PathBuf::from(worktree.root_path())),
        };
        block_on(provider.discover(&[worktree]))
            .map_err(core_error_to_zed)
            .map(|repositories| repositories.into_iter().map(repository_to_zed).collect())
    }

    fn source_control_status(
        &mut self,
        provider_id: &str,
        repository_id: &str,
        worktree: &zed::Worktree,
    ) -> Result<Vec<zed::SourceControlStatusEntry>, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let repository = Self::repository(&provider, repository_id, worktree)?;
        block_on(provider.status(&repository))
            .map_err(core_error_to_zed)
            .map(|statuses| statuses.into_iter().map(status_to_zed).collect())
    }

    fn source_control_load_text(
        &mut self,
        provider_id: &str,
        repository_id: &str,
        path: &str,
        revision: Option<zed::SourceControlRevision>,
        worktree: &zed::Worktree,
    ) -> Result<Option<Vec<u8>>, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let repository = Self::repository(&provider, repository_id, worktree)?;
        let path = core_sc::SourceControlPath::new(path).map_err(core_error_to_zed)?;
        let revision = revision.map(revision_from_zed);
        block_on(provider.load_text(&repository, &path, revision.as_ref()))
            .map_err(core_error_to_zed)
    }

    fn source_control_diff(
        &mut self,
        provider_id: &str,
        repository_id: &str,
        path: &str,
        worktree: &zed::Worktree,
    ) -> Result<Option<String>, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let repository = Self::repository(&provider, repository_id, worktree)?;
        let path = core_sc::SourceControlPath::new(path).map_err(core_error_to_zed)?;
        block_on(provider.diff(&repository, &path))
            .map_err(core_error_to_zed)
            .map(|diff| diff.map(|diff| diff.to_string()))
    }

    fn source_control_history(
        &mut self,
        provider_id: &str,
        repository_id: &str,
        path: Option<&str>,
        limit: u32,
        worktree: &zed::Worktree,
    ) -> Result<Vec<zed::SourceControlHistoryEntry>, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let repository = Self::repository(&provider, repository_id, worktree)?;
        let path = path
            .map(core_sc::SourceControlPath::new)
            .transpose()
            .map_err(core_error_to_zed)?;
        block_on(provider.history(&repository, path.as_ref(), limit as usize))
            .map_err(core_error_to_zed)
            .map(|history| history.into_iter().map(history_to_zed).collect())
    }

    fn source_control_blame(
        &mut self,
        provider_id: &str,
        repository_id: &str,
        path: &str,
        worktree: &zed::Worktree,
    ) -> Result<Vec<zed::SourceControlBlameLine>, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let repository = Self::repository(&provider, repository_id, worktree)?;
        let path = core_sc::SourceControlPath::new(path).map_err(core_error_to_zed)?;
        block_on(provider.blame(&repository, &path))
            .map_err(core_error_to_zed)
            .map(|blame| blame.into_iter().map(blame_to_zed).collect())
    }

    fn source_control_execute(
        &mut self,
        provider_id: &str,
        repository_id: &str,
        operation: zed::SourceControlOperation,
        options: zed::SourceControlExecutionOptions,
        worktree: &zed::Worktree,
    ) -> Result<zed::SourceControlOperationOutput, zed::SourceControlProviderError> {
        Self::validate_provider(provider_id)?;
        let provider = Self::provider(worktree)?;
        let repository = Self::repository(&provider, repository_id, worktree)?;
        let operation = operation_from_zed(operation)?;
        let options = core_sc::SourceControlExecutionOptions {
            authentication: options.authentication.map(|authentication| {
                core_sc::SourceControlAuthentication {
                    username: authentication.username.into(),
                    password: authentication.password.into(),
                }
            }),
            trust: options
                .trust
                .map(|trust| core_sc::SourceControlTrustDecision {
                    failures: trust.failures.into_iter().map(Into::into).collect(),
                }),
        };
        block_on(provider.execute_with_options(&repository, operation, options))
            .map_err(core_error_to_zed)
            .map(operation_output_to_zed)
    }
}

fn repository_to_zed(repository: core_sc::SourceControlRepository) -> zed::SourceControlRepository {
    zed::SourceControlRepository {
        id: repository.key.repository_id.as_str().to_owned(),
        working_copy_path: repository.working_copy_path.to_string_lossy().into_owned(),
        display_name: repository.display_name.to_string(),
        summary: repository.summary.map(|summary| summary.to_string()),
        capabilities: capabilities_to_zed(repository.capabilities),
    }
}

fn capabilities_to_zed(
    value: core_sc::SourceControlCapabilities,
) -> zed::SourceControlCapabilities {
    zed::SourceControlCapabilities {
        discovery: value.discovery,
        status: value.status,
        base_text: value.base_text,
        diff: value.diff,
        commit: value.commit,
        add: value.add,
        delete: value.delete,
        revert: value.revert,
        resolve: value.resolve,
        ignore: value.ignore,
        update: value.update,
        checkout: value.checkout,
        switch: value.switch,
        history: value.history,
        blame: value.blame,
    }
}

fn status_to_zed(value: core_sc::SourceControlStatusEntry) -> zed::SourceControlStatusEntry {
    zed::SourceControlStatusEntry {
        path: value.path.as_str().to_owned(),
        node_kind: match value.node_kind {
            core_sc::SourceControlNodeKind::File => zed::SourceControlNodeKind::File,
            core_sc::SourceControlNodeKind::Directory => zed::SourceControlNodeKind::Directory,
            core_sc::SourceControlNodeKind::SymbolicLink => {
                zed::SourceControlNodeKind::SymbolicLink
            }
            core_sc::SourceControlNodeKind::Unknown => zed::SourceControlNodeKind::Unknown,
        },
        content_status: content_status_to_zed(value.content_status),
        property_status: content_status_to_zed(value.property_status),
        selection: match value.selection {
            core_sc::SourceControlSelectionEligibility::IncludedByDefault => {
                zed::SourceControlSelectionEligibility::IncludedByDefault
            }
            core_sc::SourceControlSelectionEligibility::Available => {
                zed::SourceControlSelectionEligibility::Available
            }
            core_sc::SourceControlSelectionEligibility::RequiresAdd => {
                zed::SourceControlSelectionEligibility::RequiresAdd
            }
            core_sc::SourceControlSelectionEligibility::RequiresDelete => {
                zed::SourceControlSelectionEligibility::RequiresDelete
            }
            core_sc::SourceControlSelectionEligibility::Unavailable => {
                zed::SourceControlSelectionEligibility::Unavailable
            }
        },
        copied: value.copied,
        switched: value.switched,
        locked: value.locked,
        tree_conflicted: value.tree_conflicted,
        out_of_date: value.out_of_date,
        added_lines: value.added_lines,
        deleted_lines: value.deleted_lines,
    }
}

fn content_status_to_zed(
    value: core_sc::SourceControlContentStatus,
) -> zed::SourceControlContentStatus {
    match value {
        core_sc::SourceControlContentStatus::Normal => zed::SourceControlContentStatus::Normal,
        core_sc::SourceControlContentStatus::Modified => zed::SourceControlContentStatus::Modified,
        core_sc::SourceControlContentStatus::Added => zed::SourceControlContentStatus::Added,
        core_sc::SourceControlContentStatus::Deleted => zed::SourceControlContentStatus::Deleted,
        core_sc::SourceControlContentStatus::Missing => zed::SourceControlContentStatus::Missing,
        core_sc::SourceControlContentStatus::Replaced => zed::SourceControlContentStatus::Replaced,
        core_sc::SourceControlContentStatus::Conflicted => {
            zed::SourceControlContentStatus::Conflicted
        }
        core_sc::SourceControlContentStatus::Obstructed => {
            zed::SourceControlContentStatus::Obstructed
        }
        core_sc::SourceControlContentStatus::Unversioned => {
            zed::SourceControlContentStatus::Unversioned
        }
        core_sc::SourceControlContentStatus::Ignored => zed::SourceControlContentStatus::Ignored,
        core_sc::SourceControlContentStatus::External => zed::SourceControlContentStatus::External,
    }
}

fn history_to_zed(value: core_sc::SourceControlHistoryEntry) -> zed::SourceControlHistoryEntry {
    zed::SourceControlHistoryEntry {
        revision: revision_to_zed(value.revision),
        author: value.author.map(|author| author.to_string()),
        timestamp: value.timestamp.map(|timestamp| timestamp.to_string()),
        message: value.message.to_string(),
        changed_paths: value
            .changed_paths
            .into_iter()
            .map(|path| path.as_str().to_owned())
            .collect(),
    }
}

fn blame_to_zed(value: core_sc::SourceControlBlameLine) -> zed::SourceControlBlameLine {
    zed::SourceControlBlameLine {
        line_number: value.line_number,
        revision: value.revision.map(revision_to_zed),
        author: value.author.map(|author| author.to_string()),
        timestamp: value.timestamp.map(|timestamp| timestamp.to_string()),
    }
}

fn revision_to_zed(value: core_sc::SourceControlRevision) -> zed::SourceControlRevision {
    zed::SourceControlRevision {
        canonical: value.canonical.to_string(),
        display: value.display.to_string(),
    }
}

fn revision_from_zed(value: zed::SourceControlRevision) -> core_sc::SourceControlRevision {
    core_sc::SourceControlRevision::new(value.canonical, value.display)
}

fn operation_from_zed(
    value: zed::SourceControlOperation,
) -> Result<core_sc::SourceControlOperation, zed::SourceControlProviderError> {
    let paths = |values: Vec<String>| {
        values
            .into_iter()
            .map(core_sc::SourceControlPath::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(core_error_to_zed)
    };
    Ok(match value {
        zed::SourceControlOperation::Commit(operation) => core_sc::SourceControlOperation::Commit {
            paths: paths(operation.paths)?,
            message: operation.message,
        },
        zed::SourceControlOperation::Add(operation) => core_sc::SourceControlOperation::Add {
            paths: paths(operation.paths)?,
        },
        zed::SourceControlOperation::Delete(operation) => core_sc::SourceControlOperation::Delete {
            paths: paths(operation.paths)?,
        },
        zed::SourceControlOperation::Revert(operation) => core_sc::SourceControlOperation::Revert {
            paths: paths(operation.paths)?,
        },
        zed::SourceControlOperation::Resolve(operation) => {
            core_sc::SourceControlOperation::Resolve {
                paths: paths(operation.paths)?,
            }
        }
        zed::SourceControlOperation::Ignore(operation) => core_sc::SourceControlOperation::Ignore {
            directory: core_sc::SourceControlPath::new(operation.directory)
                .map_err(core_error_to_zed)?,
            pattern: operation.pattern,
        },
        zed::SourceControlOperation::Update => core_sc::SourceControlOperation::Update,
        zed::SourceControlOperation::Checkout(operation) => {
            core_sc::SourceControlOperation::Checkout {
                url: operation.url,
                destination: PathBuf::from(operation.destination),
                revision: operation.revision.map(revision_from_zed),
            }
        }
        zed::SourceControlOperation::Switch(operation) => core_sc::SourceControlOperation::Switch {
            url: operation.url,
            revision: operation.revision.map(revision_from_zed),
        },
    })
}

fn operation_output_to_zed(
    value: core_sc::SourceControlOperationOutput,
) -> zed::SourceControlOperationOutput {
    zed::SourceControlOperationOutput {
        message: value.message.map(|message| message.to_string()),
        opened_path: value
            .opened_path
            .map(|path| path.to_string_lossy().into_owned()),
        refresh_required: value.refresh_required,
    }
}

fn svn_error_to_zed(error: SvnError) -> zed::SourceControlProviderError {
    let kind = match &error {
        SvnError::ClientUnavailable | SvnError::UnsupportedVersion { .. } => {
            zed::SourceControlErrorKind::ProviderUnavailable
        }
        SvnError::CommandFailed { exit_code, .. } => {
            zed::SourceControlErrorKind::CommandFailed(*exit_code)
        }
        SvnError::MalformedResponse(_) => zed::SourceControlErrorKind::MalformedResponse,
        SvnError::InvalidRequest(_) => zed::SourceControlErrorKind::InvalidRequest,
        SvnError::StaleState(_) => zed::SourceControlErrorKind::StaleState,
        SvnError::Runner(_) => zed::SourceControlErrorKind::TransportFailure,
    };
    zed::SourceControlProviderError {
        kind,
        message: error.to_string(),
    }
}

fn core_error_to_zed(error: core_sc::SourceControlError) -> zed::SourceControlProviderError {
    let kind = match error.kind {
        core_sc::SourceControlErrorKind::ProviderUnavailable => {
            zed::SourceControlErrorKind::ProviderUnavailable
        }
        core_sc::SourceControlErrorKind::RepositoryNotFound => {
            zed::SourceControlErrorKind::RepositoryNotFound
        }
        core_sc::SourceControlErrorKind::UnsupportedOperation { capability } => {
            zed::SourceControlErrorKind::UnsupportedOperation(capability.to_string())
        }
        core_sc::SourceControlErrorKind::AuthenticationRequired { realm, username } => {
            zed::SourceControlErrorKind::AuthenticationRequired(
                zed::SourceControlAuthenticationRequest {
                    realm: realm.to_string(),
                    username: username.map(|username| username.to_string()),
                },
            )
        }
        core_sc::SourceControlErrorKind::TrustRequired {
            realm,
            fingerprint,
            failures,
        } => zed::SourceControlErrorKind::TrustRequired(zed::SourceControlTrustRequest {
            realm: realm.to_string(),
            fingerprint: fingerprint.to_string(),
            failures: failures
                .into_iter()
                .map(|failure| failure.to_string())
                .collect(),
        }),
        core_sc::SourceControlErrorKind::Cancelled => zed::SourceControlErrorKind::Cancelled,
        core_sc::SourceControlErrorKind::StaleState => zed::SourceControlErrorKind::StaleState,
        core_sc::SourceControlErrorKind::CommandFailed { exit_code } => {
            zed::SourceControlErrorKind::CommandFailed(exit_code)
        }
        core_sc::SourceControlErrorKind::MalformedResponse => {
            zed::SourceControlErrorKind::MalformedResponse
        }
        core_sc::SourceControlErrorKind::TransportFailure => {
            zed::SourceControlErrorKind::TransportFailure
        }
        core_sc::SourceControlErrorKind::InvalidRequest => {
            zed::SourceControlErrorKind::InvalidRequest
        }
    };
    zed::SourceControlProviderError {
        kind,
        message: error.message.to_string(),
    }
}

zed::register_extension!(SvnExtension);
