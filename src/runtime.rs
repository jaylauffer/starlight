//! The Linux runtime: every I/O source starlight has, driven by one
//! `Proactor<IoUringPort>`.
//!
//! ## What this replaced
//!
//! The previous runtime ran three OS threads and a blocking loop:
//!
//! | before | now |
//! | --- | --- |
//! | accept thread polling a non-blocking `accept()` behind `thread::sleep(100ms)` | `IoPort::accept`, re-armed from its own completion |
//! | thermal thread looping on `thread::sleep(interval)` | `ProactorHandle::defer_for`, re-armed from its own completion |
//! | blocking `pnet` `channel.next()` on the main thread | `IoPort::recv` on an `AF_PACKET` socket, re-armed from its own completion |
//! | blocking `write_all` + `seek(0)` per frame | `IoPort::write` at an explicit offset |
//! | blocking `write_all` per subscriber | `IoPort::send`, see [`crate::ThermalSignalPublisher`] |
//!
//! The whole process is now one thread parked in
//! `Proactor::run_until_stopped`.
//!
//! ## Why `pnet` is gone
//!
//! `pnet::datalink::channel()` hands back a `Box<dyn DataLinkReceiver>`
//! that never exposes the underlying descriptor, so there is nothing to
//! give io_uring. starlight only ever used `pnet` for that one call --
//! it does no packet parsing with it, since `packet_to_frame_bytes`
//! takes a plain `&[u8]` -- so opening the `AF_PACKET` socket directly
//! against `libc` (already a dependency) removes the dependency rather
//! than trading it for a bigger one.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use loadngo_proactor::{
    AcceptResult, CompletionKind, IoBuf, IoResult, IoUringPort, Proactor, ProactorHandle,
};

use crate::{
    bind_signal_socket, emit_thermal_signal, lookup_user_ids, packet_to_frame_bytes,
    read_cpu_temperature_c, Config, PacketCompressor, ThermalSignalPublisher,
    COMPRESSED_PACKET_SIZE, FRAMEBUFFER_SIZE_BYTES, PACKET_VECTOR_SIZE, THERMAL_STATE_CRITICAL,
    THERMAL_STATE_NORMAL, THERMAL_STATE_WARNING,
};

/// How long to wait before retrying a failed accept, so a persistently
/// broken listener cannot spin the proactor thread.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Sized to the largest frame `bytes_to_f32_vector` will look at; it
/// truncates anything longer anyway.
const CAPTURE_BUFFER_BYTES: usize = PACKET_VECTOR_SIZE;

/// Framebuffer writes always target the start of the 8x8 matrix. The old
/// code expressed this as `write_all` followed by `seek(0)`; `IoPort`
/// takes the offset directly, so the seek is gone.
const FRAMEBUFFER_OFFSET: u64 = 0;

const PULSE_INTERVAL: Duration = Duration::from_millis(350);

/// How many red pulses to show after crossing the critical threshold
/// before shutting down. `docs/RESILIENCE_PLAN.md` asks for a short
/// cooldown grace rather than an instant exit, and the LED matrix is the
/// only feedback channel a headless Pi has.
const CRITICAL_PULSES_BEFORE_EXIT: u8 = 6;

