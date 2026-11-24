//! Native PipeWire / PulseAudio system audio loopback for Wispr Flow Notetaker (`--capture-system-audio`).
//!
//! Connects directly to the default audio sink monitor (`@DEFAULT_MONITOR@`) via in-process
//! `libpulse-simple.so.0` (`dlopen`), capturing 16,000 Hz, 16-bit signed little-endian mono PCM
//! in 40ms frames (640 samples = 1,280 bytes) with zero child-process overhead.
//!
//! Emits `endpoint_opened\n` on `stderr` and streams 1,304-byte `WAF2` frames on `stdout`:
//!   - `0..4`:   ASCII `"WAF2"`
//!   - `4..8`:   `1280u32` (little-endian byte length of PCM payload)
//!   - `8..16`:  `sequence` (`u64` little-endian, monotonically increasing `0, 1, 2, ...`)
//!   - `16..24`: `capture_ticks_100ns` (`i64` little-endian, `CLOCK_MONOTONIC` in 100ns units minus 40ms)
//!   - `24..1304`: 1,280 bytes of `16,000 Hz` mono `s16le` PCM
//!
//! Respects the user's Notetaker Input Source preference in
//! `$XDG_CONFIG_HOME/Wispr Flow/notetaker-input-mode.json`:
//!   - `"both"` or `"system_only"`: streams live desktop/system audio
//!   - `"mic_only"`: streams zero-filled (silent) `WAF2` frames so Notetaker records microphone only
//!
//! Exits cleanly (`0`) as soon as `stdin` closes (when Electron calls `child.stdin.end()`).

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_S16LE: c_int = 3;
const SAMPLE_RATE: u32 = 16_000;
const CHANNELS: u8 = 1;
const FRAME_BYTES: usize = 1280; // 40ms @ 16kHz mono s16le
const HEADER_BYTES: usize = 24;
const PACKET_BYTES: usize = HEADER_BYTES + FRAME_BYTES; // 1304 bytes
const FRAME_TICKS_100NS: i64 = 400_000; // 40ms in 100-ns ticks

#[repr(C)]
struct PaSampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct PaBufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type PaSimpleNewFn = unsafe extern "C" fn(
    server: *const c_char,
    name: *const c_char,
    dir: c_int,
    dev: *const c_char,
    stream_name: *const c_char,
    ss: *const PaSampleSpec,
    map: *const c_void,
    attr: *const PaBufferAttr,
    error: *mut c_int,
) -> *mut c_void;

type PaSimpleReadFn = unsafe extern "C" fn(
    s: *mut c_void,
    data: *mut c_void,
    bytes: usize,
    error: *mut c_int,
) -> c_int;

type PaSimpleFreeFn = unsafe extern "C" fn(s: *mut c_void);

struct PulseSimpleApi {
    _lib: *mut c_void,
    simple_new: PaSimpleNewFn,
    simple_read: PaSimpleReadFn,
    simple_free: PaSimpleFreeFn,
}

impl PulseSimpleApi {
    unsafe fn load() -> Result<Self, String> {
        let candidates = ["libpulse-simple.so.0\0", "libpulse-simple.so\0"];
        let mut handle: *mut c_void = std::ptr::null_mut();
        for name in &candidates {
            handle = libc::dlopen(
                name.as_ptr() as *const c_char,
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return Err("libpulse-simple.so.0 not found".to_string());
        }

        let sym_new = libc::dlsym(handle, c"pa_simple_new".as_ptr());
        let sym_read = libc::dlsym(handle, c"pa_simple_read".as_ptr());
        let sym_free = libc::dlsym(handle, c"pa_simple_free".as_ptr());

        if sym_new.is_null() || sym_read.is_null() || sym_free.is_null() {
            libc::dlclose(handle);
            return Err("missing symbols in libpulse-simple.so.0".to_string());
        }

        Ok(Self {
            _lib: handle,
            simple_new: std::mem::transmute::<*mut c_void, PaSimpleNewFn>(sym_new),
            simple_read: std::mem::transmute::<*mut c_void, PaSimpleReadFn>(sym_read),
            simple_free: std::mem::transmute::<*mut c_void, PaSimpleFreeFn>(sym_free),
        })
    }
}

#[allow(clippy::unnecessary_cast)]
fn monotonic_ticks_100ns() -> i64 {
    unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        (ts.tv_sec as i64) * 10_000_000 + (ts.tv_nsec as i64) / 100
    }
}

