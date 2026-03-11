use std::env;
use std::fs::{read_to_string, remove_file, OpenOptions};
use std::ffi::CString;
use std::io::{self, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use pnet::datalink::{self, Channel, Config};
use ndarray::{Array, Array1, Array2, s};
use std::f32::consts::PI;

const THERMAL_STATE_NORMAL: u8 = 0;
const THERMAL_STATE_WARNING: u8 = 1;
const THERMAL_STATE_CRITICAL: u8 = 2;

//use cpal::traits::HostTrait;
//use cpal::traits::DeviceTrait;

struct PacketCompressor {
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
    /// Initialize the neural network with predefined weights
    fn new(input_size: usize, compressed_size: usize) -> Self {
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

    /// Generate weights with a sinusoidal pattern
    fn generate_waveform_weights(rows: usize, cols: usize) -> Array2<f32> {
        Array2::from_shape_fn((rows, cols), |(i, j)| {
           0.11389 + (i as f32 / rows as f32 * 2.0 * PI).sin() * (j as f32 / cols as f32 * 2.0 * PI).cos()
        })
    }

    /// Generate biases with a simple sine wave pattern
    fn generate_waveform_biases(size: usize) -> Array1<f32> {
        Array1::from_shape_fn(size, |i| (i as f32 / size as f32 * 2.11793 * PI).sin())
    }

    /// Generate weights with a parabolic pattern
    fn generate_parabolic_weights(rows: usize, cols: usize) -> Array2<f32> {
        Array2::from_shape_fn((rows, cols), |(i, j)| {
            let x = i as f32 / rows as f32; // Normalize row index to [0, 1]
            let y = j as f32 / cols as f32; // Normalize column index to [0, 1]
            (x - 0.5).powi(2) + (y - 0.5).powi(2) // Parabolic equation
        })
    }

    /// Apply the neural network to compress input data
    fn compress(&self, input: Array1<f32>) -> Array1<f32> {
        // Layer 1: input -> ReLU(Wx + b)
        let mut hidden1 = input.dot(&self.weights1) + &self.biases1;
        hidden1.mapv_inplace(|x| x.max(0.0)); // ReLU activation

        // Layer 2: hidden1 -> ReLU(Wx + b)
        let mut hidden2 = hidden1.dot(&self.weights2) + &self.biases2;
        hidden2.mapv_inplace(|x| x.max(0.0)); // ReLU activation

        let mut hidden3 = hidden2.dot(&self.weights3) + &self.biases3;
        hidden3.mapv_inplace(|x| x.max(0.0));

        // Layer 4: hidden3 -> Wx + b
        hidden3.dot(&self.weights4) + &self.biases4
    }
}

/// Convert &[u8] to an Array1<f32> and pad with zeros to 1518 elements
fn bytes_to_f32_vector(bytes: &[u8]) -> Array1<f32> {
    const REQUIRED_SIZE: usize = 1518;

    // Convert bytes to f32 in the range [0.0, 1.0]
    let floats: Vec<f32> = bytes.iter().map(|&byte| byte as f32 / 255.0).collect();

    if floats.len() >= REQUIRED_SIZE {
        // If the length is greater than or equal to REQUIRED_SIZE, truncate it
        Array1::from(floats[..REQUIRED_SIZE].to_vec())
    } else {
        // If the length is less than REQUIRED_SIZE, pad with zeros
        let mut padded = Array::zeros(REQUIRED_SIZE);
        padded.slice_mut(s![..floats.len()]).assign(&Array1::from(floats));
        padded
    }
}

fn parse_temp_env(var_name: &str, default: f32) -> f32 {
    env::var(var_name)
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .filter(|value| *value > 0.0 && value.is_finite())
        .unwrap_or(default)
}

fn parse_temp_interval_env(var_name: &str, default: u64) -> u64 {
    env::var(var_name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn read_cpu_temperature_c() -> Option<f32> {
    const TEMP_PATHS: [&str; 2] = [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/thermal/thermal_zone1/temp",
    ];

    for path in TEMP_PATHS {
        if let Ok(raw) = read_to_string(path) {
            if let Ok(parsed) = raw.trim().parse::<f32>() {
                return Some(if parsed > 200.0 { parsed / 1000.0 } else { parsed });
            }
        }
    }

    None
}

fn thermal_state_name(state: u8) -> &'static str {
    match state {
        THERMAL_STATE_WARNING => "warning",
        THERMAL_STATE_CRITICAL => "critical",
        _ => "normal",
    }
}

fn thermal_recommendation(state: u8) -> &'static str {
    match state {
        THERMAL_STATE_WARNING => "throttle",
        THERMAL_STATE_CRITICAL => "pause",
        _ => "normal",
    }
}

fn unix_timestamp_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone)]
struct ThermalSignalPublisher {
    clients: Arc<Mutex<Vec<UnixStream>>>,
}