pub fn serve(config: Config) -> Result<(), Box<dyn std::error::Error>> {
    let Config {
        interface_name,
        framebuffer_path,
        warn_temp_c,
        critical_temp_c,
        temp_check_interval,
        min_frame_interval,
        signal_socket_path,
        signal_socket_owner,
    } = config;

    let signal_socket = match signal_socket_path.as_deref() {
        Some(path) => {
            let (uid, gid) = lookup_user_ids(&signal_socket_owner)?;
            println!(
                "Initializing thermal signal socket: path={} owner={} uid={} gid={} mode=0660",
                path, signal_socket_owner, uid, gid
            );
            Some(
                bind_signal_socket(path, &signal_socket_owner).map_err(|err| {
                    io::Error::new(
                        err.kind(),
                        format!(
                            "unable to initialize signal socket publisher on {} for owner {}: {}",
                            path, signal_socket_owner, err
                        ),
                    )
                })?,
            )
        }
        None => None,
    };

    if let Some(start_temp) = read_cpu_temperature_c() {
        println!(
            "Starting Starlight. Current CPU temperature: {:.1}°C",
            start_temp
        );
    }
    println!(
        "CPU monitor enabled: warning at {:.1}°C, shutdown at {:.1}°C (interval: {:?})",
        warn_temp_c, critical_temp_c, temp_check_interval
    );
    if let Some(path) = signal_socket_path.as_deref() {
        println!(
            "Thermal signal publisher listening on: {} (owner: {}, mode: 0660)",
            path, signal_socket_owner
        );
    }

    // The startup splash stays synchronous on purpose: it runs before the
    // proactor owns anything, nothing else is in flight yet, and turning
    // six seconds of deliberate blocking into a deferred chain would add
    // machinery without changing what the operator sees.
    let mut framebuffer = open_framebuffer(&framebuffer_path)?;
    run_startup_splash(&mut framebuffer)?;
    let framebuffer = OwnedFd::from(framebuffer);

    let capture = open_capture_socket(&interface_name)?;

    let proactor = Proactor::new(IoUringPort::new()?);
    let handle = proactor.handle();

    let (listener, publisher) = match signal_socket {
        Some((listener, publisher)) => (Some(Arc::new(listener)), Some(publisher)),
        None => (None, None),
    };

    let state = Arc::new(RuntimeState {
        handle: handle.clone(),
        capture,
        framebuffer,
        // Held for the proactor's lifetime so the listener fd stays open
        // for as long as an accept is outstanding against it.
        listener: listener.clone(),
        publisher,
        compressor: PacketCompressor::new(PACKET_VECTOR_SIZE, COMPRESSED_PACKET_SIZE),
        thermal_state: AtomicU8::new(THERMAL_STATE_NORMAL),
        warned: AtomicBool::new(false),
        pulse_on: AtomicBool::new(false),
        pulse_armed: AtomicBool::new(false),
        critical_pulses_left: AtomicU8::new(0),
        warn_temp_c,
        critical_temp_c,
        temp_check_interval,
        min_frame_interval,
        last_frame_at: Mutex::new(None),
    });

    state.arm_accept();
    state.arm_thermal_tick();
    state.arm_capture();

    println!("Listening on interface: {}", interface_name);

    proactor.run_until_stopped()?;

    if let Some(path) = signal_socket_path.as_deref() {
        let _ = std::fs::remove_file(path);
    }

    Ok(())
}

struct RuntimeState {
    handle: ProactorHandle<IoUringPort>,
    capture: OwnedFd,
    framebuffer: OwnedFd,
    listener: Option<Arc<UnixListener>>,
    publisher: Option<ThermalSignalPublisher>,
    compressor: PacketCompressor,
    thermal_state: AtomicU8,
    warned: AtomicBool,
    pulse_on: AtomicBool,
    pulse_armed: AtomicBool,
    critical_pulses_left: AtomicU8,
    warn_temp_c: f32,
    critical_temp_c: f32,
    temp_check_interval: Duration,
    /// Shortest gap between rendered frames; see [`crate::Config`].
    min_frame_interval: Duration,
    /// When the last frame was actually rendered. `None` until the first.
    last_frame_at: Mutex<Option<Instant>>,
}

impl RuntimeState {
    /// Submits one `accept` on the thermal signal listener, re-arming
    /// from its own completion the same way capture does.
    ///
    /// This was readiness plus a plain `accept()` until `IoPort::accept`
    /// learned to report non-IP peers: it used to resolve the peer as a
    /// `std::net::SocketAddr` and, for an `AF_UNIX` connection, return an
    /// error while dropping the accepted descriptor unclosed. With
    /// `PeerAddr` in place the socket runs on true proactor semantics
    /// like everything else in this process.
    fn arm_accept(self: &Arc<Self>) {
        let Some(listener) = self.listener.as_ref() else {
            return;
        };

        let state = Arc::clone(self);
        let submitted = self
            .handle
            .accept(listener.as_raw_fd(), move |result: AcceptResult| {
                state.on_accept(result);
            });

        if let Err(err) = submitted {
            eprintln!("Unable to submit thermal socket accept: {}", err);
        }
    }

