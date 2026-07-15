use crate::protocol::{
    decode_message, error_response, notification, object, request, result_response,
    IncomingMessage, ProtocolError, ResponseBody,
};
mod io;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use euler_sdk::{
    managed_process_entrypoint_from_manifest_bytes, AgentOutcome, ArtifactRecord, ArtifactWrite,
    Capability, CommandContext, CommandDescriptor, CommandRegistrar, DiagnosticsPage,
    EventFeedCheckpoint, Extension, ExtensionCommand, ExtensionError, ExtensionManifest,
    HostAgentRecord, HostAgentResult, HostAgentTask, HostApi, LoadedExtensionPackage,
    ManagedProcessEntrypoint, ProvenancePage, SpawnAgentTask, StaticExtensionDescriptor,
};
use io::{finish_io_thread, spawn_stderr_drain, spawn_stdin_writer, spawn_stdout_reader, IoThread};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::env;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub const MANAGED_PROCESS_PROTOCOL_VERSION: &str = "euler-managed-process/1";

const MAX_PENDING_MESSAGES: usize = 32;
const MAX_PENDING_OUTBOUND_MESSAGES: usize = 8;
const RECEIVE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Host-owned ceilings for one managed-process command invocation.
///
/// The manifest cannot raise these limits. Tests may choose smaller values to
/// prove cancellation and output-bound behavior without waiting for defaults.
#[derive(Clone, Debug)]
pub struct ManagedProcessLimits {
    pub handshake_timeout: Duration,
    pub invocation_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub cancel_grace: Duration,
    pub max_message_bytes: usize,
    pub max_protocol_messages: usize,
    pub max_protocol_bytes: usize,
    pub max_host_requests: usize,
    pub max_stderr_bytes: usize,
    pub max_progress_messages: usize,
    pub max_progress_bytes: usize,
}

impl Default for ManagedProcessLimits {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(5),
            invocation_timeout: Duration::from_secs(60),
            shutdown_timeout: Duration::from_secs(2),
            cancel_grace: Duration::from_millis(250),
            max_message_bytes: 1024 * 1024,
            max_protocol_messages: 512,
            max_protocol_bytes: 4 * 1024 * 1024,
            max_host_requests: 64,
            max_stderr_bytes: 64 * 1024,
            max_progress_messages: 128,
            max_progress_bytes: 32 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ManagedProcessRuntimeError {
    #[error("managed-process extension descriptor is invalid")]
    InvalidDescriptor,
    #[error("managed-process extension could not start")]
    LaunchFailed,
    #[error("managed-process extension violated the protocol")]
    ProtocolViolation,
    #[error("managed-process extension exceeded the protocol output limit")]
    OutputLimitExceeded,
    #[error("managed-process extension did not complete its handshake in time")]
    HandshakeTimedOut,
    #[error("managed-process extension did not complete its command in time")]
    InvocationTimedOut,
    #[error("managed-process extension did not shut down cleanly")]
    ShutdownTimedOut,
    #[error("managed-process extension command failed")]
    CommandFailed,
}

impl ManagedProcessRuntimeError {
    fn into_extension_error(self) -> ExtensionError {
        ExtensionError::Message(self.to_string())
    }
}

/// An extension adapter created from a reviewed `managed-process` package
/// descriptor. It uses the same `Extension` and `HostApi` contract as native
/// Rust extensions.
#[derive(Clone, Debug)]
pub struct ManagedProcessExtension {
    package_dir: PathBuf,
    manifest: ExtensionManifest,
    entrypoint: ManagedProcessEntrypoint,
    commands: Vec<CommandDescriptor>,
    limits: ManagedProcessLimits,
}

impl ManagedProcessExtension {
    /// Build an adapter from one validated package, preserving the manifest as
    /// the single source of truth for both the static descriptor and argv.
    pub fn from_package(
        package: &LoadedExtensionPackage,
    ) -> Result<Self, ManagedProcessRuntimeError> {
        let entrypoint = managed_process_entrypoint_from_manifest_bytes(&package.manifest_bytes)
            .map_err(|_| ManagedProcessRuntimeError::InvalidDescriptor)?;
        Self::new(
            package.canonical_dir.clone(),
            &package.descriptor,
            entrypoint,
        )
    }

