use cladding::error::{Error, Result};
use cladding::identity::invoking_uid;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read as _, Write};
use std::os::unix::fs::{
    DirBuilderExt as _, FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _,
    PermissionsExt as _,
};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: u8 = 2;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_ERROR_BYTES: usize = 8 * 1024;
const SOCKET_MODE: u32 = 0o600;
const LOCK_MODE: u32 = 0o600;
const STATUS_STARTING: u8 = 0;
const STATUS_READY: u8 = 1;
const STATUS_STOPPING: u8 = 2;
const STATUS_STOPPED: u8 = 3;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u8,
    uuid: String,
    operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Status,
    Admit { role: String, container_id: String },
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u8,
    uuid: String,
    state: String,
    expected_roles: Vec<String>,
    fake_environment: Vec<String>,
    mount_identity: Option<ComponentMountIdentity>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ComponentMountIdentity {
    pub(super) role: String,
    pub(super) container_id: String,
    pub(super) mounts: Vec<MountIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MountIdentity {
    pub(super) destination: String,
    pub(super) mount_type: String,
    pub(super) source: Option<String>,
    pub(super) name: Option<String>,
    pub(super) read_only: bool,
}

#[derive(Debug, Clone)]
pub(super) struct Admission {
    pub(super) fake_environment: Vec<String>,
    pub(super) mount_identity: ComponentMountIdentity,
}

struct SharedState {
    uuid: String,
    status: AtomicU8,
    stop_requested: AtomicBool,
    expected_roles: Vec<String>,
    component_mounts: std::sync::Mutex<HashMap<String, ComponentMountIdentity>>,
    fake_environment: std::sync::Mutex<Vec<String>>,
    stop_error: std::sync::Mutex<Option<String>>,
}

pub(super) struct RunControl {
    socket_path: PathBuf,
    private_identity: String,
    state: Arc<SharedState>,
    stop_server: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
    _lifetime_lock: File,
}

#[derive(Debug, Clone)]
pub(super) struct ControlStatus {
    pub(super) state: String,
    pub(super) expected_roles: Vec<String>,
}

impl RunControl {
    pub(super) fn start(
        runtime_root: &Path,
        uuid: &str,
        expected_roles: &[String],
    ) -> Result<Self> {
        validate_private_runtime_root(runtime_root)?;
        let root_metadata = fs::symlink_metadata(runtime_root).map_err(|error| {
            Error::message(format!("failed to inspect private run directory: {error}"))
        })?;
        let lock_path = runtime_root.join("lifetime.lock");
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(LOCK_MODE)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let lock = options.open(&lock_path).map_err(|error| {
            Error::message(format!(
                "failed to create run lifetime lock {}: {error}",
                lock_path.display()
            ))
        })?;
        // SAFETY: flock operates on the valid open file descriptor and does not retain pointers.
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&lock),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            drop(lock);
            let _ = fs::remove_file(&lock_path);
            return Err(Error::message("failed to acquire run lifetime lock"));
        }
        let lock_metadata = lock.metadata().map_err(|error| {
            Error::message(format!("failed to inspect run lifetime lock: {error}"))
        })?;
        let private_identity = encode_private_identity(
            root_metadata.dev(),
            root_metadata.ino(),
            lock_metadata.dev(),
            lock_metadata.ino(),
        );

        let socket_path = control_socket_path(runtime_root, uuid)?;
        match fs::symlink_metadata(&socket_path) {
            Ok(_) => {
                return Err(Error::message(format!(
                    "run control socket path already exists: {}",
                    socket_path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::message(format!(
                    "failed to inspect run control socket path {}: {error}",
                    socket_path.display()
                )));
            }
        }
        let listener = UnixListener::bind(&socket_path).map_err(|error| {
            Error::message(format!(
                "failed to bind run control socket {}: {error}",
                socket_path.display()
            ))
        })?;
        if let Err(error) =
            fs::set_permissions(&socket_path, fs::Permissions::from_mode(SOCKET_MODE))
        {
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            return Err(Error::message(format!(
                "failed to secure run control socket {}: {error}",
                socket_path.display()
            )));
        }
        if let Err(error) = listener.set_nonblocking(true) {
            drop(listener);
            let _ = fs::remove_file(&socket_path);
            return Err(Error::message(format!(
                "failed to configure run control listener: {error}"
            )));
        }

        let state = Arc::new(SharedState {
            uuid: uuid.to_ascii_lowercase(),
            status: AtomicU8::new(STATUS_STARTING),
            stop_requested: AtomicBool::new(false),
            expected_roles: expected_roles.to_vec(),
            component_mounts: std::sync::Mutex::new(HashMap::new()),
            fake_environment: std::sync::Mutex::new(Vec::new()),
            stop_error: std::sync::Mutex::new(None),
        });
        let stop_server = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop_server);
        let server = thread::spawn(move || {
            let mut clients = Vec::new();
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = Arc::clone(&thread_state);
                        if let Ok(client) = thread::Builder::new()
                            .name("cladding-run-control-client".to_string())
                            .spawn(move || serve_client(stream, state))
                        {
                            clients.push(client);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }

                let mut index = 0;
                while index < clients.len() {
                    if clients[index].is_finished() {
                        let client = clients.swap_remove(index);
                        let _ = client.join();
                    } else {
                        index += 1;
                    }
                }
            }
            for client in clients {
                let _ = client.join();
            }
        });
        Ok(Self {
            socket_path,
            private_identity,
            state,
            stop_server,
            server: Some(server),
            _lifetime_lock: lock,
        })
    }

    pub(super) fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub(super) fn private_identity(&self) -> &str {
        &self.private_identity
    }

    pub(super) fn mark_ready(&self, fake_environment: Vec<String>) -> bool {
        *self
            .state
            .fake_environment
            .lock()
            .expect("control environment lock") = fake_environment;
        self.state
            .status
            .compare_exchange(
                STATUS_STARTING,
                STATUS_READY,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
    }

    pub(super) fn set_component_mount_identities(
        &self,
        identities: Vec<ComponentMountIdentity>,
    ) -> Result<()> {
        let mut components = HashMap::new();
        for identity in identities {
            let destinations = identity
                .mounts
                .iter()
                .map(|mount| mount.destination.as_str())
                .collect::<BTreeSet<_>>();
            let mounts_valid = destinations.len() == identity.mounts.len()
                && identity.mounts.iter().all(|mount| {
                    mount.destination.starts_with('/')
                        && matches!(mount.mount_type.as_str(), "bind" | "volume" | "tmpfs")
                        && (mount.mount_type != "bind" || mount.source.is_some())
                        && (mount.mount_type != "volume" || mount.name.is_some())
                });
            if identity.container_id.is_empty()
                || !self
                    .state
                    .expected_roles
                    .iter()
                    .any(|role| role == &identity.role)
                || !mounts_valid
                || components.insert(identity.role.clone(), identity).is_some()
            {
                return Err(Error::message(
                    "run component mount identity metadata is invalid",
                ));
            }
        }
        if components.len() != self.state.expected_roles.len()
            || self
                .state
                .expected_roles
                .iter()
                .any(|role| !components.contains_key(role))
        {
            return Err(Error::message(
                "run component mount identity metadata is incomplete",
            ));
        }
        *self
            .state
            .component_mounts
            .lock()
            .expect("control mount identity lock") = components;
        Ok(())
    }

    pub(super) fn mark_stopping(&self) {
        self.state.status.store(STATUS_STOPPING, Ordering::SeqCst);
    }

    pub(super) fn mark_stopped(&self, cleanup_error: Option<String>) {
        if cleanup_error.is_some() {
            *self
                .state
                .stop_error
                .lock()
                .expect("control cleanup error lock") = cleanup_error;
        }
        self.state.status.store(STATUS_STOPPED, Ordering::SeqCst);
    }

    pub(super) fn unlink_socket_for_shutdown(&self) -> Result<()> {
        match fs::remove_file(&self.socket_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(Error::message(format!(
                "failed to remove run control socket {}: {error}",
                self.socket_path.display()
            ))),
        }
    }

    pub(super) fn stop_requested(&self) -> bool {
        self.state.stop_requested.load(Ordering::SeqCst)
    }

    pub(super) fn finish(&mut self) {
        if self.state.status.load(Ordering::SeqCst) != STATUS_STOPPED {
            self.mark_stopped(None);
        }
        self.stop_server.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
        let _ = fs::remove_file(&self.socket_path);
        // SAFETY: the file descriptor remains owned by _lifetime_lock until this value drops.
        unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&self._lifetime_lock),
                libc::LOCK_UN,
            );
        }
    }
}