fn input_mode_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg)
                .join("Wispr Flow")
                .join("notetaker-input-mode.json");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("Wispr Flow")
        .join("notetaker-input-mode.json")
}

fn is_system_audio_muted_by_mode(path: &PathBuf) -> bool {
    if let Ok(contents) = std::fs::read_to_string(path) {
        if contents.contains("\"mic_only\"") {
            return true;
        }
    }
    false
}

pub fn run_capture_system_audio() {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_flag);

    // Watch stdin for EOF (Electron stops capture by closing child.stdin).
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    stop_clone.store(true, Ordering::SeqCst);
                    std::process::exit(0);
                }
                Ok(_) => {}
            }
        }
    });

    let api = match unsafe { PulseSimpleApi::load() } {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "capture_failed:{}", e);
            let _ = std::io::stderr().flush();
            std::process::exit(1);
        }
    };

    let app_name = CString::new("Wispr Flow").unwrap();
    let dev_name = CString::new("@DEFAULT_MONITOR@").unwrap();
    let stream_name = CString::new("Notetaker System Audio").unwrap();

    let spec = PaSampleSpec {
        format: PA_SAMPLE_S16LE,
        rate: SAMPLE_RATE,
        channels: CHANNELS,
    };
    let attr = PaBufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: FRAME_BYTES as u32,
    };

    let mut err: c_int = 0;
    let mut pa_handle = unsafe {
        (api.simple_new)(
            std::ptr::null(),
            app_name.as_ptr(),
            PA_STREAM_RECORD,
            dev_name.as_ptr(),
            stream_name.as_ptr(),
            &spec,
            std::ptr::null(),
            &attr,
            &mut err,
        )
    };

    if pa_handle.is_null() {
        let _ = writeln!(std::io::stderr(), "capture_failed:pa_simple_new_{}", err);
        let _ = std::io::stderr().flush();
        std::process::exit(1);
    }

    // Signal to Wispr Flow main process that the system audio endpoint is open.
    let _ = writeln!(std::io::stderr(), "endpoint_opened");
    let _ = std::io::stderr().flush();

    let mode_path = input_mode_config_path();
    let mut mute_system = is_system_audio_muted_by_mode(&mode_path);
    let mut last_mode_check = Instant::now();

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut seq: u64 = 0;
    let mut packet = [0u8; PACKET_BYTES];
    packet[0..4].copy_from_slice(b"WAF2");
    packet[4..8].copy_from_slice(&(FRAME_BYTES as u32).to_le_bytes());

    while !stop_flag.load(Ordering::Relaxed) {
        if last_mode_check.elapsed() >= Duration::from_millis(250) {
            mute_system = is_system_audio_muted_by_mode(&mode_path);
            last_mode_check = Instant::now();
        }

        let rc = unsafe {
            (api.simple_read)(
                pa_handle,
                packet[HEADER_BYTES..PACKET_BYTES].as_mut_ptr() as *mut c_void,
                FRAME_BYTES,
                &mut err,
            )
        };

        if rc < 0 {
            // Reconnect cleanly if the default audio sink switched (e.g. headphones plugged/unplugged).
            unsafe { (api.simple_free)(pa_handle) };
            std::thread::sleep(Duration::from_millis(40));
            pa_handle = unsafe {
                (api.simple_new)(
                    std::ptr::null(),
                    app_name.as_ptr(),
                    PA_STREAM_RECORD,
                    dev_name.as_ptr(),
                    stream_name.as_ptr(),
                    &spec,
                    std::ptr::null(),
                    &attr,
                    &mut err,
                )
            };
            if pa_handle.is_null() {
                // Fill silence for one 40ms frame while waiting for the new sink.
                packet[HEADER_BYTES..PACKET_BYTES].fill(0);
            } else {
                let _ = writeln!(std::io::stderr(), "endpoint_opened");
                let _ = std::io::stderr().flush();
                continue;
            }
        }

        if mute_system {
            packet[HEADER_BYTES..PACKET_BYTES].fill(0);
        }

        let ticks = monotonic_ticks_100ns().saturating_sub(FRAME_TICKS_100NS);
        packet[8..16].copy_from_slice(&seq.to_le_bytes());
        packet[16..24].copy_from_slice(&ticks.to_le_bytes());

        if out.write_all(&packet).is_err() || out.flush().is_err() {
            break;
        }
        seq = seq.wrapping_add(1);
    }

    if !pa_handle.is_null() {
        unsafe { (api.simple_free)(pa_handle) };
    }
}