    pub fn new(
        package_dir: impl Into<PathBuf>,
        descriptor: &StaticExtensionDescriptor,
        entrypoint: ManagedProcessEntrypoint,
    ) -> Result<Self, ManagedProcessRuntimeError> {
        let package_dir = package_dir.into();
        if descriptor.runtime_kind != "managed-process" {
            return Err(ManagedProcessRuntimeError::InvalidDescriptor);
        }
        if entrypoint.command.is_empty() || !package_dir.is_dir() {
            return Err(ManagedProcessRuntimeError::InvalidDescriptor);
        }

        let capabilities = parse_capabilities(&descriptor.capabilities)?;
        let commands = descriptor
            .commands
            .iter()
            .map(|command| {
                Ok(CommandDescriptor {
                    name: command.name.clone(),
                    display_name: command.display_name.clone(),
                    summary: command.summary.clone(),
                    required_capabilities: parse_capabilities(&command.required_capabilities)?,
                    // Process commands receive their session-scoped context
                    // through host APIs; there is no hidden path/session-id
                    // injection into their arbitrary JSON input.
                    accepts_session_id: false,
                    args: Vec::new(),
                    invocation: command.invocation,
                })
            })
            .collect::<Result<Vec<_>, ManagedProcessRuntimeError>>()?;
        if commands.is_empty() {
            return Err(ManagedProcessRuntimeError::InvalidDescriptor);
        }

        Ok(Self {
            package_dir,
            manifest: ExtensionManifest {
                id: descriptor.id.clone(),
                version: descriptor.version.clone(),
                display_name: descriptor.display_name.clone(),
                capabilities,
            },
            entrypoint,
            commands,
            limits: ManagedProcessLimits::default(),
        })
    }

    #[must_use]
    pub fn with_limits(mut self, limits: ManagedProcessLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn command_descriptor(&self, name: &str) -> Option<&CommandDescriptor> {
        self.commands.iter().find(|command| command.name == name)
    }
}

impl Extension for ManagedProcessExtension {
    fn manifest(&self) -> ExtensionManifest {
        self.manifest.clone()
    }