impl Drop for RunControl {
    fn drop(&mut self) {
        self.finish();
    }
}

pub(super) fn acquire_recovery_lock(runtime_root: &Path, expected_identity: &str) -> Result<File> {
    let expected = parse_private_identity(expected_identity).ok_or_else(|| {
        Error::message("refusing recovery because the run private identity is malformed")
    })?;
    validate_private_runtime_root(runtime_root)?;
    let root_metadata = fs::symlink_metadata(runtime_root).map_err(|error| {
        Error::message(format!("run private directory is unavailable: {error}"))
    })?;
    if root_metadata.file_type().is_symlink()
        || !root_metadata.is_dir()
        || root_metadata.uid() != invoking_uid()
        || root_metadata.mode() & 0o077 != 0
    {
        return Err(Error::message(
            "refusing recovery because the run private directory is not a private user-owned directory",
        ));
    }
    if (root_metadata.dev(), root_metadata.ino()) != (expected.0, expected.1) {
        return Err(Error::message(
            "refusing recovery because the run private directory identity changed",
        ));
    }
    let lock_path = runtime_root.join("lifetime.lock");
    let metadata = fs::symlink_metadata(&lock_path)
        .map_err(|error| Error::message(format!("run lifetime lock is unavailable: {error}")))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != invoking_uid()
        || metadata.mode() & 0o777 != LOCK_MODE
    {
        return Err(Error::message(
            "refusing recovery because the run lifetime lock identity is invalid",
        ));
    }
    if (metadata.dev(), metadata.ino()) != (expected.2, expected.3) {
        return Err(Error::message(
            "refusing recovery because the run lifetime lock identity changed",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&lock_path)
        .map_err(|error| Error::message(format!("failed to open run lifetime lock: {error}")))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| Error::message(format!("failed to verify run lifetime lock: {error}")))?;
    if (opened_metadata.dev(), opened_metadata.ino()) != (expected.2, expected.3) {
        return Err(Error::message(
            "refusing recovery because the opened run lifetime lock identity changed",
        ));
    }
    // SAFETY: flock operates on a valid file descriptor and keeps no references.
    if unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(Error::message(
            "refusing recovery because the run supervisor still owns the lifetime lock",
        ));
    }
    let final_root = fs::symlink_metadata(runtime_root).map_err(|error| {
        Error::message(format!("failed to recheck private run directory: {error}"))
    })?;
    let final_lock = fs::symlink_metadata(&lock_path)
        .map_err(|error| Error::message(format!("failed to recheck run lifetime lock: {error}")))?;
    if (final_root.dev(), final_root.ino()) != (expected.0, expected.1)
        || (final_lock.dev(), final_lock.ino()) != (expected.2, expected.3)
    {
        return Err(Error::message(
            "refusing recovery because private run ownership changed during verification",
        ));
    }
    Ok(file)
}

