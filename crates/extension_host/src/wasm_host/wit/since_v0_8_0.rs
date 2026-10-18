use crate::wasm_host::wit::since_v0_6_0::slash_command::SlashCommandOutputSection;
use crate::wasm_host::wit::{CompletionKind, CompletionLabelDetails, InsertTextFormat, SymbolKind};
use crate::wasm_host::{WasmState, wit::ToWasmtimeResult};
use ::http_client::{AsyncBody, HttpRequestExt};
use ::settings::{Settings, WorktreeId};
use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use async_trait::async_trait;
use extension::{
    ExtensionLanguageServerProxy, KeyValueStoreDelegate, ProjectDelegate, WorktreeDelegate,
};
use futures::{AsyncReadExt, AsyncWriteExt, lock::Mutex};
use futures::{FutureExt as _, io::BufReader};
use gpui::{BackgroundExecutor, SharedString};
use language::{BinaryStatus, LanguageName, language_settings::AllLanguageSettings};
use project::project_settings::ProjectSettings;
use semver::Version;
use std::{
    collections::BTreeMap,
    env,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use task::{SpawnInTerminal, ZedDebugConfig};
use url::Url;
use util::{
    archive::extract_zip, fs::make_file_executable, maybe, paths::PathStyle, rel_path::RelPath,
};
use wasmtime::component::{Linker, Resource};

pub const MIN_VERSION: Version = Version::new(0, 8, 0);
pub const MAX_VERSION: Version = Version::new(0, 8, 0);

wasmtime::component::bindgen!({
    imports: {
        default: async | trappable,
    },
    exports: {
        default: async,
    },
    path: "../extension_api/wit/since_v0.8.0",
    with: {
         "worktree": ExtensionWorktree,
         "project": ExtensionProject,
         "key-value-store": ExtensionKeyValueStore,
         "zed:extension/http-client/http-response-stream": ExtensionHttpResponseStream
    },
});

pub use self::zed::extension::*;

mod settings {
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/since_v0.8.0/settings.rs"));
}

pub type ExtensionWorktree = Arc<dyn WorktreeDelegate>;
pub type ExtensionProject = Arc<dyn ProjectDelegate>;
pub type ExtensionKeyValueStore = Arc<dyn KeyValueStoreDelegate>;
pub type ExtensionHttpResponseStream = Arc<Mutex<::http_client::Response<AsyncBody>>>;

pub fn linker(executor: &BackgroundExecutor) -> &'static Linker<WasmState> {
    static LINKER: OnceLock<Linker<WasmState>> = OnceLock::new();
    LINKER.get_or_init(|| {
        super::new_linker(executor, |linker| {
            Extension::add_to_linker::<_, WasmState>(linker, |s| s)
        })
    })
}

impl From<Range> for std::ops::Range<usize> {
    fn from(range: Range) -> Self {
        let start = range.start as usize;
        let end = range.end as usize;
        start..end
    }
}

impl From<Command> for extension::Command {
    fn from(value: Command) -> Self {
        Self {
            command: value.command.into(),
            args: value.args,
            env: value.env,
        }
    }
}

pub fn source_control_repository_from_wit(
    value: source_control::Repository,
    provider_id: ::source_control::SourceControlProviderId,
    worktree_id: u64,
) -> std::result::Result<
    ::source_control::SourceControlRepository,
    ::source_control::SourceControlError,
> {
    let repository_id = ::source_control::SourceControlRepositoryId::new(value.id)?;
    let working_copy_path = PathBuf::from(value.working_copy_path);
    if !working_copy_path.is_absolute() {
        return Err(::source_control::SourceControlError::new(
            ::source_control::SourceControlErrorKind::MalformedResponse,
            "a source-control provider returned a non-absolute working-copy path",
        ));
    }
    Ok(::source_control::SourceControlRepository {
        key: ::source_control::SourceControlRepositoryKey::new(provider_id, repository_id),
        worktree_ids: vec![worktree_id],
        working_copy_path: working_copy_path.into(),
        display_name: value.display_name.into(),
        summary: value.summary.map(Into::into),
        capabilities: source_control_capabilities_from_wit(value.capabilities),
    })
}

pub fn source_control_status_from_wit(
    value: source_control::StatusEntry,
) -> std::result::Result<
    ::source_control::SourceControlStatusEntry,
    ::source_control::SourceControlError,
> {
    Ok(::source_control::SourceControlStatusEntry {
        path: ::source_control::SourceControlPath::new(value.path)?,
        node_kind: match value.node_kind {
            source_control::NodeKind::File => ::source_control::SourceControlNodeKind::File,
            source_control::NodeKind::Directory => {
                ::source_control::SourceControlNodeKind::Directory
            }
            source_control::NodeKind::SymbolicLink => {
                ::source_control::SourceControlNodeKind::SymbolicLink
            }
            source_control::NodeKind::Unknown => ::source_control::SourceControlNodeKind::Unknown,
        },
        content_status: source_control_content_status_from_wit(value.content_status),
        property_status: source_control_content_status_from_wit(value.property_status),
        selection: match value.selection {
            source_control::SelectionEligibility::IncludedByDefault => {
                ::source_control::SourceControlSelectionEligibility::IncludedByDefault
            }
            source_control::SelectionEligibility::Available => {
                ::source_control::SourceControlSelectionEligibility::Available
            }
            source_control::SelectionEligibility::RequiresAdd => {
                ::source_control::SourceControlSelectionEligibility::RequiresAdd
            }
            source_control::SelectionEligibility::RequiresDelete => {
                ::source_control::SourceControlSelectionEligibility::RequiresDelete
            }
            source_control::SelectionEligibility::Unavailable => {
                ::source_control::SourceControlSelectionEligibility::Unavailable
            }
        },
        copied: value.copied,
        switched: value.switched,
        locked: value.locked,
        tree_conflicted: value.tree_conflicted,
        out_of_date: value.out_of_date,
        added_lines: value.added_lines,
        deleted_lines: value.deleted_lines,
    })
}

pub fn source_control_history_from_wit(
    value: source_control::HistoryEntry,
) -> std::result::Result<
    ::source_control::SourceControlHistoryEntry,
    ::source_control::SourceControlError,
> {
    Ok(::source_control::SourceControlHistoryEntry {
        revision: source_control_revision_from_wit(value.revision),
        author: value.author.map(Into::into),
        timestamp: value.timestamp.map(Into::into),
        message: value.message.into(),
        changed_paths: value
            .changed_paths
            .into_iter()
            .map(::source_control::SourceControlPath::new)
            .collect::<std::result::Result<Vec<_>, _>>()?,
    })
}

pub fn source_control_blame_from_wit(
    value: source_control::BlameLine,
) -> ::source_control::SourceControlBlameLine {
    ::source_control::SourceControlBlameLine {
        line_number: value.line_number,
        revision: value.revision.map(source_control_revision_from_wit),
        author: value.author.map(Into::into),
        timestamp: value.timestamp.map(Into::into),
    }
}

