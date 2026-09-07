use std::env;
use std::f32::consts::PI;
use std::ffi::CString;
use std::fs::{read_to_string, remove_file};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use loadngo_proactor::{IoBuf, IoPort, IoResult, ProactorHandle};
use ndarray::{s, Array, Array1, Array2};

/// Re-exported so integration tests (and any external supervisor) can
/// construct the same proactor this crate runs on without repeating the
/// git dependency.
pub use loadngo_proactor;

#[cfg(target_os = "linux")]
pub mod runtime;

pub const THERMAL_STATE_NORMAL: u8 = 0;
pub const THERMAL_STATE_WARNING: u8 = 1;
pub const THERMAL_STATE_CRITICAL: u8 = 2;
pub const PACKET_VECTOR_SIZE: usize = 1518;
pub const COMPRESSED_PACKET_SIZE: usize = 32;
pub const FRAMEBUFFER_SIZE_BYTES: usize = COMPRESSED_PACKET_SIZE * std::mem::size_of::<f32>();

pub struct PacketCompressor {
    weights1: Array2<f32>,
    biases1: Array1<f32>,
    weights2: Array2<f32>,
    biases2: Array1<f32>,
    weights3: Array2<f32>,
    biases3: Array1<f32>,
    weights4: Array2<f32>,
    biases4: Array1<f32>,
}

impl PacketCompressor {
    pub fn new(input_size: usize, compressed_size: usize) -> Self {
        let layer1_size = 1024;
        let layer2_size = 512;
        let layer3_size = 256;

        PacketCompressor {
            weights1: Self::generate_waveform_weights(input_size, layer1_size),
            biases1: Self::generate_waveform_biases(layer1_size),
            weights2: Self::generate_parabolic_weights(layer1_size, layer2_size),
            biases2: Self::generate_waveform_biases(layer2_size),
            weights3: Self::generate_waveform_weights(layer2_size, layer3_size),
            biases3: Self::generate_waveform_biases(layer3_size),
            weights4: Self::generate_parabolic_weights(layer3_size, compressed_size),
            biases4: Self::generate_waveform_biases(compressed_size),
        }
    }

    pub fn generate_waveform_weights(rows: usize, cols: usize) -> Array2<f32> {
        Array2::from_shape_fn((rows, cols), |(i, j)| {
            0.11389
                + (i as f32 / rows as f32 * 2.0 * PI).sin()
                    * (j as f32 / cols as f32 * 2.0 * PI).cos()
        })
    }

    pub fn generate_waveform_biases(size: usize) -> Array1<f32> {
        Array1::from_shape_fn(size, |i| (i as f32 / size as f32 * 2.11793 * PI).sin())
    }

    pub fn generate_parabolic_weights(rows: usize, cols: usize) -> Array2<f32> {
        Array2::from_shape_fn((rows, cols), |(i, j)| {
            let x = i as f32 / rows as f32;
            let y = j as f32 / cols as f32;
            (x - 0.5).powi(2) + (y - 0.5).powi(2)
        })
    }

    pub fn compress(&self, input: Array1<f32>) -> Array1<f32> {
        let mut hidden1 = input.dot(&self.weights1) + &self.biases1;
        hidden1.mapv_inplace(|x| x.max(0.0));

        let mut hidden2 = hidden1.dot(&self.weights2) + &self.biases2;
        hidden2.mapv_inplace(|x| x.max(0.0));

        let mut hidden3 = hidden2.dot(&self.weights3) + &self.biases3;
        hidden3.mapv_inplace(|x| x.max(0.0));

        hidden3.dot(&self.weights4) + &self.biases4
    }
}

pub fn bytes_to_f32_vector(bytes: &[u8]) -> Array1<f32> {
    let floats: Vec<f32> = bytes.iter().map(|&byte| byte as f32 / 255.0).collect();

    if floats.len() >= PACKET_VECTOR_SIZE {
        Array1::from(floats[..PACKET_VECTOR_SIZE].to_vec())
    } else {
        let mut padded = Array::zeros(PACKET_VECTOR_SIZE);
        padded
            .slice_mut(s![..floats.len()])
            .assign(&Array1::from(floats));
        padded
    }
}

