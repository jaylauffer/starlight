use std::env;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ndarray::{s, Array1};
use starlight::{
    bytes_to_f32_vector, ensure_socket_owner, initialize_signal_publisher, lookup_user_ids,
    parse_temp_env, parse_temp_interval_env, thermal_payload, thermal_recommendation,
    thermal_state_name, unix_timestamp_seconds, PacketCompressor, THERMAL_STATE_CRITICAL,
    THERMAL_STATE_NORMAL, THERMAL_STATE_WARNING,
};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn unique_path(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir()
        .join(format!("starlight-{name}-{nanos}-{}", std::process::id()))
        .to_string_lossy()
        .into_owned()
}

fn current_username() -> String {
    let uid = unsafe { libc::geteuid() };
    let passwd_ptr = unsafe { libc::getpwuid(uid) };
    assert!(!passwd_ptr.is_null(), "current uid should resolve to a passwd entry");
    let name = unsafe { std::ffi::CStr::from_ptr((*passwd_ptr).pw_name) };
    name.to_str().unwrap().to_string()
}

#[test]
fn waveform_weights_have_expected_shape_and_offset() {
    let weights = PacketCompressor::generate_waveform_weights(4, 3);
    assert_eq!(weights.dim(), (4, 3));
    assert!((weights[(0, 0)] - 0.11389).abs() < 1e-6);
}

#[test]
fn waveform_biases_start_at_zero() {
    let biases = PacketCompressor::generate_waveform_biases(8);
    assert_eq!(biases.len(), 8);
    assert!(biases[0].abs() < 1e-6);
}

#[test]
fn parabolic_weights_are_centered_lower_than_edges() {
    let weights = PacketCompressor::generate_parabolic_weights(4, 4);
    assert!(weights[(2, 2)] < weights[(0, 0)]);
    assert!(weights[(2, 2)] < weights[(0, 3)]);
}

#[test]
fn compressor_returns_expected_output_size() {
    let compressor = PacketCompressor::new(6, 2);
    let output = compressor.compress(Array1::from(vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]));
    assert_eq!(output.len(), 2);
    assert!(output.iter().all(|value| value.is_finite()));
}

#[test]
fn compressor_is_deterministic_for_same_input() {
    let compressor = PacketCompressor::new(5, 3);
    let input = Array1::from(vec![0.2, 0.4, 0.6, 0.8, 1.0]);
    let first = compressor.compress(input.clone());
    let second = compressor.compress(input);
    assert_eq!(first, second);
}

#[test]
fn bytes_to_f32_vector_pads_short_inputs() {
    let converted = bytes_to_f32_vector(&[0, 127, 255]);
    assert_eq!(converted.len(), 1518);
    assert_eq!(converted[0], 0.0);
    assert!((converted[1] - (127.0 / 255.0)).abs() < 1e-6);
    assert_eq!(converted[2], 1.0);
    assert!(converted.slice(s![3..]).iter().all(|value| *value == 0.0));
}

#[test]
fn bytes_to_f32_vector_truncates_long_inputs() {
    let bytes = vec![255u8; 1600];
    let converted = bytes_to_f32_vector(&bytes);
    assert_eq!(converted.len(), 1518);
    assert!(converted.iter().all(|value| *value == 1.0));
}

#[test]
fn parse_temp_env_uses_valid_override() {
    let _guard = env_lock().lock().unwrap();
    env::set_var("STARLIGHT_TEST_TEMP", "77.5");
    assert_eq!(parse_temp_env("STARLIGHT_TEST_TEMP", 80.0), 77.5);
    env::remove_var("STARLIGHT_TEST_TEMP");
}

#[test]
fn parse_temp_env_rejects_invalid_or_non_positive_values() {
    let _guard = env_lock().lock().unwrap();

    env::set_var("STARLIGHT_TEST_TEMP", "abc");
    assert_eq!(parse_temp_env("STARLIGHT_TEST_TEMP", 80.0), 80.0);

    env::set_var("STARLIGHT_TEST_TEMP", "-1");
    assert_eq!(parse_temp_env("STARLIGHT_TEST_TEMP", 80.0), 80.0);

    env::set_var("STARLIGHT_TEST_TEMP", "inf");
    assert_eq!(parse_temp_env("STARLIGHT_TEST_TEMP", 80.0), 80.0);

    env::remove_var("STARLIGHT_TEST_TEMP");
}

#[test]
fn parse_temp_interval_env_uses_valid_override() {
    let _guard = env_lock().lock().unwrap();
    env::set_var("STARLIGHT_TEST_INTERVAL", "9");
    assert_eq!(parse_temp_interval_env("STARLIGHT_TEST_INTERVAL", 5), 9);
    env::remove_var("STARLIGHT_TEST_INTERVAL");
}