impl ThermalSignalPublisher {
    fn emit(&self, payload: &str) {
        let mut clients = match self.clients.lock() {
            Ok(clients) => clients,
            Err(err) => {
                eprintln!("Unable to lock thermal signal client list: {}", err);
                return;
            }
        };

        let mut next_clients = Vec::with_capacity(clients.len());
        for mut client in clients.drain(..) {
            if client.write_all(payload.as_bytes()).is_ok() && client.write_all(b"\n").is_ok() {
                next_clients.push(client);
            }
        }
        *clients = next_clients;
    }
}

fn thermal_payload(
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

fn emit_thermal_signal(
    publisher: &ThermalSignalPublisher,
    state: u8,
    temperature_c: Option<f32>,
    warn_temp_c: f32,
    critical_temp_c: f32,
) {
    publisher.emit(&thermal_payload(
        state,
        temperature_c,
        warn_temp_c,
        critical_temp_c,
    ));
}

fn initialize_signal_publisher(
    path: &str,
    owner: &str,
    running: Arc<AtomicBool>,
) -> io::Result<(ThermalSignalPublisher, JoinHandle<()>)> {
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

    let clients = Arc::new(Mutex::new(Vec::new()));
    let accept_clients = Arc::clone(&clients);
    let accept_handle = thread::spawn(move || {
        while running.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Ok(mut clients) = accept_clients.lock() {
                        clients.push(stream);
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    eprintln!("Thermal signal socket accept error: {}", err);
                    break;
                }
            }
        }
    });

    Ok((ThermalSignalPublisher { clients }, accept_handle))
}