    fn register(&self, registrar: &mut dyn CommandRegistrar) -> Result<(), ExtensionError> {
        for descriptor in &self.commands {
            registrar.register_command(
                &descriptor.name,
                Box::new(ManagedProcessCommand {
                    descriptor: descriptor.clone(),
                    package_dir: self.package_dir.clone(),
                    extension_id: self.manifest.id.clone(),
                    extension_version: self.manifest.version.clone(),
                    entrypoint: self.entrypoint.clone(),
                    limits: self.limits.clone(),
                }),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct ManagedProcessCommand {
    descriptor: CommandDescriptor,
    package_dir: PathBuf,
    extension_id: String,
    extension_version: String,
    entrypoint: ManagedProcessEntrypoint,
    limits: ManagedProcessLimits,
}

impl ExtensionCommand for ManagedProcessCommand {
    fn descriptor(&self) -> CommandDescriptor {
        self.descriptor.clone()
    }

    fn execute(
        &self,
        context: CommandContext,
        host: &dyn HostApi,
    ) -> Result<Value, ExtensionError> {
        execute_managed_process(
            ProcessInvocation {
                package_dir: &self.package_dir,
                entrypoint: &self.entrypoint,
                extension_id: &self.extension_id,
                extension_version: &self.extension_version,
                command_name: &self.descriptor.name,
                input: context.input,
                limits: &self.limits,
            },
            host,
        )
        .map_err(ManagedProcessRuntimeError::into_extension_error)
    }
}

fn parse_capabilities(values: &[String]) -> Result<Vec<Capability>, ManagedProcessRuntimeError> {
    values
        .iter()
        .map(|value| Capability::parse(value).ok_or(ManagedProcessRuntimeError::InvalidDescriptor))
        .collect()
}

struct ProcessInvocation<'a> {
    package_dir: &'a Path,
    entrypoint: &'a ManagedProcessEntrypoint,
    extension_id: &'a str,
    extension_version: &'a str,
    command_name: &'a str,
    input: Value,
    limits: &'a ManagedProcessLimits,
}

fn execute_managed_process(
    invocation: ProcessInvocation<'_>,
    host: &dyn HostApi,
) -> Result<Value, ManagedProcessRuntimeError> {
    let mut process = RunningProcess::spawn(
        invocation.package_dir,
        invocation.entrypoint,
        invocation.limits,
    )?;
    let outcome = run_protocol(
        &mut process,
        invocation.extension_id,
        invocation.extension_version,
        invocation.command_name,
        invocation.input,
        host,
    );
    match outcome {
        Ok(result) => Ok(result),
        Err(error) => {
            process.abort();
            Err(error)
        }
    }
}

fn run_protocol(
    process: &mut RunningProcess,
    extension_id: &str,
    extension_version: &str,
    command_name: &str,
    input: Value,
    host: &dyn HostApi,
) -> Result<Value, ManagedProcessRuntimeError> {
    let initialize_id = Value::String("euler-initialize-1".to_owned());
    process.send(request(
        initialize_id.clone(),
        "initialize",
        json!({
            "protocol_versions": [MANAGED_PROCESS_PROTOCOL_VERSION],
            "extension": {"id": extension_id, "version": extension_version},
            "limits": {"max_message_bytes": process.limits.max_message_bytes},
        }),
    ))?;
    let initialized = process.await_response(
        &initialize_id,
        Instant::now() + process.limits.handshake_timeout,
        CallPhase::Handshake,
        host,
    )?;
    validate_initialize_result(&initialized)?;
    process.send(notification("initialized", Value::Object(Map::new())))?;

    let command_id = Value::String("euler-command-1".to_owned());
    process.invocation_started = true;
    process.send(request(
        command_id.clone(),
        "euler/command",
        json!({"command": command_name, "input": input}),
    ))?;
    let result = process.await_response(
        &command_id,
        Instant::now() + process.limits.invocation_timeout,
        CallPhase::Invocation,
        host,
    )?;
    if !result.is_object() {
        return Err(ManagedProcessRuntimeError::ProtocolViolation);
    }

    let shutdown_id = Value::String("euler-shutdown-1".to_owned());
    process.send(request(
        shutdown_id.clone(),
        "shutdown",
        Value::Object(Map::new()),
    ))?;
    let _shutdown = process.await_response(
        &shutdown_id,
        Instant::now() + process.limits.shutdown_timeout,
        CallPhase::Shutdown,
        host,
    )?;
    process.send(notification("exit", Value::Object(Map::new())))?;
    process.close_cleanly()?;
    Ok(result)
}

fn validate_initialize_result(value: &Value) -> Result<(), ManagedProcessRuntimeError> {
    let object = value
        .as_object()
        .ok_or(ManagedProcessRuntimeError::ProtocolViolation)?;
    if object.get("protocol_version").and_then(Value::as_str)
        == Some(MANAGED_PROCESS_PROTOCOL_VERSION)
    {
        Ok(())
    } else {
        Err(ManagedProcessRuntimeError::ProtocolViolation)
    }
}

struct RunningProcess {
    child: Child,
    outbound: Option<SyncSender<Vec<u8>>>,
    receiver: Receiver<Result<IncomingMessage, ProtocolError>>,
    stdout_limit_reached: Arc<AtomicBool>,
    stderr_limit_reached: Arc<AtomicBool>,
    writer_failed: Arc<AtomicBool>,
    writer_thread: Option<IoThread>,
    stdout_thread: Option<IoThread>,
    stderr_thread: Option<IoThread>,
    limits: ManagedProcessLimits,
    progress: ProgressBudget,
    host_requests: usize,
    invocation_started: bool,
    finished: bool,
}

impl RunningProcess {
    fn spawn(
        package_dir: &Path,
        entrypoint: &ManagedProcessEntrypoint,
        limits: &ManagedProcessLimits,
    ) -> Result<Self, ManagedProcessRuntimeError> {
        let Some(executable) = entrypoint.command.first() else {
            return Err(ManagedProcessRuntimeError::InvalidDescriptor);
        };
        let mut command = Command::new(executable);
        configure_process_group(&mut command);
        command
            .args(entrypoint.command.iter().skip(1))
            .current_dir(package_dir)
            .env_clear()
            .env(
                "EULER_MANAGED_PROCESS_PROTOCOL",
                MANAGED_PROCESS_PROTOCOL_VERSION,
            )
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = env::var_os("PATH") {
            command.env("PATH", path);
        }
        let mut child = command
            .spawn()
            .map_err(|_| ManagedProcessRuntimeError::LaunchFailed)?;
        let (stdin, stdout, stderr) = take_child_pipes(&mut child)?;
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_MESSAGES);
        let (outbound, outbound_receiver) = mpsc::sync_channel(MAX_PENDING_OUTBOUND_MESSAGES);
        let stdout_limit_reached = Arc::new(AtomicBool::new(false));
        let stderr_limit_reached = Arc::new(AtomicBool::new(false));
        let writer_failed = Arc::new(AtomicBool::new(false));
        let writer_thread = Some(spawn_stdin_writer(
            stdin,
            outbound_receiver,
            Arc::clone(&writer_failed),
        ));
        let stdout_thread = Some(spawn_stdout_reader(
            stdout,
            sender,
            Arc::clone(&stdout_limit_reached),
            limits.max_message_bytes,
            limits.max_protocol_messages,
            limits.max_protocol_bytes,
        ));
        let stderr_thread = Some(spawn_stderr_drain(
            stderr,
            Arc::clone(&stderr_limit_reached),
            limits.max_stderr_bytes,
        ));

        Ok(Self {
            child,
            outbound: Some(outbound),
            receiver,
            stdout_limit_reached,
            stderr_limit_reached,
            writer_failed,
            writer_thread,
            stdout_thread,
            stderr_thread,
            limits: limits.clone(),
            progress: ProgressBudget::default(),
            host_requests: 0,
            invocation_started: false,
            finished: false,
        })
    }

    fn send(&mut self, message: Value) -> Result<(), ManagedProcessRuntimeError> {
        let bytes = serde_json::to_vec(&message)
            .map_err(|_| ManagedProcessRuntimeError::ProtocolViolation)?;
        if bytes.len() > self.limits.max_message_bytes {
            return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
        }
        if self.writer_failed.load(Ordering::Relaxed) {
            return Err(ManagedProcessRuntimeError::CommandFailed);
        }
        let outbound = self
            .outbound
            .as_ref()
            .ok_or(ManagedProcessRuntimeError::CommandFailed)?;
        outbound.try_send(bytes).map_err(|error| match error {
            TrySendError::Full(_) | TrySendError::Disconnected(_) => {
                ManagedProcessRuntimeError::CommandFailed
            }
        })
    }

    fn await_response(
        &mut self,
        expected_id: &Value,
        deadline: Instant,
        phase: CallPhase,
        host: &dyn HostApi,
    ) -> Result<Value, ManagedProcessRuntimeError> {
        loop {
            if self.stdout_limit_reached.load(Ordering::Relaxed) {
                return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
            }
            if self.stderr_limit_reached.load(Ordering::Relaxed) {
                return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
            }
            if self.writer_failed.load(Ordering::Relaxed) {
                return Err(ManagedProcessRuntimeError::CommandFailed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(phase.timeout_error());
            }
            let wait = deadline
                .saturating_duration_since(now)
                .min(RECEIVE_POLL_INTERVAL);
            match self.receiver.recv_timeout(wait) {
                Ok(Ok(IncomingMessage::Request { id, method, params })) => {
                    if !phase.accepts_peer_activity() {
                        return Err(ManagedProcessRuntimeError::ProtocolViolation);
                    }
                    self.reply_to_host_request(id, &method, params, host)?;
                    if Instant::now() >= deadline {
                        return Err(phase.timeout_error());
                    }
                }
                Ok(Ok(IncomingMessage::Notification { method, params })) => {
                    if !phase.accepts_peer_activity() {
                        return Err(ManagedProcessRuntimeError::ProtocolViolation);
                    }
                    self.observe_notification(&method, params)?;
                }
                Ok(Ok(IncomingMessage::Response { id, body })) if id == *expected_id => {
                    return match body {
                        ResponseBody::Result(result) => Ok(result),
                        ResponseBody::Error => Err(ManagedProcessRuntimeError::CommandFailed),
                    };
                }
                Ok(Ok(IncomingMessage::Response { .. })) => {
                    return Err(ManagedProcessRuntimeError::ProtocolViolation);
                }
                Ok(Err(_)) => return Err(ManagedProcessRuntimeError::ProtocolViolation),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    if self.stdout_limit_reached.load(Ordering::Relaxed)
                        || self.stderr_limit_reached.load(Ordering::Relaxed)
                    {
                        return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
                    }
                    return Err(ManagedProcessRuntimeError::CommandFailed);
                }
            }
        }
    }

    fn reply_to_host_request(
        &mut self,
        id: Value,
        method: &str,
        params: Value,
        host: &dyn HostApi,
    ) -> Result<(), ManagedProcessRuntimeError> {
        if self.host_requests >= self.limits.max_host_requests {
            return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
        }
        self.host_requests = self.host_requests.saturating_add(1);
        let response = match dispatch_host_request(method, params, host) {
            Ok(result) => result_response(id, result),
            Err(HostRequestError::CapabilityDenied(capability)) => error_response(
                id,
                -32010,
                &format!("host capability denied: {}", capability.as_str()),
            ),
            Err(HostRequestError::InvalidParams) => {
                error_response(id, -32602, "invalid host request parameters")
            }
            Err(HostRequestError::MethodNotFound) => {
                error_response(id, -32601, "unknown host method")
            }
            Err(HostRequestError::Failed) => error_response(id, -32000, "host operation failed"),
        };
        self.send(response)
    }

    fn observe_notification(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<(), ManagedProcessRuntimeError> {
        if method != "euler/progress" {
            return Err(ManagedProcessRuntimeError::ProtocolViolation);
        }
        let params = object(params).map_err(|_| ManagedProcessRuntimeError::ProtocolViolation)?;
        reject_unknown_fields(&params, &["message", "fraction"])?;
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .filter(|message| !message.is_empty() && message.len() <= 4096)
            .ok_or(ManagedProcessRuntimeError::ProtocolViolation)?;
        if let Some(fraction) = params.get("fraction") {
            let fraction = fraction
                .as_f64()
                .filter(|fraction| (0.0..=1.0).contains(fraction))
                .ok_or(ManagedProcessRuntimeError::ProtocolViolation)?;
            if !fraction.is_finite() {
                return Err(ManagedProcessRuntimeError::ProtocolViolation);
            }
        }
        self.progress.record(message, &self.limits)
    }

    fn close_cleanly(&mut self) -> Result<(), ManagedProcessRuntimeError> {
        self.close_input();
        self.wait_for_exit(Instant::now() + self.limits.shutdown_timeout)?;
        if self.output_limit_reached() {
            return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
        }
        // A managed extension owns its process group. A normal child exits
        // after `exit`; anything still holding the protocol pipes is an
        // orphaned descendant and must not keep Euler's cleanup blocked.
        self.force_stop_process_group();
        self.finish_io();
        self.finished = true;
        Ok(())
    }

    fn abort(&mut self) {
        if self.finished {
            return;
        }
        if self.invocation_started {
            let _ = self.send(notification(
                "$/cancelRequest",
                json!({"id": "euler-command-1"}),
            ));
        }
        self.close_input();
        let deadline = Instant::now() + self.limits.cancel_grace;
        self.wait_for_child_until(deadline);
        self.request_process_group_stop();
        self.force_stop_process_group();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.finish_io();
        self.finished = true;
    }

    fn close_input(&mut self) {
        self.outbound.take();
    }

    fn output_limit_reached(&self) -> bool {
        self.stdout_limit_reached.load(Ordering::Relaxed)
            || self.stderr_limit_reached.load(Ordering::Relaxed)
    }

    fn wait_for_child_until(&mut self, deadline: Instant) {
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn wait_for_exit(&mut self, deadline: Instant) -> Result<(), ManagedProcessRuntimeError> {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) | Err(_) => return Err(ManagedProcessRuntimeError::CommandFailed),
                Ok(None) if Instant::now() >= deadline => {
                    return Err(ManagedProcessRuntimeError::ShutdownTimedOut);
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn request_process_group_stop(&self) {
        #[cfg(unix)]
        signal_process_group(&self.child, libc::SIGTERM);
    }

    fn force_stop_process_group(&self) {
        #[cfg(unix)]
        signal_process_group(&self.child, libc::SIGKILL);
    }

    fn finish_io(&mut self) {
        let deadline = Instant::now() + self.limits.cancel_grace;
        finish_io_thread(&mut self.writer_thread, deadline);
        finish_io_thread(&mut self.stdout_thread, deadline);
        finish_io_thread(&mut self.stderr_thread, deadline);
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn signal_process_group(child: &Child, signal: libc::c_int) {
    let process_group = -(child.id() as libc::pid_t);
    // The child is launched in its own group, so this reaches ordinary
    // descendants that inherited the protocol pipe handles. Errors are
    // intentionally ignored: an already-empty group is the desired state.
    unsafe {
        libc::kill(process_group, signal);
    }
}

fn take_child_pipes(
    child: &mut Child,
) -> Result<(ChildStdin, ChildStdout, ChildStderr), ManagedProcessRuntimeError> {
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    match (stdin, stdout, stderr) {
        (Some(stdin), Some(stdout), Some(stderr)) => Ok((stdin, stdout, stderr)),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            Err(ManagedProcessRuntimeError::LaunchFailed)
        }
    }
}

#[derive(Default)]
struct ProgressBudget {
    messages: usize,
    bytes: usize,
}

impl ProgressBudget {
    fn record(
        &mut self,
        message: &str,
        limits: &ManagedProcessLimits,
    ) -> Result<(), ManagedProcessRuntimeError> {
        let next_messages = self.messages.saturating_add(1);
        let next_bytes = self.bytes.saturating_add(message.len());
        if next_messages > limits.max_progress_messages || next_bytes > limits.max_progress_bytes {
            return Err(ManagedProcessRuntimeError::OutputLimitExceeded);
        }
        self.messages = next_messages;
        self.bytes = next_bytes;
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum CallPhase {
    Handshake,
    Invocation,
    Shutdown,
}

impl CallPhase {
    fn accepts_peer_activity(self) -> bool {
        matches!(self, Self::Invocation)
    }

    fn timeout_error(self) -> ManagedProcessRuntimeError {
        match self {
            Self::Handshake => ManagedProcessRuntimeError::HandshakeTimedOut,
            Self::Invocation => ManagedProcessRuntimeError::InvocationTimedOut,
            Self::Shutdown => ManagedProcessRuntimeError::ShutdownTimedOut,
        }
    }
}

#[derive(Debug)]
enum HostRequestError {
    CapabilityDenied(Capability),
    InvalidParams,
    MethodNotFound,
    Failed,
}

fn dispatch_host_request(
    method: &str,
    params: Value,
    host: &dyn HostApi,
) -> Result<Value, HostRequestError> {
    match method {
        "euler/host/query-provenance" => {
            let query = decode(params)?;
            let page: ProvenancePage = host_result(host.query_provenance(query))?;
            to_json(page)
        }
        "euler/host/read-diagnostics" => {
            let query = decode(params)?;
            let page: DiagnosticsPage = host_result(host.read_diagnostics(query))?;
            to_json(page)
        }
        "euler/host/state-dir" => {
            require_empty_params(params)?;
            let path = host_result(host.state_dir())?;
            Ok(json!({"path": path.to_string_lossy()}))
        }
        "euler/host/write-artifact" => write_artifact(params, host),
        "euler/host/load-checkpoint" => {
            let request: CheckpointName = decode(params)?;
            let checkpoint = host_result(host.load_event_feed_checkpoint(&request.name))?;
            to_json(checkpoint)
        }
        "euler/host/store-checkpoint" => {
            let request: StoreCheckpoint = decode(params)?;
            host_result(host.store_event_feed_checkpoint(&request.name, request.checkpoint))?;
            Ok(Value::Object(Map::new()))
        }
        "euler/host/record-agent-task-result" => {
            let request: AgentTaskResult = decode(params)?;
            let record: HostAgentRecord =
                host_result(host.record_agent_task_result(request.task, request.result))?;
            to_json(record)
        }
        "euler/host/update-context-slot" => {
            let request: ContextSlotUpdate = decode(params)?;
            host_result(host.update_context_slot(&request.slot, &request.content))?;
            Ok(Value::Object(Map::new()))
        }
        "euler/host/spawn-agent" => {
            let task: SpawnAgentTask = decode(params)?;
            let outcome: AgentOutcome = host_result(host.spawn_agent(task))?;
            to_json(outcome)
        }
        "euler/host/spawn-agents" => {
            let tasks: SpawnAgentTasks = decode(params)?;
            let outcomes: Vec<AgentOutcome> = host_result(host.spawn_agents(tasks.tasks))?;
            to_json(outcomes)
        }
        _ => Err(HostRequestError::MethodNotFound),
    }
}

fn write_artifact(params: Value, host: &dyn HostApi) -> Result<Value, HostRequestError> {
    let request: WireArtifactWrite = decode(params)?;
    let bytes = BASE64
        .decode(request.bytes_base64)
        .map_err(|_| HostRequestError::InvalidParams)?;
    let artifact = ArtifactWrite {
        display_name: request.display_name,
        media_type: request.media_type,
        bytes,
        source_event_ids: request.source_event_ids,
        metadata: request.metadata,
    };
    let record: ArtifactRecord = host_result(host.write_artifact(artifact))?;
    to_json(record)
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, HostRequestError> {
    serde_json::from_value(value).map_err(|_| HostRequestError::InvalidParams)
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, HostRequestError> {
    serde_json::to_value(value).map_err(|_| HostRequestError::Failed)
}

fn host_result<T>(result: Result<T, ExtensionError>) -> Result<T, HostRequestError> {
    result.map_err(|error| match error {
        ExtensionError::CapabilityDenied { capability } => {
            HostRequestError::CapabilityDenied(capability)
        }
        _ => HostRequestError::Failed,
    })
}

fn require_empty_params(params: Value) -> Result<(), HostRequestError> {
    match params {
        Value::Null => Ok(()),
        Value::Object(object) if object.is_empty() => Ok(()),
        _ => Err(HostRequestError::InvalidParams),
    }
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), ManagedProcessRuntimeError> {
    if object.keys().all(|key| allowed.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(ManagedProcessRuntimeError::ProtocolViolation)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireArtifactWrite {
    display_name: String,
    media_type: String,
    bytes_base64: String,
    #[serde(default)]
    source_event_ids: Vec<String>,
    #[serde(default)]
    metadata: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointName {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreCheckpoint {
    name: String,
    checkpoint: EventFeedCheckpoint,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTaskResult {
    task: HostAgentTask,
    result: HostAgentResult,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextSlotUpdate {
    slot: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentTasks {
    tasks: Vec<SpawnAgentTask>,
}