fn validate_private_runtime_root(runtime_root: &Path) -> Result<()> {
    if !runtime_root.is_absolute() {
        return Err(Error::message("run private directory path is not absolute"));
    }
    let mut paths = vec![(runtime_root, true)];
    if let Some(runs_dir) = runtime_root
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "runs"))
    {
        let runtime_dir = runs_dir
            .parent()
            .ok_or_else(|| Error::message("run private directory has no runtime parent"))?;
        let project_root = runtime_dir
            .parent()
            .ok_or_else(|| Error::message("run private directory has no project parent"))?;
        if fs::canonicalize(project_root).ok().as_deref() != Some(project_root) {
            return Err(Error::message(
                "refusing recovery because the project path is not canonical",
            ));
        }
        paths.extend([
            (runs_dir, true),
            (runtime_dir, false),
            (project_root, false),
        ]);
    }
    for (path, private) in paths {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| Error::message(format!("run private path is unavailable: {error}")))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != invoking_uid()
            || (private && metadata.mode() & 0o077 != 0)
        {
            return Err(Error::message(
                "refusing recovery because a run private path is not a user-owned directory with the expected permissions",
            ));
        }
    }
    Ok(())
}

fn encode_private_identity(
    root_device: u64,
    root_inode: u64,
    lock_device: u64,
    lock_inode: u64,
) -> String {
    format!("v1:{root_device}:{root_inode}:{lock_device}:{lock_inode}")
}