pub fn compressed_payload_to_frame_bytes(compressed: &Array1<f32>) -> [u8; FRAMEBUFFER_SIZE_BYTES] {
    let mut frame = [0u8; FRAMEBUFFER_SIZE_BYTES];
    for (chunk, value) in frame.chunks_exact_mut(std::mem::size_of::<f32>()).zip(
        compressed
            .iter()
            .copied()
            .chain(std::iter::repeat(0.0))
            .take(COMPRESSED_PACKET_SIZE),
    ) {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    frame
}

pub fn packet_to_frame_bytes(
    compressor: &PacketCompressor,
    packet: &[u8],
) -> [u8; FRAMEBUFFER_SIZE_BYTES] {
    let compressed = compressor.compress(bytes_to_f32_vector(packet));
    compressed_payload_to_frame_bytes(&compressed)
}

pub fn parse_temp_env(var_name: &str, default: f32) -> f32 {
    env::var(var_name)
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| *value > 0.0 && value.is_finite())
        .unwrap_or(default)
}

pub fn parse_temp_interval_env(var_name: &str, default: u64) -> u64 {
    env::var(var_name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

pub fn read_cpu_temperature_c() -> Option<f32> {
    const TEMP_PATHS: [&str; 2] = [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/thermal/thermal_zone1/temp",
    ];

    for path in TEMP_PATHS {
        if let Ok(raw) = read_to_string(path) {
            if let Ok(parsed) = raw.trim().parse::<f32>() {
                return Some(if parsed > 200.0 {
                    parsed / 1000.0
                } else {
                    parsed
                });
            }
        }
    }

    None
}

pub fn thermal_state_name(state: u8) -> &'static str {
    match state {
        THERMAL_STATE_WARNING => "warning",
        THERMAL_STATE_CRITICAL => "critical",
        _ => "normal",
    }
}

pub fn thermal_recommendation(state: u8) -> &'static str {
    match state {
        THERMAL_STATE_WARNING => "throttle",
        THERMAL_STATE_CRITICAL => "pause",
        _ => "normal",
    }
}

pub fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The set of connected thermal-status subscribers.
///
/// Clients are held as `Arc<UnixStream>` rather than plain `UnixStream`
/// specifically because sends are now asynchronous: an `IoPort::send` is
/// submitted to the kernel and completes later, so dropping the stream
/// (and closing its fd) the moment a write fails would let the fd be
/// closed -- and possibly reused by an unrelated `open` -- while the
/// kernel still holds a reference to it for an in-flight send. Each
/// submission moves an `Arc` clone into its own completion handler, so
/// the fd cannot close until that specific send has completed.
#[derive(Clone, Default)]
pub struct ThermalSignalPublisher {
    clients: Arc<Mutex<Vec<Arc<UnixStream>>>>,
}

impl ThermalSignalPublisher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a freshly accepted subscriber.
    pub fn add_client(&self, stream: UnixStream) {
        match self.clients.lock() {
            Ok(mut clients) => clients.push(Arc::new(stream)),
            Err(err) => eprintln!("Unable to lock thermal signal client list: {}", err),
        }
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().map(|c| c.len()).unwrap_or(0)
    }

    /// Publishes one newline-delimited payload to every subscriber as a
    /// real `IoPort::send`, rather than a blocking `write_all` on the
    /// caller's thread. A send that fails drops just that subscriber.
    pub fn emit_via<P: IoPort>(&self, handle: &ProactorHandle<P>, payload: &str) {
        let clients = match self.clients.lock() {
            Ok(clients) => clients,
            Err(err) => {
                eprintln!("Unable to lock thermal signal client list: {}", err);
                return;
            }
        };

        let mut line = Vec::with_capacity(payload.len() + 1);
        line.extend_from_slice(payload.as_bytes());
        line.push(b'\n');

        for client in clients.iter() {
            let fd = client.as_raw_fd();
            // Keeps this client's fd open for the whole in-flight send,
            // and identifies it for removal if the send fails.
            let keep = Arc::clone(client);
            let list = Arc::clone(&self.clients);
            let submitted = handle.send(
                fd,
                IoBuf::from_vec(line.clone()),
                move |result: IoResult| {
                    if result.is_err() {
                        if let Ok(mut clients) = list.lock() {
                            clients.retain(|c| !Arc::ptr_eq(c, &keep));
                        }
                    }
                    drop(keep);
                },
            );

            if let Err(err) = submitted {
                eprintln!("Unable to submit thermal signal send: {}", err);
            }
        }
    }
}