    fn on_accept(self: &Arc<Self>, result: AcceptResult) {
        match result {
            Ok(transfer) => {
                match self.publisher.as_ref() {
                    // SAFETY: the descriptor came from a completed accept
                    // and is owned by this handler; wrapping it transfers
                    // that ownership to the publisher, which closes it
                    // when the client is dropped.
                    Some(publisher) => {
                        publisher.add_client(unsafe { UnixStream::from_raw_fd(transfer.new_fd) })
                    }
                    None => drop(unsafe { UnixStream::from_raw_fd(transfer.new_fd) }),
                }
                self.arm_accept();
            }
            Err(err) => {
                // Re-arm on a delay rather than immediately. A listener
                // that fails every accept -- a closed or broken fd --
                // would otherwise spin this thread at full speed, which
                // is exactly the tight retry loop docs/RESILIENCE_PLAN.md
                // warns against under load.
                eprintln!("Thermal signal socket accept error: {}", err);
                let state = Arc::clone(self);
                let scheduled = self.handle.defer_for(
                    ACCEPT_RETRY_DELAY,
                    CompletionKind::Timer,
                    0,
                    move |_| {
                        state.arm_accept();
                    },
                );
                if let Err(err) = scheduled {
                    eprintln!("Unable to reschedule thermal socket accept: {}", err);
                }
            }
        }
    }

    /// Submits one `recv` on the capture socket. The completion handler
    /// calls this again, so capture is a self-sustaining chain of kernel
    /// operations rather than a loop that blocks a thread.
    ///
    /// Re-arming from inside a completion is safe by construction:
    /// `IoUringPort::submit_or_defer` takes the ring with `try_lock` and
    /// falls back to its `pending_ops` queue plus a wake, so a handler
    /// running while the loop holds the ring cannot deadlock.
    fn arm_capture(self: &Arc<Self>) {
        self.arm_capture_with(IoBuf::with_capacity(CAPTURE_BUFFER_BYTES));
    }

    fn arm_capture_with(self: &Arc<Self>, buf: IoBuf) {
        let state = Arc::clone(self);
        let submitted = self
            .handle
            .recv(self.capture.as_raw_fd(), buf, move |result: IoResult| {
                state.on_packet(result);
            });

        if let Err(err) = submitted {
            eprintln!("Unable to submit packet capture recv: {}", err);
            let _ = self.handle.stop();
        }
    }

    fn on_packet(self: &Arc<Self>, result: IoResult) {
        // A critical reading stops capture for good; don't re-arm.
        if self.thermal_state.load(Ordering::Acquire) == THERMAL_STATE_CRITICAL {
            return;
        }

        let buf = match result {
            Ok(transfer) => {
                // Reuse the capture allocation rather than handing back a
                // fresh 1518-byte buffer on every single packet.
                let mut raw = transfer.buf.into_vec();
                if self.thermal_state.load(Ordering::Acquire) == THERMAL_STATE_NORMAL
                    && self.claim_frame_slot()
                {
                    let frame = packet_to_frame_bytes(&self.compressor, &raw);
                    self.write_frame(frame.to_vec());
                }
                // Warm/warning states keep capturing but leave the matrix
                // to the pulse timer.
                raw.clear();
                raw.resize(CAPTURE_BUFFER_BYTES, 0);
                IoBuf::from_vec(raw)
            }
            Err(err) => {
                if err.kind() != io::ErrorKind::WouldBlock
                    && err.kind() != io::ErrorKind::TimedOut
                    && err.kind() != io::ErrorKind::Interrupted
                {
                    eprintln!("Failed to read packet: {}", err);
                }
                IoBuf::with_capacity(CAPTURE_BUFFER_BYTES)
            }
        };

        self.arm_capture_with(buf);
    }