fn parse_private_identity(value: &str) -> Option<(u64, u64, u64, u64)> {
    let mut fields = value.split(':');
    (fields.next()? == "v1").then_some(())?;
    let root_device = fields.next()?.parse().ok()?;
    let root_inode = fields.next()?.parse().ok()?;
    let lock_device = fields.next()?.parse().ok()?;
    let lock_inode = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some((root_device, root_inode, lock_device, lock_inode))
}

pub(super) fn remove_stale_socket(path: &Path, runtime_root: &Path, uuid: &str) -> Result<()> {
    let preferred = runtime_root.join("control.sock");
    let short = short_control_socket_path(uuid);
    if path != preferred && path != short {
        return Err(Error::message(
            "refusing to remove a supervisor endpoint outside the expected private run paths",
        ));
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(Error::message(format!(
                "failed to inspect stale supervisor endpoint: {error}"
            )));
        }
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != invoking_uid()
        || metadata.mode() & 0o777 != SOCKET_MODE
    {
        return Err(Error::message(
            "refusing to remove supervisor endpoint with invalid ownership or type",
        ));
    }
    if path == short {
        let parent = path.parent().expect("short socket has parent");
        let parent_metadata = fs::symlink_metadata(parent).map_err(|error| {
            Error::message(format!(
                "failed to inspect private control socket directory: {error}"
            ))
        })?;
        if parent_metadata.file_type().is_symlink()
            || !parent_metadata.is_dir()
            || parent_metadata.uid() != invoking_uid()
            || parent_metadata.mode() & 0o077 != 0
        {
            return Err(Error::message(
                "refusing to remove supervisor endpoint from an unverified directory",
            ));
        }
    }
    fs::remove_file(path).map_err(|error| {
        Error::message(format!(
            "failed to remove stale supervisor endpoint: {error}"
        ))
    })
}

fn control_socket_path(runtime_root: &Path, uuid: &str) -> Result<PathBuf> {
    let preferred = expected_control_socket_path(runtime_root, uuid);
    if preferred == runtime_root.join("control.sock") {
        return Ok(preferred);
    }
    let short_root = preferred
        .parent()
        .expect("short control socket path has a parent");
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(short_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(Error::message(format!(
                "failed to create private control socket directory: {error}"
            )));
        }
    }
    let metadata = fs::symlink_metadata(short_root).map_err(|error| {
        Error::message(format!(
            "failed to inspect control socket directory: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != invoking_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(Error::message(
            "run control socket directory is not private and user-owned",
        ));
    }
    fs::set_permissions(short_root, fs::Permissions::from_mode(0o700)).map_err(|error| {
        Error::message(format!(
            "failed to secure private control socket directory: {error}"
        ))
    })?;
    Ok(preferred)
}

pub(super) fn expected_control_socket_path(runtime_root: &Path, uuid: &str) -> PathBuf {
    let preferred = runtime_root.join("control.sock");
    if preferred.as_os_str().len() < 100 {
        preferred
    } else {
        short_control_socket_path(uuid)
    }
}

fn short_control_socket_path(uuid: &str) -> PathBuf {
    Path::new("/tmp")
        .join(format!("cladding-control-{}", invoking_uid()))
        .join(format!("{uuid}.sock"))
}