pub fn thermal_payload(
    state: u8,
    temperature_c: Option<f32>,
    warn_temp_c: f32,
    critical_temp_c: f32,
) -> String {
    let temp_json = match temperature_c {
        Some(temp) => format!("{temp:.1}"),
        None => "null".to_string(),
    };
    format!(
        "{{\"state\":\"{}\",\"temp_c\":{},\"warn_c\":{:.1},\"crit_c\":{:.1},\"ts\":{},\"recommendation\":\"{}\"}}",
        thermal_state_name(state),
        temp_json,
        warn_temp_c,
        critical_temp_c,
        unix_timestamp_seconds(),
        thermal_recommendation(state),
    )
}

pub fn emit_thermal_signal<P: IoPort>(
    publisher: &ThermalSignalPublisher,
    handle: &ProactorHandle<P>,
    state: u8,
    temperature_c: Option<f32>,
    warn_temp_c: f32,
    critical_temp_c: f32,
) {
    publisher.emit_via(
        handle,
        &thermal_payload(state, temperature_c, warn_temp_c, critical_temp_c),
    );
}

/// Binds the thermal signal socket and returns it alongside an empty
/// publisher.
///
/// This used to also spawn a dedicated accept thread that polled a
/// non-blocking `accept()` on a 100ms `thread::sleep` loop. That thread
/// is gone: the listener is now registered with the proactor and drained
/// only when the kernel reports it readable (see
/// [`runtime::serve`]). The listener is still returned in non-blocking
/// mode, which is what makes the readiness-driven drain loop terminate
/// on `WouldBlock` instead of stalling the proactor thread.
pub fn bind_signal_socket(
    path: &str,
    owner: &str,
) -> io::Result<(UnixListener, ThermalSignalPublisher)> {
    let socket_path = Path::new(path);
    if socket_path.exists() {
        let metadata = std::fs::metadata(socket_path)?;
        if !metadata.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{path} exists and is not a Unix socket"),
            ));
        }
        remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    ensure_socket_owner(path, owner)?;
    ensure_socket_mode(path, 0o660)?;

    Ok((listener, ThermalSignalPublisher::new()))
}

/// Drains every connection currently pending on the listener into the
/// publisher, stopping at `WouldBlock`.
///
/// Deliberately not `IoPort::accept`: that call reports the peer as a
/// `std::net::SocketAddr`, which it obtains through
/// `socket2::SockAddr::as_socket()`. For an `AF_UNIX` peer that returns
/// `None`, so the completion arrives as
/// `Err(InvalidData, "accept completed but the peer address family was
/// unrecognized")` -- and the already-accepted fd carried by that
/// completion is dropped without being closed, leaking one fd per
/// connection. `IoPort::accept` is IP-only today; a Unix listener has to
/// go through readiness plus a plain `accept()` until it grows an
/// address-family-agnostic completion type.
pub fn drain_pending_clients(listener: &UnixListener, publisher: &ThermalSignalPublisher) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => publisher.add_client(stream),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                eprintln!("Thermal signal socket accept error: {}", err);
                return;
            }
        }
    }
}

pub fn lookup_user_ids(username: &str) -> io::Result<(u32, u32)> {
    let c_username = CString::new(username)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "username contains NUL"))?;
    let passwd_ptr = unsafe { libc::getpwnam(c_username.as_ptr()) };
    if passwd_ptr.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("user `{username}` not found"),
        ));
    }

    let passwd = unsafe { *passwd_ptr };
    Ok((passwd.pw_uid, passwd.pw_gid))
}