    /// Whether this packet is allowed to become a frame.
    ///
    /// Capture is promiscuous, so without this the projection ran for
    /// every packet on the segment — work set by other people's traffic
    /// rather than by anything starlight needs. An 8x8 matrix shows
    /// nothing useful above a few frames per second, so packets arriving
    /// inside `min_frame_interval` are dropped before the expensive part.
    ///
    /// Returns true (and claims the slot) only when enough time has
    /// passed. A zero interval disables the gate entirely.
    fn claim_frame_slot(self: &Arc<Self>) -> bool {
        if self.min_frame_interval.is_zero() {
            return true;
        }
        let Ok(mut last) = self.last_frame_at.lock() else {
            return true;
        };
        let now = Instant::now();
        match *last {
            Some(previous) if now.duration_since(previous) < self.min_frame_interval => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    /// Queues one framebuffer write. The buffer is handed to the kernel
    /// and comes back through the completion, so nothing here blocks.
    fn write_frame(self: &Arc<Self>, frame: Vec<u8>) {
        let submitted = self.handle.write(
            self.framebuffer.as_raw_fd(),
            IoBuf::from_vec(frame),
            FRAMEBUFFER_OFFSET,
            move |result: IoResult| {
                if let Err(err) = result {
                    eprintln!("Framebuffer write failed: {}", err);
                }
            },
        );

        if let Err(err) = submitted {
            eprintln!("Unable to submit framebuffer write: {}", err);
        }
    }

    fn arm_thermal_tick(self: &Arc<Self>) {
        let state = Arc::clone(self);
        let submitted = self.handle.defer_for(
            self.temp_check_interval,
            CompletionKind::Timer,
            0,
            move |_| {
                state.on_thermal_tick();
            },
        );

        if let Err(err) = submitted {
            eprintln!("Unable to schedule thermal check: {}", err);
            let _ = self.handle.stop();
        }
    }

    fn on_thermal_tick(self: &Arc<Self>) {
        let current_temp = read_cpu_temperature_c();
        match current_temp {
            Some(temperature_c) if temperature_c >= self.critical_temp_c => {
                self.thermal_state
                    .store(THERMAL_STATE_CRITICAL, Ordering::Release);
                eprintln!(
                    "CRITICAL: CPU temperature {:.1}°C >= {:.1}°C, stopping capture",
                    temperature_c, self.critical_temp_c
                );
                self.critical_pulses_left
                    .store(CRITICAL_PULSES_BEFORE_EXIT, Ordering::Release);
                self.publish(current_temp);
                self.arm_pulse();
                // No further thermal ticks: the pulse chain owns shutdown.
                return;
            }
            Some(temperature_c) if temperature_c >= self.warn_temp_c => {
                self.thermal_state
                    .store(THERMAL_STATE_WARNING, Ordering::Release);
                if !self.warned.swap(true, Ordering::AcqRel) {
                    eprintln!(
                        "WARNING: CPU temperature {:.1}°C >= {:.1}°C",
                        temperature_c, self.warn_temp_c
                    );
                }
                self.arm_pulse();
            }
            Some(temperature_c) => {
                self.thermal_state
                    .store(THERMAL_STATE_NORMAL, Ordering::Release);
                if self.warned.swap(false, Ordering::AcqRel) {
                    println!("CPU temperature recovered to {:.1}°C", temperature_c);
                }
            }
            None => {
                eprintln!("Unable to read CPU temperature from /sys/class/thermal/*/temp");
            }
        }

        self.publish(current_temp);
        self.arm_thermal_tick();
    }

    /// Emits a thermal status line once per check interval and on every
    /// state change -- the cadence `README.md` documents.
    fn publish(self: &Arc<Self>, temperature_c: Option<f32>) {
        let Some(publisher) = self.publisher.as_ref() else {
            return;
        };
        emit_thermal_signal(
            publisher,
            &self.handle,
            self.thermal_state.load(Ordering::Acquire),
            temperature_c,
            self.warn_temp_c,
            self.critical_temp_c,
        );
    }

    /// Starts the warning/critical pulse if it isn't already running.
    ///
    /// The old code toggled this inside the capture loop, so the matrix
    /// only pulsed as fast as packets happened to arrive -- and not at
    /// all on an idle link. Driving it from its own deferred timer makes
    /// the cadence independent of traffic.
    fn arm_pulse(self: &Arc<Self>) {
        if self.pulse_armed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.schedule_pulse();
    }

    fn schedule_pulse(self: &Arc<Self>) {
        let state = Arc::clone(self);
        let submitted =
            self.handle
                .defer_for(PULSE_INTERVAL, CompletionKind::Timer, 0, move |_| {
                    state.on_pulse();
                });

        if let Err(err) = submitted {
            eprintln!("Unable to schedule thermal pulse: {}", err);
            self.pulse_armed.store(false, Ordering::Release);
        }
    }

    fn on_pulse(self: &Arc<Self>) {
        let current = self.thermal_state.load(Ordering::Acquire);
        if current == THERMAL_STATE_NORMAL {
            // Recovered: clear the matrix and let capture drive it again.
            self.pulse_armed.store(false, Ordering::Release);
            self.pulse_on.store(false, Ordering::Release);
            self.write_frame(vec![0u8; FRAMEBUFFER_SIZE_BYTES]);
            return;
        }

        let lit = !self.pulse_on.fetch_xor(true, Ordering::AcqRel);
        self.write_frame(if lit {
            pulse_frame(current)
        } else {
            vec![0u8; FRAMEBUFFER_SIZE_BYTES]
        });

        if current == THERMAL_STATE_CRITICAL {
            let left = self.critical_pulses_left.load(Ordering::Acquire);
            if left <= 1 {
                let _ = self.handle.stop();
                return;
            }
            self.critical_pulses_left.store(left - 1, Ordering::Release);
        }

        self.schedule_pulse();
    }
}

/// 16bpp RGB565, 64 pixels: yellow while warning, red once critical.
/// Byte-for-byte the frames the previous implementation built inline.
fn pulse_frame(state: u8) -> Vec<u8> {
    let mut frame = vec![0u8; FRAMEBUFFER_SIZE_BYTES];
    match state {
        THERMAL_STATE_WARNING => {
            for i in 0..64 {
                frame[i * 2] = 0xFF;
                frame[i * 2 + 1] = 0xF0;
            }
        }
        THERMAL_STATE_CRITICAL => {
            for i in 0..64 {
                frame[i * 2] = 0xFF;
            }
        }
        _ => {}
    }
    frame
}

fn open_framebuffer(path: &str) -> io::Result<File> {
    OpenOptions::new().write(true).open(path).map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "unable to open framebuffer {path}: {err} \
                 (the Sense HAT matrix is the /dev/fb* whose \
                 /sys/class/graphics/<fb>/name reads \"RPi-Sense FB\"; \
                 the index is not stable across boots or HDMI state)"
            ),
        )
    })
}