fn serve_client(mut stream: UnixStream, state: Arc<SharedState>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let request = match read_request(&stream) {
        Ok(request) => request,
        Err(_) => return,
    };
    let response = if request.version != PROTOCOL_VERSION || request.uuid != state.uuid {
        error_response(
            &state,
            "control protocol version or run identity did not match",
        )
    } else {
        match request.operation {
            Operation::Status => response(&state, Vec::new(), None),
            Operation::Admit { role, container_id } => {
                if state.status.load(Ordering::SeqCst) != STATUS_READY {
                    error_response(&state, "run is not accepting attachments")
                } else if !state
                    .expected_roles
                    .iter()
                    .any(|expected| expected == &role)
                {
                    error_response(&state, "requested component is not enabled for this run")
                } else {
                    let identity = state
                        .component_mounts
                        .lock()
                        .expect("control mount identity lock")
                        .get(&role)
                        .filter(|identity| identity.container_id == container_id)
                        .cloned();
                    match identity {
                        Some(identity) => {
                            let values = state
                                .fake_environment
                                .lock()
                                .expect("control environment lock")
                                .clone();
                            response_with_mount_identity(
                                &state,
                                if role == "agent" { values } else { Vec::new() },
                                identity,
                                None,
                            )
                        }
                        None => error_response(&state, "selected component identity changed"),
                    }
                }
            }
            Operation::Stop => {
                let current = state.status.load(Ordering::SeqCst);
                if current == STATUS_READY || current == STATUS_STARTING {
                    state.status.store(STATUS_STOPPING, Ordering::SeqCst);
                    state.stop_requested.store(true, Ordering::SeqCst);
                }
                let deadline = Instant::now() + Duration::from_secs(120);
                while state.status.load(Ordering::SeqCst) != STATUS_STOPPED
                    && Instant::now() < deadline
                {
                    thread::sleep(Duration::from_millis(50));
                }
                if state.status.load(Ordering::SeqCst) == STATUS_STOPPED {
                    if let Some(error) = state
                        .stop_error
                        .lock()
                        .expect("control cleanup error lock")
                        .clone()
                    {
                        error_response(&state, &error)
                    } else {
                        response(&state, Vec::new(), None)
                    }
                } else {
                    error_response(&state, "timed out waiting for supervisor cleanup")
                }
            }
        }
    };
    let _ = write_response(&mut stream, &response);
}

fn response(state: &SharedState, fake_environment: Vec<String>, error: Option<String>) -> Response {
    Response {
        version: PROTOCOL_VERSION,
        uuid: state.uuid.clone(),
        state: state_name(state.status.load(Ordering::SeqCst)).to_string(),
        expected_roles: state.expected_roles.clone(),
        fake_environment,
        mount_identity: None,
        error,
    }
}

fn error_response(state: &SharedState, error: &str) -> Response {
    response(state, Vec::new(), Some(error.to_string()))
}

fn response_with_mount_identity(
    state: &SharedState,
    fake_environment: Vec<String>,
    mount_identity: ComponentMountIdentity,
    error: Option<String>,
) -> Response {
    let mut response = response(state, fake_environment, error);
    response.mount_identity = Some(mount_identity);
    response
}

fn state_name(value: u8) -> &'static str {
    match value {
        STATUS_STARTING => "starting",
        STATUS_READY => "ready",
        STATUS_STOPPING => "stopping",
        STATUS_STOPPED => "stopped",
        _ => "unknown",
    }
}

fn read_request(stream: &UnixStream) -> Result<Request> {
    let mut line = Vec::new();
    let reader = BufReader::new(stream);
    reader
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|error| Error::message(format!("failed to read run control request: {error}")))?;
    if line.len() > MAX_MESSAGE_BYTES || !line.ends_with(b"\n") {
        return Err(Error::message(
            "run control request exceeded its size limit",
        ));
    }
    serde_json::from_slice(&line[..line.len() - 1])
        .map_err(|_| Error::message("run control request was malformed"))
}

fn write_response(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let mut bytes = serde_json::to_vec(response)
        .map_err(|_| Error::message("failed to encode run control response"))?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        let error = response
            .error
            .as_deref()
            .map(|error| truncate_response_error(error, MAX_RESPONSE_ERROR_BYTES))
            .unwrap_or_else(|| "run control response exceeded its size limit".to_string());
        let bounded_response = Response {
            version: response.version,
            uuid: response.uuid.clone(),
            state: response.state.clone(),
            expected_roles: Vec::new(),
            fake_environment: Vec::new(),
            mount_identity: None,
            error: Some(error),
        };
        bytes = serde_json::to_vec(&bounded_response)
            .map_err(|_| Error::message("failed to encode bounded run control response"))?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(Error::message(
                "bounded run control response exceeded its size limit",
            ));
        }
    }
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|error| Error::message(format!("failed to write run control response: {error}")))
}

fn truncate_response_error(error: &str, max_bytes: usize) -> String {
    const SUFFIX: &str = "… [truncated]";
    if error.len() <= max_bytes {
        return error.to_string();
    }

    let content_limit = max_bytes.saturating_sub(SUFFIX.len());
    let mut boundary = content_limit.min(error.len());
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}{SUFFIX}", &error[..boundary])
}