fn lookup_user_ids(username: &str) -> io::Result<(u32, u32)> {
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

fn ensure_socket_owner(path: &str, username: &str) -> io::Result<()> {
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
/*     let host = cpal::default_host();

    match host.devices() {
        Ok(devices) => {
            println!("Available audio devices:");
            for (index, device) in devices.enumerate() {
                match device.name() {
                    Ok(name) => println!("{}. {}", index + 1, name),
                    Err(err) => println!("{}. <Unknown Device> (Error: {:?})", index + 1, err),
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to get devices: {:?}", err);
        }
    }

    match host.input_devices() {
        Ok(devices) => {
            println!("Input-capable audio devices:");
            for (index, device) in devices.enumerate() {
                match device.name() {
                    Ok(name) => println!("{}. {}", index + 1, name),
                    Err(err) => println!("{}. <Unknown Device> (Error: {:?})", index + 1, err),
                }
            }
        }
        Err(err) => {
            eprintln!("Failed to get input devices: {:?}", err);
        }
    } */
    let mut args = env::args().skip(1);
    let interface_name = args
        .next()
        .expect("Usage: cargo run <interface_name> <framebuffer_name>");
    let fbuffer = args
        .next()
        .expect("Usage: cargo run <interface_name> <framebuffer_name>");

    let warn_temp_c = parse_temp_env("STARLIGHT_WARN_TEMP_C", 80.0);
    let critical_temp_c = parse_temp_env("STARLIGHT_CRIT_TEMP_C", 85.0);
    let temp_check_interval = Duration::from_secs(parse_temp_interval_env(
        "STARLIGHT_TEMP_CHECK_INTERVAL_SECS",
        5,
    ));
    let signal_socket_path = env::var("STARLIGHT_SIGNAL_SOCKET")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let signal_socket_owner = env::var("STARLIGHT_SIGNAL_SOCKET_OWNER")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "starlight".to_string());
    let running = Arc::new(AtomicBool::new(true));
    let signal_publisher_setup = if let Some(path) = signal_socket_path.as_deref() {
        match initialize_signal_publisher(path, &signal_socket_owner, Arc::clone(&running)) {
            Ok(parts) => Some(parts),
            Err(err) => {
                eprintln!("Unable to initialize signal socket publisher on {}: {}", path, err);
                None
            }
        }
    } else {
        None
    };

    if critical_temp_c <= warn_temp_c {
        return Err(format!(
            "Environment config invalid: STARLIGHT_CRIT_TEMP_C ({critical_temp_c:.1}) must be higher than STARLIGHT_WARN_TEMP_C ({warn_temp_c:.1})"
        )
        .into());
    }

    if let Some(start_temp) = read_cpu_temperature_c() {
        println!(
            "Starting Starlight. Current CPU temperature: {:.1}°C",
            start_temp
        );
    }

    println!(
        "CPU monitor enabled: warning at {:.1}°C, shutdown at {:.1}°C (interval: {:?})",
        warn_temp_c,
        critical_temp_c,
        temp_check_interval
    );
    if let Some(path) = signal_socket_path.as_deref() {
        println!("Thermal signal publisher listening on: {}", path);
    }

    let thermal_state_arc = Arc::new(AtomicU8::new(THERMAL_STATE_NORMAL));
    let monitor_running = running.clone();
    let monitor_state = thermal_state_arc.clone();
    let monitor_signal_publisher = signal_publisher_setup
        .as_ref()
        .map(|(publisher, _)| publisher.clone());
    let monitor = thread::spawn(move || {
        let mut warned = false;
        let mut last_emitted_state = THERMAL_STATE_NORMAL;
        let mut last_signal = Instant::now()
            .checked_sub(temp_check_interval)
            .unwrap_or_else(Instant::now);

    while monitor_running.load(Ordering::Acquire) {
        let mut current_temp: Option<f32> = None;
        if let Some(temperature_c) = read_cpu_temperature_c() {
            current_temp = Some(temperature_c);
            if temperature_c >= critical_temp_c {
                monitor_state.store(THERMAL_STATE_CRITICAL, Ordering::Release);
                eprintln!(
                    "CRITICAL: CPU temperature {:.1}°C >= {:.1}°C, stopping capture",
                    temperature_c,
                    critical_temp_c
                );
                if let Some(publisher) = monitor_signal_publisher.as_ref() {
                    emit_thermal_signal(
                        publisher,
                        THERMAL_STATE_CRITICAL,
                        current_temp,
                        warn_temp_c,
                        critical_temp_c,
                    );
                }
                monitor_running.store(false, Ordering::Release);
                break;
            }

            if temperature_c >= warn_temp_c {
                monitor_state.store(THERMAL_STATE_WARNING, Ordering::Release);
                if !warned {
                    eprintln!(
                        "WARNING: CPU temperature {:.1}°C >= {:.1}°C",
                        temperature_c,
                        warn_temp_c
                    );
                    warned = true;
                }
            } else if warned {
                monitor_state.store(THERMAL_STATE_NORMAL, Ordering::Release);
                println!("CPU temperature recovered to {:.1}°C", temperature_c);
                warned = false;
            }
        } else {
            eprintln!("Unable to read CPU temperature from /sys/class/thermal/*/temp");
        }

        let current_state = monitor_state.load(Ordering::Acquire);
        let should_emit = current_state != last_emitted_state || last_signal.elapsed() >= temp_check_interval;
        if should_emit {
            if let Some(publisher) = monitor_signal_publisher.as_ref() {
                emit_thermal_signal(
                    publisher,
                    current_state,
                    current_temp,
                    warn_temp_c,
                    critical_temp_c,
                );
            }
            last_signal = Instant::now();
            last_emitted_state = current_state;
        }

        thread::sleep(temp_check_interval);
    }

    });

    let mut fb = OpenOptions::new()
        .write(true)
        .open(fbuffer)?; // Adjust if your framebuffer is not fb1

    // // Define a simple RGB pattern for the 8x8 LED matrix
    let mut buffer = [0u8; 128]; // 8x8 RGB matrix (8 rows x 8 columns x 3 bytes per LED)

    // Fill buffer with colors (red, green, blue)
    for i in 0..8 {
        buffer[i * 2] = 0; // Red
        buffer[i * 2 + 1] = 0x0F; // Green
    }

    fb.write_all(&buffer)?;
    fb.seek(SeekFrom::Start(0))?;
    thread::sleep(Duration::from_secs(3));

    let clear = [0, 0].repeat(64); // All LEDs red
   
    fb.write_all(&clear)?;
    fb.seek(SeekFrom::Start(0))?;

    let buffer = [255, 0].repeat(64); // All LEDs red
    // Send buffer to Sense HAT
    fb.write_all(&buffer)?;
    fb.seek(SeekFrom::Start(0))?;

    // Keep the LEDs lit for 5 seconds
    thread::sleep(Duration::from_secs(3));

    // Turn off the LEDs by sending a buffer of zeros
    fb.write_all(&[0; 128])?;
    fb.seek(SeekFrom::Start(0))?;

    // Find the network interface by name
    let interfaces = datalink::interfaces();
    let interface = interfaces
        .into_iter()
        .find(|iface| iface.name == interface_name)
        .expect("Interface not found");

    // Configure the interface for promiscuous mode
    let mut config = Config::default();
    config.promiscuous = true;
    config.read_timeout = Some(temp_check_interval);

    // Create a datalink channel to capture packets
    let mut channel = match datalink::channel(&interface, config) {
        Ok(Channel::Ethernet(_tx, rx)) => rx,
        Ok(_) => panic!("Unhandled channel type"),
        Err(e) => panic!("Failed to create datalink channel: {}", e),
    };

    println!("Listening on interface: {}", interface_name);

    // Input packet size and compressed output size
    let input_size = 1518; // Max Ethernet packet size
    let compressed_size = 32; // Compressed size for Sense HAT

    // Create the compressor network
    let compressor = PacketCompressor::new(input_size, compressed_size);
    let mut warning_pulse = false;
    let mut last_warning_pulse_toggle = Instant::now();
    let warning_pulse_rate = Duration::from_millis(350);
    let yellow_frame: [u8; 128] = {
        let mut frame = [0u8; 128];
        for i in (0..64).step_by(1) {
            frame[i * 2] = 0xFF;
            frame[i * 2 + 1] = 0xF0;
        }
        frame
    };
    let red_frame: [u8; 128] = {
        let mut frame = [0u8; 128];
        for i in (0..64).step_by(1) {
            frame[i * 2] = 0xFF;
        }
        frame
    };
    let clear_frame = [0u8; 128];

    // Capture and process packets
    loop {
        let current_thermal_state = thermal_state_arc.load(Ordering::Acquire);
        if !running.load(Ordering::Acquire) {
            if current_thermal_state == THERMAL_STATE_CRITICAL {
                if last_warning_pulse_toggle.elapsed() >= warning_pulse_rate {
                    warning_pulse = !warning_pulse;
                }

                let frame = if warning_pulse {
                    &red_frame
                } else {
                    &clear_frame
                };
                fb.write_all(frame)?;
                fb.seek(SeekFrom::Start(0))?;
            }
            break;
        }

        let current_thermal_state = thermal_state_arc.load(Ordering::Acquire);
        let mut packet_data: Option<Array1<f32>> = None;

        match channel.next() {
            Ok(packet) => {
                if current_thermal_state == THERMAL_STATE_NORMAL {
                    let compressed = compressor.compress(bytes_to_f32_vector(packet));
                    packet_data = Some(compressed);
                }
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::TimedOut {
                    eprintln!("Failed to read packet: {}", e);
                }
            }
        }

        let current_thermal_state = thermal_state_arc.load(Ordering::Acquire);
        if current_thermal_state != THERMAL_STATE_NORMAL {
            if last_warning_pulse_toggle.elapsed() >= warning_pulse_rate {
                warning_pulse = !warning_pulse;
                last_warning_pulse_toggle = Instant::now();
            }

            let frame = if warning_pulse {
                match current_thermal_state {
                    THERMAL_STATE_WARNING => &yellow_frame,
                    THERMAL_STATE_CRITICAL => &red_frame,
                    _ => &clear_frame,
                }
            } else {
                &clear_frame
            };
            fb.write_all(frame)?;
            fb.seek(SeekFrom::Start(0))?;
            continue;
        }

        if let Some(compressed) = packet_data {
            let buffer: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    compressed.as_ptr() as *const u8,
                    compressed.len() * std::mem::size_of::<f32>(),
                )
            };
            fb.write_all(&buffer)?;
            fb.seek(SeekFrom::Start(0))?;
        }
    }

    running.store(false, Ordering::Release);
    if let Err(err) = monitor.join() {
        eprintln!("Temperature monitor thread failed: {:?}", err);
    }
    if let Some((_, accept_thread)) = signal_publisher_setup {
        if let Err(err) = accept_thread.join() {
            eprintln!("Thermal signal accept thread failed: {:?}", err);
        }
    }
    if let Some(path) = signal_socket_path.as_deref() {
        let _ = remove_file(path);
    }

    Ok(())
}