/// The original power-on sequence: blue bars, red wash, clear.
fn run_startup_splash(framebuffer: &mut File) -> io::Result<()> {
    let mut bars = [0u8; FRAMEBUFFER_SIZE_BYTES];
    for i in 0..8 {
        bars[i * 2] = 0;
        bars[i * 2 + 1] = 0x0F;
    }
    write_at_start(framebuffer, &bars)?;
    std::thread::sleep(Duration::from_secs(3));

    write_at_start(framebuffer, &[0u8; FRAMEBUFFER_SIZE_BYTES])?;

    let red = [255u8, 0].repeat(64);
    write_at_start(framebuffer, &red)?;
    std::thread::sleep(Duration::from_secs(3));

    write_at_start(framebuffer, &[0u8; FRAMEBUFFER_SIZE_BYTES])
}

fn write_at_start(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

/// Opens a promiscuous `AF_PACKET`/`SOCK_RAW` socket bound to one
/// interface -- the capture path `pnet::datalink::channel()` used to set
/// up, minus the boxed receiver that hid the descriptor from io_uring.
fn open_capture_socket(interface_name: &str) -> io::Result<OwnedFd> {
    let protocol = (libc::ETH_P_ALL as u16).to_be() as libc::c_int;
    let raw = unsafe { libc::socket(libc::AF_PACKET, libc::SOCK_RAW, protocol) };
    if raw < 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("unable to open AF_PACKET capture socket: {err} (needs CAP_NET_RAW)"),
        ));
    }
    // Owned from here on, so every early return below closes it.
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };

    let c_name = CString::new(interface_name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name contains an interior NUL",
        )
    })?;
    let ifindex = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
    if ifindex == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("interface `{interface_name}` not found"),
        ));
    }

    let mut addr: libc::sockaddr_ll = unsafe { mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as libc::c_ushort;
    addr.sll_protocol = (libc::ETH_P_ALL as u16).to_be();
    addr.sll_ifindex = ifindex as libc::c_int;
    let rc = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            std::ptr::addr_of!(addr).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("unable to bind capture socket to `{interface_name}`: {err}"),
        ));
    }

    let mut mreq: libc::packet_mreq = unsafe { mem::zeroed() };
    mreq.mr_ifindex = ifindex as libc::c_int;
    mreq.mr_type = libc::PACKET_MR_PROMISC as libc::c_ushort;
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_PACKET,
            libc::PACKET_ADD_MEMBERSHIP,
            std::ptr::addr_of!(mreq).cast::<libc::c_void>(),
            mem::size_of::<libc::packet_mreq>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        return Err(io::Error::new(
            err.kind(),
            format!("unable to enable promiscuous mode on `{interface_name}`: {err}"),
        ));
    }

    Ok(socket)
}