pub(super) fn status(socket_path: &Path, uuid: &str) -> Result<ControlStatus> {
    let response = request(socket_path, uuid, Operation::Status)?;
    Ok(ControlStatus {
        state: response.state,
        expected_roles: response.expected_roles,
    })
}

pub(super) fn admit(
    socket_path: &Path,
    uuid: &str,
    role: &str,
    container_id: &str,
) -> Result<Admission> {
    let response = request(
        socket_path,
        uuid,
        Operation::Admit {
            role: role.to_string(),
            container_id: container_id.to_string(),
        },
    )?;
    if response.state != "ready" {
        return Err(Error::message(format!(
            "run is {} and cannot accept attachments",
            response.state
        )));
    }
    if role != "agent" && !response.fake_environment.is_empty() {
        return Err(Error::message(
            "run supervisor returned agent defaults for a non-agent target",
        ));
    }
    let mut environment_names = BTreeSet::new();
    for value in &response.fake_environment {
        let Some((name, content)) = value.split_once('=') else {
            return Err(Error::message(
                "run supervisor returned malformed fake environment defaults",
            ));
        };
        if !valid_environment_name(name)
            || content.contains('\0')
            || !environment_names.insert(name)
        {
            return Err(Error::message(
                "run supervisor returned invalid fake environment defaults",
            ));
        }
    }
    let mount_identity = response
        .mount_identity
        .ok_or_else(|| Error::message("run supervisor omitted component mount identity"))?;
    if mount_identity.role != role || mount_identity.container_id != container_id {
        return Err(Error::message(
            "run supervisor returned a different component identity",
        ));
    }
    Ok(Admission {
        fake_environment: response.fake_environment,
        mount_identity,
    })
}

pub(super) fn stop(socket_path: &Path, uuid: &str, timeout: Duration) -> Result<()> {
    if timeout > Duration::from_secs(120) {
        return Err(Error::message(
            "run stop timeout exceeds the protocol bound",
        ));
    }
    let response = request(socket_path, uuid, Operation::Stop)?;
    if response.state == "stopped" {
        Ok(())
    } else {
        Err(Error::message(
            "supervisor did not confirm orderly run shutdown",
        ))
    }
}