#[test]
fn parse_temp_interval_env_rejects_invalid_or_zero_values() {
    let _guard = env_lock().lock().unwrap();

    env::set_var("STARLIGHT_TEST_INTERVAL", "0");
    assert_eq!(parse_temp_interval_env("STARLIGHT_TEST_INTERVAL", 5), 5);

    env::set_var("STARLIGHT_TEST_INTERVAL", "bad");
    assert_eq!(parse_temp_interval_env("STARLIGHT_TEST_INTERVAL", 5), 5);

    env::remove_var("STARLIGHT_TEST_INTERVAL");
}

#[test]
fn thermal_state_name_maps_all_states() {
    assert_eq!(thermal_state_name(THERMAL_STATE_NORMAL), "normal");
    assert_eq!(thermal_state_name(THERMAL_STATE_WARNING), "warning");
    assert_eq!(thermal_state_name(THERMAL_STATE_CRITICAL), "critical");
    assert_eq!(thermal_state_name(99), "normal");
}

#[test]
fn thermal_recommendation_maps_all_states() {
    assert_eq!(thermal_recommendation(THERMAL_STATE_NORMAL), "normal");
    assert_eq!(thermal_recommendation(THERMAL_STATE_WARNING), "throttle");
    assert_eq!(thermal_recommendation(THERMAL_STATE_CRITICAL), "pause");
    assert_eq!(thermal_recommendation(99), "normal");
}

#[test]
fn unix_timestamp_seconds_is_non_zero() {
    assert!(unix_timestamp_seconds() > 0);
}

#[test]
fn thermal_payload_formats_temperature_and_thresholds() {
    let payload = thermal_payload(THERMAL_STATE_WARNING, Some(81.25), 80.0, 85.0);
    assert!(payload.contains("\"state\":\"warning\""));
    assert!(payload.contains("\"temp_c\":81.2"));
    assert!(payload.contains("\"warn_c\":80.0"));
    assert!(payload.contains("\"crit_c\":85.0"));
    assert!(payload.contains("\"recommendation\":\"throttle\""));
    assert!(payload.contains("\"ts\":"));
}

#[test]
fn thermal_payload_formats_missing_temperature_as_null() {
    let payload = thermal_payload(THERMAL_STATE_NORMAL, None, 80.0, 85.0);
    assert!(payload.contains("\"temp_c\":null"));
    assert!(payload.contains("\"state\":\"normal\""));
}

#[test]
fn signal_publisher_emits_to_connected_clients() {
    let path = unique_path("emit");
    let running = Arc::new(AtomicBool::new(true));
    let owner = current_username();
    let (publisher, accept_thread) =
        initialize_signal_publisher(&path, &owner, Arc::clone(&running)).unwrap();

    let mut client = UnixStream::connect(&path).unwrap();
    thread::sleep(Duration::from_millis(150));

    publisher.emit("hello");

    let mut buf = [0u8; 6];
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"hello\n");

    running.store(false, Ordering::Release);
    accept_thread.join().unwrap();
    fs::remove_file(&path).unwrap();
}

#[test]
fn initialize_signal_publisher_rejects_regular_files() {
    let path = unique_path("nonsocket");
    File::create(&path).unwrap();
    let running = Arc::new(AtomicBool::new(true));
    let owner = current_username();

    let err = initialize_signal_publisher(&path, &owner, running)
        .err()
        .expect("regular file path should be rejected");
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);

    fs::remove_file(&path).unwrap();
}

#[test]
fn initialize_signal_publisher_replaces_stale_socket_path() {
    let path = unique_path("stale");
    let stale_listener = UnixListener::bind(&path).unwrap();
    drop(stale_listener);

    assert!(fs::metadata(&path).unwrap().file_type().is_socket());

    let running = Arc::new(AtomicBool::new(true));
    let owner = current_username();
    let (_publisher, accept_thread) =
        initialize_signal_publisher(&path, &owner, Arc::clone(&running)).unwrap();

    assert!(fs::metadata(&path).unwrap().file_type().is_socket());

    running.store(false, Ordering::Release);
    accept_thread.join().unwrap();
    fs::remove_file(&path).unwrap();
}

#[test]
fn lookup_user_ids_rejects_nul_in_username() {
    let err = lookup_user_ids("bad\0name").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn ensure_socket_owner_rejects_regular_files() {
    let path = unique_path("file-owner");
    File::create(&path).unwrap();

    let err = ensure_socket_owner(&path, &current_username()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

    fs::remove_file(&path).unwrap();
}