pub fn current_effective_username() -> io::Result<String> {
    let uid = unsafe { libc::geteuid() };
    let passwd_ptr = unsafe { libc::getpwuid(uid) };
    if passwd_ptr.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("effective uid `{uid}` does not map to a passwd entry"),
        ));
    }

    let passwd = unsafe { *passwd_ptr };
    let username = unsafe { std::ffi::CStr::from_ptr(passwd.pw_name) }
        .to_str()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "passwd username is not utf-8"))?
        .to_string();
    Ok(username)
}

pub fn ensure_socket_owner(path: &str, username: &str) -> io::Result<()> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{path} is not a Unix socket"),
        ));
    }

    let (uid, gid) = lookup_user_ids(username)?;
    if metadata.uid() == uid && metadata.gid() == gid {
        return Ok(());
    }

    let c_path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL"))?;
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

pub fn ensure_socket_mode(path: &str, mode: u32) -> io::Result<()> {
    let c_path = CString::new(path)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL"))?;
    // `mode_t` is u32 on Linux but u16 on the BSDs/macOS, so the cast is
    // what lets this crate type-check off-target as well as on the Pi.
    let rc = unsafe { libc::chmod(c_path.as_ptr(), mode as libc::mode_t) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Parsed launch configuration, shared by every runtime path.
pub struct Config {
    pub interface_name: String,
    pub framebuffer_path: String,
    pub warn_temp_c: f32,
    pub critical_temp_c: f32,
    pub temp_check_interval: std::time::Duration,
    pub signal_socket_path: Option<String>,
    pub signal_socket_owner: String,
}

impl Config {
    pub fn from_env_and_args() -> Result<Self, Box<dyn std::error::Error>> {
        let mut args = env::args().skip(1);
        let interface_name = args
            .next()
            .ok_or("Usage: starlight <interface_name> <framebuffer_name>")?;
        let framebuffer_path = args
            .next()
            .ok_or("Usage: starlight <interface_name> <framebuffer_name>")?;

        let warn_temp_c = parse_temp_env("STARLIGHT_WARN_TEMP_C", 80.0);
        let critical_temp_c = parse_temp_env("STARLIGHT_CRIT_TEMP_C", 85.0);
        let temp_check_interval = std::time::Duration::from_secs(parse_temp_interval_env(
            "STARLIGHT_TEMP_CHECK_INTERVAL_SECS",
            5,
        ));

        if critical_temp_c <= warn_temp_c {
            return Err(format!(
                "Environment config invalid: STARLIGHT_CRIT_TEMP_C ({critical_temp_c:.1}) must be higher than STARLIGHT_WARN_TEMP_C ({warn_temp_c:.1})"
            )
            .into());
        }

        let signal_socket_path = env::var("STARLIGHT_SIGNAL_SOCKET")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let signal_socket_owner = env::var("STARLIGHT_SIGNAL_SOCKET_OWNER")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(Ok)
            .unwrap_or_else(current_effective_username)?;

        Ok(Self {
            interface_name,
            framebuffer_path,
            warn_temp_c,
            critical_temp_c,
            temp_check_interval,
            signal_socket_path,
            signal_socket_owner,
        })
    }
}

#[cfg(target_os = "linux")]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    runtime::serve(Config::from_env_and_args()?)
}

/// starlight drives a Sense HAT framebuffer from `AF_PACKET` capture on a
/// Raspberry Pi; neither has a meaningful non-Linux equivalent. The pure
/// packet-compression, thermal-payload, and socket-permission code above
/// still builds and is still tested everywhere, so the crate stays usable
/// as a development target on a workstation.
#[cfg(not(target_os = "linux"))]
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    Err(
        "starlight's capture and framebuffer runtime requires Linux (Raspberry Pi + Sense HAT)"
            .into(),
    )
}