fn request(socket_path: &Path, uuid: &str, operation: Operation) -> Result<Response> {
    validate_socket(socket_path)?;
    let read_timeout = if matches!(&operation, Operation::Stop) {
        Duration::from_secs(125)
    } else {
        Duration::from_secs(3)
    };
    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        Error::message(format!(
            "run supervisor is unavailable at {}: {error}",
            socket_path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|error| {
            Error::message(format!("failed to configure run control timeout: {error}"))
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .map_err(|error| {
            Error::message(format!("failed to configure run control timeout: {error}"))
        })?;
    let request = Request {
        version: PROTOCOL_VERSION,
        uuid: uuid.to_ascii_lowercase(),
        operation,
    };
    let mut bytes = serde_json::to_vec(&request)
        .map_err(|_| Error::message("failed to encode run control request"))?;
    bytes.push(b'\n');
    stream
        .write_all(&bytes)
        .map_err(|error| Error::message(format!("failed to write run control request: {error}")))?;
    let mut response = read_response(&stream)?;
    if response.version != PROTOCOL_VERSION || response.uuid != uuid.to_ascii_lowercase() {
        return Err(Error::message(
            "run control response identity did not match the selected run",
        ));
    }
    if !matches!(
        response.state.as_str(),
        "starting" | "ready" | "stopping" | "stopped"
    ) {
        return Err(Error::message(
            "run control response contains an invalid supervisor state",
        ));
    }
    if let Some(error) = response.error.take() {
        return Err(Error::message(error));
    }
    let roles = response
        .expected_roles
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if roles.len() != response.expected_roles.len() {
        return Err(Error::message(
            "run control response contains duplicate component roles",
        ));
    }
    let allowed = ["agent", "proxy", "nw-sandbox", "fs-sandbox"];
    if !roles.contains("agent")
        || !roles.contains("proxy")
        || roles.iter().any(|role| !allowed.contains(&role.as_str()))
    {
        return Err(Error::message(
            "run control response contains invalid component roles",
        ));
    }
    Ok(response)
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn read_response(stream: &UnixStream) -> Result<Response> {
    let mut line = Vec::new();
    let reader = BufReader::new(stream);
    reader
        .take((MAX_MESSAGE_BYTES + 1) as u64)
        .read_until(b'\n', &mut line)
        .map_err(|error| Error::message(format!("failed to read run control response: {error}")))?;
    if line.len() > MAX_MESSAGE_BYTES || !line.ends_with(b"\n") {
        return Err(Error::message(
            "run control response exceeded its size limit",
        ));
    }
    serde_json::from_slice(&line[..line.len() - 1])
        .map_err(|_| Error::message("run control response was malformed"))
}

fn validate_socket(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        Error::message(format!(
            "run supervisor endpoint is unavailable at {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != invoking_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(Error::message(
            "run supervisor endpoint ownership or permissions are invalid",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn run_root(label: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "cladding-control-{label}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let runtime_root = base
            .join("project/custom-state/runtime/runs")
            .join("cladding-run-aabbccdd-1111-4222-8333-123456789abc");
        fs::create_dir_all(&runtime_root).unwrap();
        fs::set_permissions(
            runtime_root.parent().unwrap(),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(
            runtime_root.parent().unwrap().parent().unwrap(),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::set_permissions(&runtime_root, fs::Permissions::from_mode(0o700)).unwrap();
        (base, runtime_root)
    }

    #[test]
    fn control_admits_only_after_ready_and_stop_requires_completion() {
        let root =
            std::env::temp_dir().join(format!("cladding-control-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let control = RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789abc",
            &["agent".to_string(), "proxy".to_string()],
        )
        .unwrap();
        let identities = ["agent", "proxy"]
            .into_iter()
            .map(|role| ComponentMountIdentity {
                role: role.to_string(),
                container_id: format!("{role}-id"),
                mounts: Vec::new(),
            })
            .collect();
        control.set_component_mount_identities(identities).unwrap();
        assert!(acquire_recovery_lock(&root, control.private_identity()).is_err());
        assert!(
            admit(
                control.socket_path(),
                "aabbccdd-1111-4222-8333-123456789abc",
                "agent",
                "agent-id"
            )
            .is_err()
        );
        control.mark_ready(vec!["TOKEN=fake-value".to_string()]);
        let admission = admit(
            control.socket_path(),
            "aabbccdd-1111-4222-8333-123456789abc",
            "agent",
            "agent-id",
        )
        .unwrap();
        assert_eq!(admission.fake_environment, ["TOKEN=fake-value"]);
        assert!(
            admit(
                control.socket_path(),
                "aabbccdd-1111-4222-8333-123456789abc",
                "nw-sandbox",
                "sandbox-id"
            )
            .is_err()
        );
        control.mark_stopping();
        control.mark_stopped(None);
        assert!(
            stop(
                control.socket_path(),
                "aabbccdd-1111-4222-8333-123456789abc",
                Duration::from_secs(1)
            )
            .is_ok()
        );
        let private_identity = control.private_identity().to_string();
        drop(control);
        let recovery_lock = acquire_recovery_lock(&root, &private_identity).unwrap();
        drop(recovery_lock);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rejects_replaced_private_directory_and_lock() {
        for replace_lock in [false, true] {
            let (base, root) = run_root(if replace_lock {
                "lock-replacement"
            } else {
                "root-replacement"
            });
            let uuid = if replace_lock {
                "aabbccdd-1111-4222-8333-123456789ab2"
            } else {
                "aabbccdd-1111-4222-8333-123456789ab1"
            };
            let control =
                RunControl::start(&root, uuid, &["agent".to_string(), "proxy".to_string()])
                    .unwrap();
            let identity = control.private_identity().to_string();
            drop(control);

            if replace_lock {
                fs::rename(root.join("lifetime.lock"), root.join("original.lock")).unwrap();
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(LOCK_MODE)
                    .open(root.join("lifetime.lock"))
                    .unwrap();
            } else {
                fs::rename(&root, root.with_extension("original")).unwrap();
                fs::create_dir(&root).unwrap();
                fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(LOCK_MODE)
                    .open(root.join("lifetime.lock"))
                    .unwrap();
            }

            assert!(acquire_recovery_lock(&root, &identity).is_err());
            fs::remove_dir_all(base).unwrap();
        }
    }

    #[test]
    fn recovery_rejects_symlinked_private_parent() {
        let (base, root) = run_root("symlinked-parent");
        let control = RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789ab3",
            &["agent".to_string(), "proxy".to_string()],
        )
        .unwrap();
        let identity = control.private_identity().to_string();
        drop(control);

        let runs = root.parent().unwrap().to_path_buf();
        let saved = runs.with_extension("saved");
        fs::rename(&runs, &saved).unwrap();
        symlink(&saved, &runs).unwrap();

        assert!(acquire_recovery_lock(&root, &identity).is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn stop_reports_cleanup_failure_after_supervisor_stops() {
        let root = std::env::temp_dir().join(format!(
            "cladding-control-failed-stop-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let control = RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789abc",
            &["agent".to_string(), "proxy".to_string()],
        )
        .unwrap();
        control.mark_ready(Vec::new());
        control.mark_stopping();
        control.mark_stopped(Some("container cleanup failed".to_string()));
        let error = stop(
            control.socket_path(),
            "aabbccdd-1111-4222-8333-123456789abc",
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("container cleanup failed"));
        drop(control);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_cleanup_error_still_returns_a_bounded_stop_response() {
        let root = std::env::temp_dir().join(format!(
            "cladding-control-oversized-error-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let control = RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789abc",
            &["agent".to_string(), "proxy".to_string()],
        )
        .unwrap();
        control.mark_ready(Vec::new());
        control.mark_stopping();
        control.mark_stopped(Some("container cleanup failed ".repeat(MAX_MESSAGE_BYTES)));

        let error = stop(
            control.socket_path(),
            "aabbccdd-1111-4222-8333-123456789abc",
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("container cleanup failed"));
        assert!(error.to_string().ends_with("[truncated]"));
        assert!(
            !error
                .to_string()
                .contains("response exceeded its size limit")
        );
        drop(control);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_shutdown_waits_for_active_client_handlers() {
        let root = std::env::temp_dir().join(format!(
            "cladding-control-active-client-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let mut control = RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789abc",
            &["agent".to_string(), "proxy".to_string()],
        )
        .unwrap();
        let mut client = UnixStream::connect(control.socket_path()).unwrap();
        client.write_all(b"{\"version\":1,").unwrap();
        thread::sleep(Duration::from_millis(100));

        let started = Instant::now();
        control.finish();
        assert!(started.elapsed() >= Duration::from_secs(1));

        drop(client);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_socket_uses_short_private_path_when_project_path_is_long() {
        let base =
            std::env::temp_dir().join(format!("cladding-long-control-{}", std::process::id()));
        let root = base
            .join("long-project-runtime-segment-one")
            .join("long-project-runtime-segment-two")
            .join("long-project-runtime-segment-three")
            .join("long-project-runtime-segment-four");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let control = RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789abc",
            &["agent".to_string(), "proxy".to_string()],
        )
        .unwrap();
        assert_eq!(
            control.socket_path(),
            expected_control_socket_path(&root, "aabbccdd-1111-4222-8333-123456789abc")
        );
        assert_eq!(
            control.socket_path().parent(),
            Some(
                Path::new("/tmp")
                    .join(format!("cladding-control-{}", invoking_uid()))
                    .as_path()
            )
        );
        assert!(control.socket_path().as_os_str().len() < 100);
        drop(control);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn stale_control_socket_is_never_replaced() {
        let root =
            std::env::temp_dir().join(format!("cladding-stale-control-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let error = match RunControl::start(
            &root,
            "aabbccdd-1111-4222-8333-123456789abc",
            &["agent".to_string(), "proxy".to_string()],
        ) {
            Ok(_) => panic!("stale supervisor socket must prevent run ownership"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("already exists"));
        assert!(
            fs::symlink_metadata(&socket)
                .unwrap()
                .file_type()
                .is_socket()
        );
        drop(listener);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_control_endpoint_fails_clearly_without_orphan_claim() {
        let missing = std::env::temp_dir().join(format!(
            "cladding-missing-control-{}-{}.sock",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let error = status(&missing, "aabbccdd-1111-4222-8333-123456789abc").unwrap_err();
        assert!(error.to_string().contains("endpoint is unavailable"));
    }
}