pub fn source_control_operation_to_wit(
    value: ::source_control::SourceControlOperation,
) -> std::result::Result<source_control::Operation, ::source_control::SourceControlError> {
    Ok(match value {
        ::source_control::SourceControlOperation::Commit { paths, message } => {
            source_control::Operation::Commit(source_control::CommitOperation {
                paths: source_control_paths_to_wit(paths),
                message,
            })
        }
        ::source_control::SourceControlOperation::Add { paths } => {
            source_control::Operation::Add(source_control::PathsOperation {
                paths: source_control_paths_to_wit(paths),
            })
        }
        ::source_control::SourceControlOperation::Delete { paths } => {
            source_control::Operation::Delete(source_control::PathsOperation {
                paths: source_control_paths_to_wit(paths),
            })
        }
        ::source_control::SourceControlOperation::Revert { paths } => {
            source_control::Operation::Revert(source_control::PathsOperation {
                paths: source_control_paths_to_wit(paths),
            })
        }
        ::source_control::SourceControlOperation::Resolve { paths } => {
            source_control::Operation::Resolve(source_control::PathsOperation {
                paths: source_control_paths_to_wit(paths),
            })
        }
        ::source_control::SourceControlOperation::Ignore { directory, pattern } => {
            source_control::Operation::Ignore(source_control::IgnoreOperation {
                directory: directory.as_str().to_owned(),
                pattern,
            })
        }
        ::source_control::SourceControlOperation::Update => source_control::Operation::Update,
        ::source_control::SourceControlOperation::Checkout {
            url,
            destination,
            revision,
        } => source_control::Operation::Checkout(source_control::CheckoutOperation {
            url,
            destination: destination
                .to_str()
                .ok_or_else(|| {
                    ::source_control::SourceControlError::invalid_request(
                        "checkout destination must be UTF-8",
                    )
                })?
                .to_owned(),
            revision: revision.map(source_control_revision_to_wit),
        }),
        ::source_control::SourceControlOperation::Switch { url, revision } => {
            source_control::Operation::Switch(source_control::SwitchOperation {
                url,
                revision: revision.map(source_control_revision_to_wit),
            })
        }
    })
}

pub fn source_control_execution_options_to_wit(
    value: ::source_control::SourceControlExecutionOptions,
) -> source_control::ExecutionOptions {
    source_control::ExecutionOptions {
        authentication: value
            .authentication
            .map(|authentication| source_control::Authentication {
                username: authentication.username.to_string(),
                password: authentication.password.as_str().to_owned(),
            }),
        trust: value.trust.map(|trust| source_control::TrustDecision {
            failures: trust
                .failures
                .into_iter()
                .map(|failure| failure.to_string())
                .collect(),
        }),
    }
}

pub fn source_control_operation_output_from_wit(
    value: source_control::OperationOutput,
) -> ::source_control::SourceControlOperationOutput {
    ::source_control::SourceControlOperationOutput {
        message: value.message.map(Into::into),
        opened_path: value.opened_path.map(PathBuf::from),
        refresh_required: value.refresh_required,
    }
}

pub fn source_control_error_from_wit(
    value: source_control::ProviderError,
) -> ::source_control::SourceControlError {
    let kind = match value.kind {
        source_control::ErrorKind::ProviderUnavailable => {
            ::source_control::SourceControlErrorKind::ProviderUnavailable
        }
        source_control::ErrorKind::RepositoryNotFound => {
            ::source_control::SourceControlErrorKind::RepositoryNotFound
        }
        source_control::ErrorKind::UnsupportedOperation(capability) => {
            let Some(capability) = source_control_capability_from_name(&capability) else {
                return ::source_control::SourceControlError::new(
                    ::source_control::SourceControlErrorKind::MalformedResponse,
                    format!(
                        "source-control extension returned an unknown capability {capability:?}"
                    ),
                );
            };
            ::source_control::SourceControlErrorKind::UnsupportedOperation { capability }
        }
        source_control::ErrorKind::AuthenticationRequired(request) => {
            ::source_control::SourceControlErrorKind::AuthenticationRequired {
                realm: request.realm.into(),
                username: request.username.map(Into::into),
            }
        }
        source_control::ErrorKind::TrustRequired(request) => {
            ::source_control::SourceControlErrorKind::TrustRequired {
                realm: request.realm.into(),
                fingerprint: request.fingerprint.into(),
                failures: request.failures.into_iter().map(Into::into).collect(),
            }
        }
        source_control::ErrorKind::Cancelled => ::source_control::SourceControlErrorKind::Cancelled,
        source_control::ErrorKind::StaleState => {
            ::source_control::SourceControlErrorKind::StaleState
        }
        source_control::ErrorKind::CommandFailed(exit_code) => {
            ::source_control::SourceControlErrorKind::CommandFailed { exit_code }
        }
        source_control::ErrorKind::MalformedResponse => {
            ::source_control::SourceControlErrorKind::MalformedResponse
        }
        source_control::ErrorKind::TransportFailure => {
            ::source_control::SourceControlErrorKind::TransportFailure
        }
        source_control::ErrorKind::InvalidRequest => {
            ::source_control::SourceControlErrorKind::InvalidRequest
        }
    };
    ::source_control::SourceControlError::new(kind, value.message)
}

pub fn source_control_revision_to_wit(
    value: ::source_control::SourceControlRevision,
) -> source_control::Revision {
    source_control::Revision {
        canonical: value.canonical.to_string(),
        display: value.display.to_string(),
    }
}

pub fn source_control_revision_from_wit(
    value: source_control::Revision,
) -> ::source_control::SourceControlRevision {
    ::source_control::SourceControlRevision::new(value.canonical, value.display)
}

fn source_control_paths_to_wit(paths: Vec<::source_control::SourceControlPath>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.as_str().to_owned())
        .collect()
}

fn source_control_capabilities_from_wit(
    value: source_control::Capabilities,
) -> ::source_control::SourceControlCapabilities {
    ::source_control::SourceControlCapabilities {
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

fn source_control_content_status_from_wit(
    value: source_control::ContentStatus,
) -> ::source_control::SourceControlContentStatus {
    match value {
        source_control::ContentStatus::Normal => {
            ::source_control::SourceControlContentStatus::Normal
        }
        source_control::ContentStatus::Modified => {
            ::source_control::SourceControlContentStatus::Modified
        }
        source_control::ContentStatus::Added => ::source_control::SourceControlContentStatus::Added,
        source_control::ContentStatus::Deleted => {
            ::source_control::SourceControlContentStatus::Deleted
        }
        source_control::ContentStatus::Missing => {
            ::source_control::SourceControlContentStatus::Missing
        }
        source_control::ContentStatus::Replaced => {
            ::source_control::SourceControlContentStatus::Replaced
        }
        source_control::ContentStatus::Conflicted => {
            ::source_control::SourceControlContentStatus::Conflicted
        }
        source_control::ContentStatus::Obstructed => {
            ::source_control::SourceControlContentStatus::Obstructed
        }
        source_control::ContentStatus::Unversioned => {
            ::source_control::SourceControlContentStatus::Unversioned
        }
        source_control::ContentStatus::Ignored => {
            ::source_control::SourceControlContentStatus::Ignored
        }
        source_control::ContentStatus::External => {
            ::source_control::SourceControlContentStatus::External
        }
    }
}

fn source_control_capability_from_name(
    value: &str,
) -> Option<::source_control::SourceControlCapability> {
    Some(match value {
        "discovery" => ::source_control::SourceControlCapability::Discovery,
        "status" => ::source_control::SourceControlCapability::Status,
        "base text" | "base-text" => ::source_control::SourceControlCapability::BaseText,
        "diff" => ::source_control::SourceControlCapability::Diff,
        "commit" => ::source_control::SourceControlCapability::Commit,
        "add" => ::source_control::SourceControlCapability::Add,
        "delete" => ::source_control::SourceControlCapability::Delete,
        "revert" => ::source_control::SourceControlCapability::Revert,
        "resolve" => ::source_control::SourceControlCapability::Resolve,
        "ignore" => ::source_control::SourceControlCapability::Ignore,
        "update" => ::source_control::SourceControlCapability::Update,
        "checkout" => ::source_control::SourceControlCapability::Checkout,
        "switch" => ::source_control::SourceControlCapability::Switch,
        "history" => ::source_control::SourceControlCapability::History,
        "blame" => ::source_control::SourceControlCapability::Blame,
        _ => return None,
    })
}

impl From<StartDebuggingRequestArgumentsRequest>
    for extension::StartDebuggingRequestArgumentsRequest
{
    fn from(value: StartDebuggingRequestArgumentsRequest) -> Self {
        match value {
            StartDebuggingRequestArgumentsRequest::Launch => Self::Launch,
            StartDebuggingRequestArgumentsRequest::Attach => Self::Attach,
        }
    }
}
impl TryFrom<dap::StartDebuggingRequestArguments> for extension::StartDebuggingRequestArguments {
    type Error = anyhow::Error;

    fn try_from(value: dap::StartDebuggingRequestArguments) -> Result<Self, Self::Error> {
        Ok(Self {
            configuration: serde_json::from_str(&value.configuration)?,
            request: value.request.into(),
        })
    }
}
impl From<dap::IpAddress> for IpAddr {
    fn from(value: dap::IpAddress) -> Self {
        match value {
            dap::IpAddress::Ipv4((a, b, c, d)) => IpAddr::V4(Ipv4Addr::new(a, b, c, d)),
            dap::IpAddress::Ipv6((a, b, c, d, e, f, g, h)) => {
                IpAddr::V6(Ipv6Addr::new(a, b, c, d, e, f, g, h))
            }
        }
    }
}

impl From<IpAddr> for dap::IpAddress {
    fn from(value: IpAddr) -> Self {
        match value {
            IpAddr::V4(v4) => {
                let [a, b, c, d] = v4.octets();
                Self::Ipv4((a, b, c, d))
            }
            IpAddr::V6(v6) => {
                let [a, b, c, d, e, f, g, h] = v6.segments();
                Self::Ipv6((a, b, c, d, e, f, g, h))
            }
        }
    }
}

impl From<dap::TcpArguments> for extension::TcpArguments {
    fn from(value: dap::TcpArguments) -> Self {
        Self {
            host: value.host.into(),
            port: value.port,
            timeout: value.timeout,
        }
    }
}

impl From<extension::TcpArgumentsTemplate> for dap::TcpArgumentsTemplate {
    fn from(value: extension::TcpArgumentsTemplate) -> Self {
        Self {
            host: value.host.map(Into::into),
            port: value.port,
            timeout: value.timeout,
        }
    }
}

impl From<dap::TcpArgumentsTemplate> for extension::TcpArgumentsTemplate {
    fn from(value: dap::TcpArgumentsTemplate) -> Self {
        Self {
            host: value.host.map(Into::into),
            port: value.port,
            timeout: value.timeout,
        }
    }
}

impl TryFrom<extension::DebugTaskDefinition> for DebugTaskDefinition {
    type Error = anyhow::Error;
    fn try_from(value: extension::DebugTaskDefinition) -> Result<Self, Self::Error> {
        Ok(Self {
            label: value.label.to_string(),
            adapter: value.adapter.to_string(),
            config: value.config.to_string(),
            tcp_connection: value.tcp_connection.map(Into::into),
        })
    }
}

impl From<task::DebugRequest> for DebugRequest {
    fn from(value: task::DebugRequest) -> Self {
        match value {
            task::DebugRequest::Launch(launch_request) => Self::Launch(launch_request.into()),
            task::DebugRequest::Attach(attach_request) => Self::Attach(attach_request.into()),
        }
    }
}

impl From<DebugRequest> for task::DebugRequest {
    fn from(value: DebugRequest) -> Self {
        match value {
            DebugRequest::Launch(launch_request) => Self::Launch(launch_request.into()),
            DebugRequest::Attach(attach_request) => Self::Attach(attach_request.into()),
        }
    }
}

impl From<task::LaunchRequest> for LaunchRequest {
    fn from(value: task::LaunchRequest) -> Self {
        Self {
            program: value.program,
            cwd: value.cwd.map(|p| p.to_string_lossy().into_owned()),
            args: value.args,
            envs: value.env.into_iter().collect(),
        }
    }
}

impl From<task::AttachRequest> for AttachRequest {
    fn from(value: task::AttachRequest) -> Self {
        Self {
            process_id: value.process_id,
        }
    }
}

impl From<LaunchRequest> for task::LaunchRequest {
    fn from(value: LaunchRequest) -> Self {
        Self {
            program: value.program,
            cwd: value.cwd.map(|p| p.into()),
            args: value.args,
            env: value.envs.into_iter().collect(),
        }
    }
}
impl From<AttachRequest> for task::AttachRequest {
    fn from(value: AttachRequest) -> Self {
        Self {
            process_id: value.process_id,
        }
    }
}

impl From<ZedDebugConfig> for DebugConfig {
    fn from(value: ZedDebugConfig) -> Self {
        Self {
            label: value.label.into(),
            adapter: value.adapter.into(),
            request: value.request.into(),
            stop_on_entry: value.stop_on_entry,
        }
    }
}
impl TryFrom<DebugAdapterBinary> for extension::DebugAdapterBinary {
    type Error = anyhow::Error;
    fn try_from(value: DebugAdapterBinary) -> Result<Self, Self::Error> {
        Ok(Self {
            command: value.command,
            arguments: value.arguments,
            envs: value.envs.into_iter().collect(),
            cwd: value.cwd.map(|s| s.into()),
            connection: value.connection.map(Into::into),
            request_args: value.request_args.try_into()?,
        })
    }
}

impl From<dap::BuildTaskDefinition> for extension::BuildTaskDefinition {
    fn from(value: dap::BuildTaskDefinition) -> Self {
        match value {
            dap::BuildTaskDefinition::ByName(name) => Self::ByName(name.into()),
            dap::BuildTaskDefinition::Template(build_task_template) => Self::Template {
                task_template: build_task_template.template.into(),
                locator_name: build_task_template.locator_name.map(SharedString::from),
            },
        }
    }
}

impl From<extension::BuildTaskDefinition> for dap::BuildTaskDefinition {
    fn from(value: extension::BuildTaskDefinition) -> Self {
        match value {
            extension::BuildTaskDefinition::ByName(name) => Self::ByName(name.into()),
            extension::BuildTaskDefinition::Template {
                task_template,
                locator_name,
            } => Self::Template(dap::BuildTaskDefinitionTemplatePayload {
                template: task_template.into(),
                locator_name: locator_name.map(String::from),
            }),
        }
    }
}
impl From<BuildTaskTemplate> for extension::BuildTaskTemplate {
    fn from(value: BuildTaskTemplate) -> Self {
        Self {
            label: value.label,
            command: value.command,
            args: value.args,
            env: value.env.into_iter().collect(),
            cwd: value.cwd,
            ..Default::default()
        }
    }
}
impl From<extension::BuildTaskTemplate> for BuildTaskTemplate {
    fn from(value: extension::BuildTaskTemplate) -> Self {
        Self {
            label: value.label,
            command: value.command,
            args: value.args,
            env: value.env.into_iter().collect(),
            cwd: value.cwd,
        }
    }
}

impl TryFrom<DebugScenario> for extension::DebugScenario {
    type Error = anyhow::Error;

    fn try_from(value: DebugScenario) -> std::result::Result<Self, Self::Error> {
        Ok(Self {
            adapter: value.adapter.into(),
            label: value.label.into(),
            build: value.build.map(Into::into),
            config: serde_json::Value::from_str(&value.config)?,
            tcp_connection: value.tcp_connection.map(Into::into),
        })
    }
}

impl From<extension::DebugScenario> for DebugScenario {
    fn from(value: extension::DebugScenario) -> Self {
        Self {
            adapter: value.adapter.into(),
            label: value.label.into(),
            build: value.build.map(Into::into),
            config: value.config.to_string(),
            tcp_connection: value.tcp_connection.map(Into::into),
        }
    }
}

impl TryFrom<SpawnInTerminal> for ResolvedTask {
    type Error = anyhow::Error;

    fn try_from(value: SpawnInTerminal) -> Result<Self, Self::Error> {
        Ok(Self {
            label: value.label,
            command: value.command.context("missing command")?,
            args: value.args,
            env: value.env.into_iter().collect(),
            cwd: value.cwd.map(|s| {
                let s = s.to_string_lossy();
                if cfg!(target_os = "windows") {
                    s.replace('\\', "/")
                } else {
                    s.into_owned()
                }
            }),
        })
    }
}

impl From<CodeLabel> for extension::CodeLabel {
    fn from(value: CodeLabel) -> Self {
        Self {
            code: value.code,
            spans: value.spans.into_iter().map(Into::into).collect(),
            filter_range: value.filter_range.into(),
        }
    }
}

impl From<CodeLabelSpan> for extension::CodeLabelSpan {
    fn from(value: CodeLabelSpan) -> Self {
        match value {
            CodeLabelSpan::CodeRange(range) => Self::CodeRange(range.into()),
            CodeLabelSpan::Literal(literal) => Self::Literal(literal.into()),
        }
    }
}

impl From<CodeLabelSpanLiteral> for extension::CodeLabelSpanLiteral {
    fn from(value: CodeLabelSpanLiteral) -> Self {
        Self {
            text: value.text,
            highlight_name: value.highlight_name,
        }
    }
}

impl From<extension::Completion> for Completion {
    fn from(value: extension::Completion) -> Self {
        Self {
            label: value.label,
            label_details: value.label_details.map(Into::into),
            detail: value.detail,
            kind: value.kind.map(Into::into),
            insert_text_format: value.insert_text_format.map(Into::into),
        }
    }
}

impl From<extension::CompletionLabelDetails> for CompletionLabelDetails {
    fn from(value: extension::CompletionLabelDetails) -> Self {
        Self {
            detail: value.detail,
            description: value.description,
        }
    }
}

impl From<extension::CompletionKind> for CompletionKind {
    fn from(value: extension::CompletionKind) -> Self {
        match value {
            extension::CompletionKind::Text => Self::Text,
            extension::CompletionKind::Method => Self::Method,
            extension::CompletionKind::Function => Self::Function,
            extension::CompletionKind::Constructor => Self::Constructor,
            extension::CompletionKind::Field => Self::Field,
            extension::CompletionKind::Variable => Self::Variable,
            extension::CompletionKind::Class => Self::Class,
            extension::CompletionKind::Interface => Self::Interface,
            extension::CompletionKind::Module => Self::Module,
            extension::CompletionKind::Property => Self::Property,
            extension::CompletionKind::Unit => Self::Unit,
            extension::CompletionKind::Value => Self::Value,
            extension::CompletionKind::Enum => Self::Enum,
            extension::CompletionKind::Keyword => Self::Keyword,
            extension::CompletionKind::Snippet => Self::Snippet,
            extension::CompletionKind::Color => Self::Color,
            extension::CompletionKind::File => Self::File,
            extension::CompletionKind::Reference => Self::Reference,
            extension::CompletionKind::Folder => Self::Folder,
            extension::CompletionKind::EnumMember => Self::EnumMember,
            extension::CompletionKind::Constant => Self::Constant,
            extension::CompletionKind::Struct => Self::Struct,
            extension::CompletionKind::Event => Self::Event,
            extension::CompletionKind::Operator => Self::Operator,
            extension::CompletionKind::TypeParameter => Self::TypeParameter,
            extension::CompletionKind::Other(value) => Self::Other(value),
        }
    }
}

impl From<extension::InsertTextFormat> for InsertTextFormat {
    fn from(value: extension::InsertTextFormat) -> Self {
        match value {
            extension::InsertTextFormat::PlainText => Self::PlainText,
            extension::InsertTextFormat::Snippet => Self::Snippet,
            extension::InsertTextFormat::Other(value) => Self::Other(value),
        }
    }
}

impl From<extension::Symbol> for Symbol {
    fn from(value: extension::Symbol) -> Self {
        Self {
            kind: value.kind.into(),
            name: value.name,
            container_name: value.container_name,
        }
    }
}

impl From<extension::SymbolKind> for SymbolKind {
    fn from(value: extension::SymbolKind) -> Self {
        match value {
            extension::SymbolKind::File => Self::File,
            extension::SymbolKind::Module => Self::Module,
            extension::SymbolKind::Namespace => Self::Namespace,
            extension::SymbolKind::Package => Self::Package,
            extension::SymbolKind::Class => Self::Class,
            extension::SymbolKind::Method => Self::Method,
            extension::SymbolKind::Property => Self::Property,
            extension::SymbolKind::Field => Self::Field,
            extension::SymbolKind::Constructor => Self::Constructor,
            extension::SymbolKind::Enum => Self::Enum,
            extension::SymbolKind::Interface => Self::Interface,
            extension::SymbolKind::Function => Self::Function,
            extension::SymbolKind::Variable => Self::Variable,
            extension::SymbolKind::Constant => Self::Constant,
            extension::SymbolKind::String => Self::String,
            extension::SymbolKind::Number => Self::Number,
            extension::SymbolKind::Boolean => Self::Boolean,
            extension::SymbolKind::Array => Self::Array,
            extension::SymbolKind::Object => Self::Object,
            extension::SymbolKind::Key => Self::Key,
            extension::SymbolKind::Null => Self::Null,
            extension::SymbolKind::EnumMember => Self::EnumMember,
            extension::SymbolKind::Struct => Self::Struct,
            extension::SymbolKind::Event => Self::Event,
            extension::SymbolKind::Operator => Self::Operator,
            extension::SymbolKind::TypeParameter => Self::TypeParameter,
            extension::SymbolKind::Other(value) => Self::Other(value),
        }
    }
}

impl From<extension::SlashCommand> for SlashCommand {
    fn from(value: extension::SlashCommand) -> Self {
        Self {
            name: value.name,
            description: value.description,
            tooltip_text: value.tooltip_text,
            requires_argument: value.requires_argument,
        }
    }
}

impl From<SlashCommandOutput> for extension::SlashCommandOutput {
    fn from(value: SlashCommandOutput) -> Self {
        Self {
            text: value.text,
            sections: value.sections.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<SlashCommandOutputSection> for extension::SlashCommandOutputSection {
    fn from(value: SlashCommandOutputSection) -> Self {
        Self {
            range: value.range.start as usize..value.range.end as usize,
            label: value.label,
        }
    }
}

impl From<SlashCommandArgumentCompletion> for extension::SlashCommandArgumentCompletion {
    fn from(value: SlashCommandArgumentCompletion) -> Self {
        Self {
            label: value.label,
            new_text: value.new_text,
            run_command: value.run_command,
        }
    }
}

impl TryFrom<ContextServerConfiguration> for extension::ContextServerConfiguration {
    type Error = anyhow::Error;

    fn try_from(value: ContextServerConfiguration) -> Result<Self, Self::Error> {
        let settings_schema: serde_json::Value = serde_json::from_str(&value.settings_schema)
            .context("Failed to parse settings_schema")?;

        Ok(Self {
            installation_instructions: value.installation_instructions,
            default_settings: value.default_settings,
            settings_schema,
        })
    }
}

impl HostKeyValueStore for WasmState {
    async fn insert(
        &mut self,
        kv_store: Resource<ExtensionKeyValueStore>,
        key: String,
        value: String,
    ) -> wasmtime::Result<Result<(), String>> {
        let kv_store = self.table.get(&kv_store)?;
        kv_store.insert(key, value).await.to_wasmtime_result()
    }

    async fn drop(&mut self, _worktree: Resource<ExtensionKeyValueStore>) -> Result<()> {
        // We only ever hand out borrows of key-value stores.
        Ok(())
    }
}

impl HostProject for WasmState {
    async fn worktree_ids(
        &mut self,
        project: Resource<ExtensionProject>,
    ) -> wasmtime::Result<Vec<u64>> {
        let project = self.table.get(&project)?;
        Ok(project.worktree_ids())
    }

    async fn drop(&mut self, _project: Resource<Project>) -> Result<()> {
        // We only ever hand out borrows of projects.
        Ok(())
    }
}

impl HostWorktree for WasmState {
    async fn id(&mut self, delegate: Resource<Arc<dyn WorktreeDelegate>>) -> wasmtime::Result<u64> {
        let delegate = self.table.get(&delegate)?;
        Ok(delegate.id())
    }

    async fn root_path(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
    ) -> wasmtime::Result<String> {
        let delegate = self.table.get(&delegate)?;
        Ok(delegate.root_path())
    }

    async fn read_text_file(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
        path: String,
    ) -> wasmtime::Result<Result<String, String>> {
        let delegate = self.table.get(&delegate)?;
        Ok(delegate
            .read_text_file(&RelPath::new(Path::new(&path), PathStyle::Unix)?)
            .await
            .map_err(|error| error.to_string()))
    }

    async fn shell_env(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
    ) -> wasmtime::Result<EnvVars> {
        let delegate = self.table.get(&delegate)?;
        Ok(delegate.shell_env().await.into_iter().collect())
    }

    async fn which(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
        binary_name: String,
    ) -> wasmtime::Result<Option<String>> {
        let delegate = self.table.get(&delegate)?;
        Ok(delegate.which(binary_name).await)
    }

    async fn run_command(
        &mut self,
        delegate: Resource<Arc<dyn WorktreeDelegate>>,
        command: process::ScopedCommand,
    ) -> wasmtime::Result<Result<process::Output, String>> {
        let delegate = self.table.get(&delegate)?.clone();
        let worktree_root = PathBuf::from(delegate.root_path());
        if let Err(error) = self
            .capability_granter
            .grant_exec(&command.command, &command.args)
        {
            return Ok(Err(error.to_string()));
        }
        let host = self.host.clone();

        maybe!(async move {
            const MAX_STANDARD_INPUT_BYTES: usize = 8 * 1024 * 1024;
            const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

            let requested_directory = if command.working_directory.is_empty() {
                worktree_root.clone()
            } else {
                let requested_directory = PathBuf::from(&command.working_directory);
                if requested_directory.is_absolute() {
                    requested_directory
                } else {
                    worktree_root.join(requested_directory)
                }
            };
            let normalized_directory = util::paths::normalize_lexically(&requested_directory)
                .map_err(|_| anyhow!("working directory escapes its filesystem root"))?;
            let canonical_root = host.fs.canonicalize(&worktree_root).await?;
            let canonical_directory = host.fs.canonicalize(&normalized_directory).await?;
            anyhow::ensure!(
                canonical_directory.starts_with(&canonical_root),
                "working directory must be inside the selected worktree"
            );

            if command
                .stdin
                .as_ref()
                .is_some_and(|stdin| stdin.len() > MAX_STANDARD_INPUT_BYTES)
            {
                bail!("command standard input exceeds the 8 MiB limit");
            }

            let executable = delegate
                .which(command.command.clone())
                .await
                .ok_or_else(|| anyhow!("command is not available in the selected worktree"))?;
            let mut environment = delegate
                .shell_env()
                .await
                .into_iter()
                .filter(|(name, _)| scoped_command_environment_variable_allowed(name))
                .collect::<BTreeMap<_, _>>();
            for (name, value) in command.env {
                anyhow::ensure!(
                    scoped_command_environment_override_allowed(&name),
                    "environment variable {name:?} is not allowed for a scoped command"
                );
                environment.insert(name, value);
            }

            let mut process = util::command::new_command(executable);
            process
                .args(&command.args)
                .env_clear()
                .envs(environment)
                .current_dir(canonical_directory)
                .stdout(util::command::Stdio::piped())
                .stderr(util::command::Stdio::piped())
                .kill_on_drop(true);
            if command.stdin.is_some() {
                process.stdin(util::command::Stdio::piped());
            }
            let mut child = process.spawn()?;
            if let Some(stdin) = command.stdin {
                let stdin = zeroize::Zeroizing::new(stdin);
                let mut child_stdin = child
                    .stdin
                    .take()
                    .context("failed to open command standard input")?;
                child_stdin.write_all(stdin.as_slice()).await?;
                child_stdin.close().await?;
            }
            let stdout = child
                .stdout
                .take()
                .context("failed to open command standard output")?;
            let stderr = child
                .stderr
                .take()
                .context("failed to open command standard error")?;
            let captured_bytes = Arc::new(AtomicUsize::new(0));
            let status = child.status();
            let (stdout, stderr, status) = futures::try_join!(
                read_bounded_command_output(stdout, captured_bytes.clone(), MAX_OUTPUT_BYTES,),
                read_bounded_command_output(stderr, captured_bytes, MAX_OUTPUT_BYTES),
                async { status.await.map_err(anyhow::Error::from) },
            )?;
            let output = std::process::Output {
                status,
                stdout,
                stderr,
            };
            Ok(output.into())
        })
        .await
        .to_wasmtime_result()
    }

    async fn drop(&mut self, _worktree: Resource<Worktree>) -> Result<()> {
        // We only ever hand out borrows of worktrees.
        Ok(())
    }
}

async fn read_bounded_command_output(
    mut reader: impl futures::AsyncRead + Unpin,
    captured_bytes: Arc<AtomicUsize>,
    maximum_bytes: usize,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let bytes_read = reader.read(&mut buffer).await?;
        if bytes_read == 0 {
            return Ok(output);
        }
        let previous_bytes = captured_bytes.fetch_add(bytes_read, Ordering::Relaxed);
        anyhow::ensure!(
            previous_bytes.saturating_add(bytes_read) <= maximum_bytes,
            "command output exceeds the 16 MiB limit"
        );
        output.extend_from_slice(&buffer[..bytes_read]);
    }
}

fn scoped_command_environment_variable_allowed(name: &str) -> bool {
    let uppercase_name = name.to_ascii_uppercase();
    matches!(
        uppercase_name.as_str(),
        "APPDATA"
            | "COMSPEC"
            | "HOME"
            | "LANG"
            | "LANGUAGE"
            | "LC_ALL"
            | "LC_CTYPE"
            | "LOCALAPPDATA"
            | "PATH"
            | "PATHEXT"
            | "SSH_AUTH_SOCK"
            | "SSL_CERT_DIR"
            | "SSL_CERT_FILE"
            | "SVN_ASP_DOT_NET_HACK"
            | "SVN_SSH"
            | "SYSTEMROOT"
            | "TEMP"
            | "TMP"
            | "TMPDIR"
            | "TZ"
            | "USERPROFILE"
            | "WINDIR"
    ) || uppercase_name.starts_with("LC_")
}

fn scoped_command_environment_override_allowed(name: &str) -> bool {
    let uppercase_name = name.to_ascii_uppercase();
    matches!(
        uppercase_name.as_str(),
        "LANG" | "LANGUAGE" | "LC_ALL" | "LC_CTYPE" | "TZ"
    ) || uppercase_name.starts_with("LC_")
}

#[cfg(test)]
mod tests {
    use super::{
        read_bounded_command_output, scoped_command_environment_override_allowed,
        scoped_command_environment_variable_allowed,
    };
    use futures::executor::block_on;
    use std::sync::{Arc, atomic::AtomicUsize};

    #[test]
    fn scoped_command_output_is_bounded_while_reading() -> anyhow::Result<()> {
        let captured_bytes = Arc::new(AtomicUsize::new(0));
        let output = block_on(read_bounded_command_output(
            futures::io::Cursor::new(b"four"),
            captured_bytes.clone(),
            4,
        ))?;
        assert_eq!(output, b"four");

        let error = block_on(read_bounded_command_output(
            futures::io::Cursor::new(b"more"),
            captured_bytes,
            4,
        ))
        .expect_err("combined command output should be bounded");
        assert!(error.to_string().contains("exceeds"));
        Ok(())
    }

    #[test]
    fn scoped_commands_receive_only_explicitly_allowed_environment_variables() {
        for allowed in [
            "HOME",
            "LANG",
            "LC_MESSAGES",
            "Path",
            "SystemRoot",
            "SVN_SSH",
            "TEMP",
        ] {
            assert!(
                scoped_command_environment_variable_allowed(allowed),
                "expected {allowed} to be allowed"
            );
        }

        for denied in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "PASSWORD",
        ] {
            assert!(
                !scoped_command_environment_variable_allowed(denied),
                "expected {denied} to be denied"
            );
        }

        for denied_override in ["HOME", "PATH", "SSH_AUTH_SOCK", "SSL_CERT_FILE", "SVN_SSH"] {
            assert!(
                !scoped_command_environment_override_allowed(denied_override),
                "expected {denied_override} override to be denied"
            );
        }
        for allowed_override in ["LANG", "LC_MESSAGES", "TZ"] {
            assert!(
                scoped_command_environment_override_allowed(allowed_override),
                "expected {allowed_override} override to be allowed"
            );
        }
    }
}

impl common::Host for WasmState {}

impl source_control::Host for WasmState {}

impl http_client::Host for WasmState {
    async fn fetch(
        &mut self,
        request: http_client::HttpRequest,
    ) -> wasmtime::Result<Result<http_client::HttpResponse, String>> {
        maybe!(async {
            let url = &request.url;
            let request = convert_request(&request)?;
            let mut response = self.host.http_client.send(request).await?;

            if response.status().is_client_error() || response.status().is_server_error() {
                bail!("failed to fetch '{url}': status code {}", response.status())
            }
            convert_response(&mut response).await
        })
        .await
        .to_wasmtime_result()
    }

    async fn fetch_stream(
        &mut self,
        request: http_client::HttpRequest,
    ) -> wasmtime::Result<Result<Resource<ExtensionHttpResponseStream>, String>> {
        let request = convert_request(&request)?;
        let response = self.host.http_client.send(request);
        maybe!(async {
            let response = response.await?;
            let stream = Arc::new(Mutex::new(response));
            let resource = self.table.push(stream)?;
            Ok(resource)
        })
        .await
        .to_wasmtime_result()
    }
}

impl http_client::HostHttpResponseStream for WasmState {
    async fn next_chunk(
        &mut self,
        resource: Resource<ExtensionHttpResponseStream>,
    ) -> wasmtime::Result<Result<Option<Vec<u8>>, String>> {
        let stream = self.table.get(&resource)?.clone();
        maybe!(async move {
            let mut response = stream.lock().await;
            let mut buffer = vec![0; 8192]; // 8KB buffer
            let bytes_read = response.body_mut().read(&mut buffer).await?;
            if bytes_read == 0 {
                Ok(None)
            } else {
                buffer.truncate(bytes_read);
                Ok(Some(buffer))
            }
        })
        .await
        .to_wasmtime_result()
    }

    async fn drop(&mut self, _resource: Resource<ExtensionHttpResponseStream>) -> Result<()> {
        Ok(())
    }
}

impl From<http_client::HttpMethod> for ::http_client::Method {
    fn from(value: http_client::HttpMethod) -> Self {
        match value {
            http_client::HttpMethod::Get => Self::GET,
            http_client::HttpMethod::Post => Self::POST,
            http_client::HttpMethod::Put => Self::PUT,
            http_client::HttpMethod::Delete => Self::DELETE,
            http_client::HttpMethod::Head => Self::HEAD,
            http_client::HttpMethod::Options => Self::OPTIONS,
            http_client::HttpMethod::Patch => Self::PATCH,
        }
    }
}

fn convert_request(
    extension_request: &http_client::HttpRequest,
) -> anyhow::Result<::http_client::Request<AsyncBody>> {
    let mut request = ::http_client::Request::builder()
        .method(::http_client::Method::from(extension_request.method))
        .uri(&extension_request.url)
        .follow_redirects(match extension_request.redirect_policy {
            http_client::RedirectPolicy::NoFollow => ::http_client::RedirectPolicy::NoFollow,
            http_client::RedirectPolicy::FollowLimit(limit) => {
                ::http_client::RedirectPolicy::FollowLimit(limit)
            }
            http_client::RedirectPolicy::FollowAll => ::http_client::RedirectPolicy::FollowAll,
        });
    for (key, value) in &extension_request.headers {
        request = request.header(key, value);
    }
    let body = extension_request
        .body
        .clone()
        .map(AsyncBody::from)
        .unwrap_or_default();
    request.body(body).map_err(anyhow::Error::from)
}

async fn convert_response(
    response: &mut ::http_client::Response<AsyncBody>,
) -> anyhow::Result<http_client::HttpResponse> {
    let mut extension_response = http_client::HttpResponse {
        body: Vec::new(),
        headers: Vec::new(),
    };

    for (key, value) in response.headers() {
        extension_response
            .headers
            .push((key.to_string(), value.to_str().unwrap_or("").to_string()));
    }

    response
        .body_mut()
        .read_to_end(&mut extension_response.body)
        .await?;

    Ok(extension_response)
}

impl nodejs::Host for WasmState {
    async fn node_binary_path(&mut self) -> wasmtime::Result<Result<String, String>> {
        self.host
            .node_runtime
            .binary_path()
            .await
            .map(|path| path.to_string_lossy().into_owned())
            .to_wasmtime_result()
    }

    async fn npm_package_latest_version(
        &mut self,
        package_name: String,
    ) -> wasmtime::Result<Result<String, String>> {
        self.host
            .node_runtime
            .npm_package_latest_version(&package_name)
            .await
            .map(|v| v.to_string())
            .to_wasmtime_result()
    }

    async fn npm_package_installed_version(
        &mut self,
        package_name: String,
    ) -> wasmtime::Result<Result<Option<String>, String>> {
        self.host
            .node_runtime
            .npm_package_installed_version(&self.work_dir(), &package_name)
            .await
            .map(|option| option.map(|version| version.to_string()))
            .to_wasmtime_result()
    }

    async fn npm_install_package(
        &mut self,
        package_name: String,
        version: String,
    ) -> wasmtime::Result<Result<(), String>> {
        self.capability_granter
            .grant_npm_install_package(&package_name)?;

        self.host
            .node_runtime
            .npm_install_packages(&self.work_dir(), &[(&package_name, &version)])
            .await
            .to_wasmtime_result()
    }
}

#[async_trait]
impl lsp::Host for WasmState {}

impl From<::http_client::github::GithubRelease> for github::GithubRelease {
    fn from(value: ::http_client::github::GithubRelease) -> Self {
        Self {
            version: value.tag_name,
            assets: value.assets.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<::http_client::github::GithubReleaseAsset> for github::GithubReleaseAsset {
    fn from(value: ::http_client::github::GithubReleaseAsset) -> Self {
        Self {
            name: value.name,
            download_url: value.browser_download_url,
            digest: value.digest,
        }
    }
}

impl github::Host for WasmState {
    async fn latest_github_release(
        &mut self,
        repo: String,
        options: github::GithubReleaseOptions,
    ) -> wasmtime::Result<Result<github::GithubRelease, String>> {
        maybe!(async {
            let release = ::http_client::github::latest_github_release(
                &repo,
                options.require_assets,
                options.pre_release,
                self.host.http_client.clone(),
            )
            .await?;
            Ok(release.into())
        })
        .await
        .to_wasmtime_result()
    }

    async fn github_release_by_tag_name(
        &mut self,
        repo: String,
        tag: String,
    ) -> wasmtime::Result<Result<github::GithubRelease, String>> {
        maybe!(async {
            let release = ::http_client::github::get_release_by_tag_name(
                &repo,
                &tag,
                self.host.http_client.clone(),
            )
            .await?;
            Ok(release.into())
        })
        .await
        .to_wasmtime_result()
    }
}

impl platform::Host for WasmState {
    async fn current_platform(&mut self) -> Result<(platform::Os, platform::Architecture)> {
        Ok((
            match env::consts::OS {
                "macos" => platform::Os::Mac,
                "linux" => platform::Os::Linux,
                "windows" => platform::Os::Windows,
                _ => bail!("unsupported os"),
            },
            match env::consts::ARCH {
                "aarch64" => platform::Architecture::Aarch64,
                "x86_64" => platform::Architecture::X8664,
                _ => bail!("unsupported architecture"),
            },
        ))
    }
}

impl From<std::process::Output> for process::Output {
    fn from(output: std::process::Output) -> Self {
        Self {
            status: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

impl process::Host for WasmState {
    async fn run_command(
        &mut self,
        command: process::Command,
    ) -> wasmtime::Result<Result<process::Output, String>> {
        maybe!(async {
            self.capability_granter
                .grant_exec(&command.command, &command.args)?;

            let output = util::command::new_command(command.command.as_str())
                .args(&command.args)
                .envs(command.env)
                .output()
                .await?;

            Ok(output.into())
        })
        .await
        .to_wasmtime_result()
    }
}

#[async_trait]
impl slash_command::Host for WasmState {}

#[async_trait]
impl context_server::Host for WasmState {}

impl dap::Host for WasmState {
    async fn resolve_tcp_template(
        &mut self,
        template: dap::TcpArgumentsTemplate,
    ) -> wasmtime::Result<Result<dap::TcpArguments, String>> {
        maybe!(async {
            let (host, port, timeout) =
                ::dap::configure_tcp_connection(task::TcpArgumentsTemplate {
                    port: template.port,
                    host: template.host.map(Into::into),
                    timeout: template.timeout,
                })
                .await?;
            Ok(dap::TcpArguments {
                port,
                host: host.into(),
                timeout,
            })
        })
        .await
        .to_wasmtime_result()
    }
}

impl ExtensionImports for WasmState {
    async fn get_settings(
        &mut self,
        location: Option<self::SettingsLocation>,
        category: String,
        key: Option<String>,
    ) -> wasmtime::Result<Result<String, String>> {
        self.on_main_thread(|cx| {
            async move {
                let path = location.as_ref().and_then(|location| {
                    RelPath::new(Path::new(&location.path), PathStyle::Unix).ok()
                });
                let location = path
                    .as_ref()
                    .zip(location.as_ref())
                    .map(|(path, location)| ::settings::SettingsLocation {
                        worktree_id: WorktreeId::from_proto(location.worktree_id),
                        path,
                    });

                cx.update(|cx| match category.as_str() {
                    "language" => {
                        let key = key.map(|k| LanguageName::new(&k));
                        let settings = AllLanguageSettings::get(location, cx).language(
                            location,
                            key.as_ref(),
                            cx,
                        );
                        Ok(serde_json::to_string(&settings::LanguageSettings {
                            tab_size: settings.tab_size,
                            hard_tabs: settings.hard_tabs,
                            preferred_line_length: settings.preferred_line_length,
                        })?)
                    }
                    "lsp" => {
                        let settings = key
                            .and_then(|key| {
                                ProjectSettings::get(location, cx)
                                    .lsp
                                    .get(&::lsp::LanguageServerName::from_proto(key))
                            })
                            .cloned()
                            .unwrap_or_default();
                        Ok(serde_json::to_string(&settings::LspSettings {
                            binary: settings.binary.map(|binary| settings::CommandSettings {
                                path: binary.path,
                                arguments: binary.arguments,
                                env: binary.env.map(|env| env.into_iter().collect()),
                            }),
                            settings: settings.settings,
                            initialization_options: settings.initialization_options,
                        })?)
                    }
                    "context_servers" => {
                        let settings = key
                            .and_then(|key| {
                                ProjectSettings::get(location, cx)
                                    .context_servers
                                    .get(key.as_str())
                            })
                            .cloned()
                            .unwrap_or_else(|| {
                                project::project_settings::ContextServerSettings::default_extension(
                                )
                            });

                        match settings {
                            project::project_settings::ContextServerSettings::Stdio {
                                enabled: _,
                                command,
                                ..
                            } => Ok(serde_json::to_string(&settings::ContextServerSettings {
                                command: Some(settings::CommandSettings {
                                    path: command.path.to_str().map(|path| path.to_string()),
                                    arguments: Some(command.args),
                                    env: command.env.map(|env| env.into_iter().collect()),
                                }),
                                settings: None,
                            })?),
                            project::project_settings::ContextServerSettings::Extension {
                                enabled: _,
                                settings,
                                ..
                            } => Ok(serde_json::to_string(&settings::ContextServerSettings {
                                command: None,
                                settings: Some(settings),
                            })?),
                            project::project_settings::ContextServerSettings::Http { .. } => {
                                bail!("remote context server settings not supported in 0.6.0")
                            }
                        }
                    }
                    _ => {
                        bail!("Unknown settings category: {}", category);
                    }
                })
            }
            .boxed_local()
        })
        .await
        .to_wasmtime_result()
    }

    async fn set_language_server_installation_status(
        &mut self,
        server_name: String,
        status: LanguageServerInstallationStatus,
    ) -> wasmtime::Result<()> {
        let status = match status {
            LanguageServerInstallationStatus::CheckingForUpdate => BinaryStatus::CheckingForUpdate,
            LanguageServerInstallationStatus::Downloading => BinaryStatus::Downloading,
            LanguageServerInstallationStatus::None => BinaryStatus::None,
            LanguageServerInstallationStatus::Failed(error) => BinaryStatus::Failed { error },
        };

        self.host
            .proxy
            .update_language_server_status(::lsp::LanguageServerName(server_name.into()), status);

        Ok(())
    }

    async fn download_file(
        &mut self,
        url: String,
        path: String,
        file_type: DownloadedFileType,
    ) -> wasmtime::Result<Result<(), String>> {
        maybe!(async {
            let parsed_url = Url::parse(&url)?;
            self.capability_granter.grant_download_file(&parsed_url)?;

            let path = PathBuf::from(path);
            let extension_work_dir = self.host.work_dir.join(self.manifest.id.as_ref());

            self.host.fs.create_dir(&extension_work_dir).await?;

            let destination_path = self
                .host
                .writeable_path_from_extension(&self.manifest.id, &path)
                .await?;

            let mut response = self
                .host
                .http_client
                .get(&url, Default::default(), true)
                .await
                .context("downloading release")?;

            anyhow::ensure!(
                response.status().is_success(),
                "download failed with status {}",
                response.status()
            );
            let mut body = BufReader::new(response.body_mut());

            match file_type {
                DownloadedFileType::Uncompressed => {
                    futures::pin_mut!(body);
                    self.host
                        .fs
                        .create_file_with(&destination_path, body)
                        .await?;
                }
                DownloadedFileType::Gzip => {
                    let body = GzipDecoder::new(body);
                    futures::pin_mut!(body);
                    self.host
                        .fs
                        .create_file_with(&destination_path, body)
                        .await?;
                }
                DownloadedFileType::GzipTar => {
                    let mut tar_gz_bytes = Vec::new();
                    body.read_to_end(&mut tar_gz_bytes).await?;
                    let decompressed_bytes =
                        GzipDecoder::new(BufReader::new(tar_gz_bytes.as_slice()));
                    futures::pin_mut!(decompressed_bytes);
                    self.host
                        .fs
                        .extract_tar_file(&destination_path, Archive::new(decompressed_bytes))
                        .await?;
                }
                DownloadedFileType::Zip => {
                    futures::pin_mut!(body);
                    extract_zip(&destination_path, body)
                        .await
                        .with_context(|| format!("unzipping {path:?} archive"))?;
                }
            }

            Ok(())
        })
        .await
        .to_wasmtime_result()
    }

    async fn make_file_executable(&mut self, path: String) -> wasmtime::Result<Result<(), String>> {
        let path = self
            .host
            .writeable_path_from_extension(&self.manifest.id, Path::new(&path))
            .await?;

        make_file_executable(&path)
            .await
            .with_context(|| format!("setting permissions for path {path:?}"))
            .to_wasmtime_result()
    }
}
