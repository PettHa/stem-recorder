use chrono::Local;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, Stream,
};
use hound::{SampleFormat, WavSpec, WavWriter};
use serde::{Deserialize, Serialize};
use std::{
    fs::{create_dir_all, File},
    io::BufWriter,
    marker::{Send, Sync},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, LazyLock, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tauri::{command, AppHandle, Emitter, Runtime};

#[cfg(target_os = "windows")]
use crate::loopback::{start_loopback_monitor, LoopbackMonitor, SysCounters};

// All recordings normalize to 32-bit float WAV — lossless from any input
// device format, and lets writer threads stay format-agnostic.
const REC_BITS: u16 = 32;
const REC_FMT: SampleFormat = SampleFormat::Float;
const CHANNEL_CAP: usize = 64; // ~1-2 s of audio buffers in flight

/// Refuse to start recording when the output disk has less than this many bytes free.
/// 5 GB ≈ ~3.5 hours of stereo 48 kHz f32 + headroom for whatever else the user is doing.
const MIN_FREE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

type SampleTxSlot = Arc<Mutex<Option<mpsc::SyncSender<Vec<f32>>>>>;

#[derive(Default)]
pub struct Counters {
    pub mic_buffers_sent: Arc<AtomicU64>,
    pub mic_buffers_dropped: Arc<AtomicU64>,
    pub mic_samples_sent: Arc<AtomicU64>,
    pub sys_buffers_sent: Arc<AtomicU64>,
    pub sys_buffers_dropped: Arc<AtomicU64>,
    pub sys_samples_sent: Arc<AtomicU64>,
}

struct SafeStream(Stream);
unsafe impl Send for SafeStream {}
unsafe impl Sync for SafeStream {}

struct MicMonitor {
    _stream: SafeStream,
    spec: WavSpec, // output spec (32-bit float)
    peak_state: Arc<Mutex<MicPeakState>>,
    sample_tx_slot: SampleTxSlot,
}

struct RecordingSession {
    mic_path: PathBuf,
    sys_path: PathBuf,
    mic_writer_thread: Option<JoinHandle<Result<(), String>>>,
    #[cfg(target_os = "windows")]
    sys_writer_thread: Option<JoinHandle<Result<(), String>>>,
}

#[derive(Default)]
struct State {
    mic: Mutex<Option<MicMonitor>>,
    #[cfg(target_os = "windows")]
    sys: Mutex<Option<LoopbackMonitor>>,
    recording: Mutex<Option<RecordingSession>>,
    is_recording: AtomicBool,
    counters: Counters,
}

static STATE: LazyLock<Arc<State>> = LazyLock::new(|| Arc::new(State::default()));

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct DeviceList {
    pub mics: Vec<DeviceInfo>,
    pub default_render: Option<String>,
}

#[derive(Serialize, Clone)]
struct LevelPayload {
    channel: &'static str,
    peak: f32,
}

#[derive(Deserialize, Debug)]
pub struct StartRecordArgs {
    pub output_dir: String,
    pub filename_base: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct StopRecordResult {
    pub mic_path: String,
    pub sys_path: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Stats {
    pub recording: bool,
    pub mic_buffers_sent: u64,
    pub mic_buffers_dropped: u64,
    pub mic_samples_sent: u64,
    pub sys_buffers_sent: u64,
    pub sys_buffers_dropped: u64,
    pub sys_samples_sent: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct WavInfo {
    pub path: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits_per_sample: u16,
    pub sample_count: u64, // total samples across all channels (cumulative)
    pub frame_count: u64,  // sample_count / channels
    pub duration_secs: f64,
    pub max_peak: f32,
    pub mean_abs: f32,
    pub silent: bool, // true if max_peak < 0.0005 (≈ -66 dBFS)
}

#[command]
pub fn list_devices() -> Result<DeviceList, String> {
    let host = cpal::default_host();
    let default_mic_name = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();

    let mics: Vec<DeviceInfo> = host
        .input_devices()
        .map_err(|e| e.to_string())?
        .filter_map(|d| {
            let name = d.name().ok()?;
            Some(DeviceInfo {
                is_default: name == default_mic_name,
                name,
            })
        })
        .collect();

    let default_render = host.default_output_device().and_then(|d| d.name().ok());

    Ok(DeviceList {
        mics,
        default_render,
    })
}

#[command]
pub fn start_monitor<R: Runtime>(
    app_handle: AppHandle<R>,
    mic_device: Option<String>,
) -> Result<(), String> {
    if STATE.is_recording.load(Ordering::SeqCst) {
        return Err("Cannot reconfigure monitor while recording.".into());
    }

    // ── Mic ─────────────────────────────────────────────────────────────────
    let host = cpal::default_host();
    let device = match mic_device.as_deref() {
        Some(name) if name != "default" => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| format!("No input device named {name}"))?,
        _ => host
            .default_input_device()
            .ok_or("No default input device")?,
    };
    let config = device.default_input_config().map_err(|e| e.to_string())?;
    let out_spec = WavSpec {
        channels: config.channels() as u16,
        sample_rate: config.sample_rate().0,
        bits_per_sample: REC_BITS,
        sample_format: REC_FMT,
    };

    let sample_tx_slot: SampleTxSlot = Arc::new(Mutex::new(None));
    let peak_state = Arc::new(Mutex::new(MicPeakState::default()));

    let tx_for_cb = sample_tx_slot.clone();
    let peak_for_cb = peak_state.clone();
    let app_for_cb = app_handle.clone();
    let counters_for_cb = STATE.clone();

    let err_fn = |err: cpal::StreamError| eprintln!("mic stream error: {err}");

    let stream = match config.sample_format() {
        cpal::SampleFormat::I8 => device.build_input_stream(
            &config.into(),
            move |data: &[i8], _: &_| {
                capture_cb(data, &tx_for_cb, &peak_for_cb, &app_for_cb, &counters_for_cb)
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &config.into(),
            move |data: &[i16], _: &_| {
                capture_cb(data, &tx_for_cb, &peak_for_cb, &app_for_cb, &counters_for_cb)
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I32 => device.build_input_stream(
            &config.into(),
            move |data: &[i32], _: &_| {
                capture_cb(data, &tx_for_cb, &peak_for_cb, &app_for_cb, &counters_for_cb)
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::F32 => device.build_input_stream(
            &config.into(),
            move |data: &[f32], _: &_| {
                capture_cb(data, &tx_for_cb, &peak_for_cb, &app_for_cb, &counters_for_cb)
            },
            err_fn,
            None,
        ),
        _ => return Err("Unsupported mic sample format".into()),
    }
    .map_err(|e| format!("build_input_stream: {e}"))?;

    stream.play().map_err(|e| format!("stream.play: {e}"))?;

    *STATE.mic.lock().unwrap() = Some(MicMonitor {
        _stream: SafeStream(stream),
        spec: out_spec,
        peak_state,
        sample_tx_slot,
    });

    // ── System loopback (Windows-only, soft-fail) ──────────────────────────
    #[cfg(target_os = "windows")]
    {
        let mut sys_guard = STATE.sys.lock().unwrap();
        if sys_guard.is_none() {
            let sys_counters = SysCounters {
                buffers_sent: STATE.counters.sys_buffers_sent.clone(),
                buffers_dropped: STATE.counters.sys_buffers_dropped.clone(),
                samples_sent: STATE.counters.sys_samples_sent.clone(),
            };
            match start_loopback_monitor(app_handle.clone(), sys_counters) {
                Ok(mon) => *sys_guard = Some(mon),
                Err(e) => {
                    let _ = app_handle.emit("sys-error", format!("loopback init failed: {e}"));
                }
            }
        }
    }

    Ok(())
}

#[command]
pub fn stop_monitor() -> Result<(), String> {
    if STATE.is_recording.load(Ordering::SeqCst) {
        return Err("Cannot stop monitor while recording.".into());
    }
    *STATE.mic.lock().unwrap() = None;

    #[cfg(target_os = "windows")]
    {
        if let Some(mon) = STATE.sys.lock().unwrap().take() {
            mon.stop_flag.store(false, Ordering::SeqCst);
            drop(mon.handle);
        }
    }
    Ok(())
}

#[command]
pub fn start_record(args: StartRecordArgs) -> Result<(), String> {
    if STATE.is_recording.load(Ordering::SeqCst) {
        return Err("Already recording.".into());
    }

    // Reset per-session counters so frontend stats are clean from t=0.
    STATE.counters.mic_buffers_sent.store(0, Ordering::Relaxed);
    STATE.counters.mic_buffers_dropped.store(0, Ordering::Relaxed);
    STATE.counters.mic_samples_sent.store(0, Ordering::Relaxed);
    STATE.counters.sys_buffers_sent.store(0, Ordering::Relaxed);
    STATE.counters.sys_buffers_dropped.store(0, Ordering::Relaxed);
    STATE.counters.sys_samples_sent.store(0, Ordering::Relaxed);

    let dir = PathBuf::from(&args.output_dir);
    create_dir_all(&dir).map_err(|e| format!("create_dir_all: {e}"))?;

    // Guardrail: refuse to start if the output volume is nearly full. Hitting
    // mid-recording disk-full silently truncates stems (writes start failing
    // but the audio callback can't slow down — samples just vanish).
    let free = fs4::available_space(&dir)
        .map_err(|e| format!("disk space check failed for {}: {e}", dir.display()))?;
    if free < MIN_FREE_BYTES {
        return Err(format!(
            "Refusing to record — only {:.2} GB free on {}, need ≥ {:.0} GB.",
            free as f64 / 1_073_741_824.0,
            dir.display(),
            MIN_FREE_BYTES as f64 / 1_073_741_824.0,
        ));
    }

    let base = if args.filename_base.trim().is_empty() {
        Local::now().format("%Y%m%d-%H%M%S").to_string()
    } else {
        args.filename_base.clone()
    };
    let mic_path = dir.join(format!("{base}.mic.wav"));
    let sys_path = dir.join(format!("{base}.sys.wav"));

    // ── Mic writer thread ───────────────────────────────────────────────────
    let mic_writer_thread = {
        let mic_guard = STATE.mic.lock().unwrap();
        let mic = mic_guard
            .as_ref()
            .ok_or("Monitor not started — call start_monitor first.")?;
        let writer = WavWriter::create(&mic_path, mic.spec)
            .map_err(|e| format!("mic WAV: {e}"))?;
        let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(CHANNEL_CAP);
        *mic.sample_tx_slot.lock().unwrap() = Some(tx);
        thread::spawn(move || writer_loop(rx, writer))
    };

    // ── Sys writer thread (Windows-only) ────────────────────────────────────
    #[cfg(target_os = "windows")]
    let sys_writer_thread = {
        let sys_guard = STATE.sys.lock().unwrap();
        if let Some(mon) = sys_guard.as_ref() {
            let writer = WavWriter::create(&sys_path, mon.spec)
                .map_err(|e| format!("sys WAV: {e}"))?;
            let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(CHANNEL_CAP);
            *mon.sample_tx_slot.lock().unwrap() = Some(tx);
            Some(thread::spawn(move || writer_loop(rx, writer)))
        } else {
            None
        }
    };

    *STATE.recording.lock().unwrap() = Some(RecordingSession {
        mic_path,
        sys_path,
        mic_writer_thread: Some(mic_writer_thread),
        #[cfg(target_os = "windows")]
        sys_writer_thread,
    });
    STATE.is_recording.store(true, Ordering::SeqCst);
    Ok(())
}

#[command]
pub fn stop_record() -> Result<StopRecordResult, String> {
    if !STATE.is_recording.load(Ordering::SeqCst) {
        return Err("Not recording.".into());
    }
    STATE.is_recording.store(false, Ordering::SeqCst);

    let mut session = STATE
        .recording
        .lock()
        .unwrap()
        .take()
        .ok_or("No recording session in state")?;

    // Drop the senders so the writer threads see EOF and finalize their WAVs.
    if let Some(mic) = STATE.mic.lock().unwrap().as_ref() {
        let _ = mic.sample_tx_slot.lock().unwrap().take();
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(mon) = STATE.sys.lock().unwrap().as_ref() {
            let _ = mon.sample_tx_slot.lock().unwrap().take();
        }
    }

    if let Some(h) = session.mic_writer_thread.take() {
        h.join()
            .map_err(|_| "mic writer thread panicked".to_string())??;
    }
    let mut sys_path_out: Option<String> = None;
    #[cfg(target_os = "windows")]
    {
        if let Some(h) = session.sys_writer_thread.take() {
            h.join()
                .map_err(|_| "sys writer thread panicked".to_string())??;
            sys_path_out = Some(session.sys_path.to_string_lossy().to_string());
        }
    }

    Ok(StopRecordResult {
        mic_path: session.mic_path.to_string_lossy().to_string(),
        sys_path: sys_path_out,
    })
}

#[command]
pub fn get_stats() -> Stats {
    Stats {
        recording: STATE.is_recording.load(Ordering::SeqCst),
        mic_buffers_sent: STATE.counters.mic_buffers_sent.load(Ordering::Relaxed),
        mic_buffers_dropped: STATE.counters.mic_buffers_dropped.load(Ordering::Relaxed),
        mic_samples_sent: STATE.counters.mic_samples_sent.load(Ordering::Relaxed),
        sys_buffers_sent: STATE.counters.sys_buffers_sent.load(Ordering::Relaxed),
        sys_buffers_dropped: STATE.counters.sys_buffers_dropped.load(Ordering::Relaxed),
        sys_samples_sent: STATE.counters.sys_samples_sent.load(Ordering::Relaxed),
    }
}

#[command]
pub fn verify_wav(path: String) -> Result<WavInfo, String> {
    let reader = hound::WavReader::open(&path).map_err(|e| format!("open: {e}"))?;
    let spec = reader.spec();
    let mut max_peak: f32 = 0.0;
    let mut sum_abs: f64 = 0.0;
    let mut sample_count: u64 = 0;

    match spec.sample_format {
        SampleFormat::Float => {
            for s in reader.into_samples::<f32>() {
                let v = s.map_err(|e| format!("read sample: {e}"))?;
                let a = v.abs();
                if a > max_peak {
                    max_peak = a;
                }
                sum_abs += a as f64;
                sample_count += 1;
            }
        }
        SampleFormat::Int => {
            let scale = match spec.bits_per_sample {
                16 => i16::MAX as f32,
                24 | 32 => i32::MAX as f32,
                8 => i8::MAX as f32,
                _ => return Err(format!("unsupported bits_per_sample {}", spec.bits_per_sample)),
            };
            for s in reader.into_samples::<i32>() {
                let v = s.map_err(|e| format!("read sample: {e}"))? as f32 / scale;
                let a = v.abs();
                if a > max_peak {
                    max_peak = a;
                }
                sum_abs += a as f64;
                sample_count += 1;
            }
        }
    }

    let channels = spec.channels as u64;
    let frame_count = if channels == 0 { 0 } else { sample_count / channels };
    let duration_secs = if spec.sample_rate == 0 {
        0.0
    } else {
        frame_count as f64 / spec.sample_rate as f64
    };
    let mean_abs = if sample_count == 0 {
        0.0
    } else {
        (sum_abs / sample_count as f64) as f32
    };

    Ok(WavInfo {
        path,
        sample_rate: spec.sample_rate,
        channels: spec.channels,
        bits_per_sample: spec.bits_per_sample,
        sample_count,
        frame_count,
        duration_secs,
        max_peak,
        mean_abs,
        silent: max_peak < 0.0005,
    })
}

// ─── helpers ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct MicPeakState {
    peak: f32,
    last_emit: Option<Instant>,
}

const EMIT_PERIOD: Duration = Duration::from_millis(50);

fn capture_cb<T>(
    input: &[T],
    sample_tx_slot: &SampleTxSlot,
    peak_state: &Arc<Mutex<MicPeakState>>,
    app: &AppHandle<impl Runtime>,
    state: &Arc<State>,
) where
    T: Sample + Copy,
    f32: FromSample<T>,
{
    // Single pass: convert each sample to f32, track peak, accumulate into out.
    // Allocating per callback (~4-8 KB) is much cheaper than blocking on disk.
    let mut out: Vec<f32> = Vec::with_capacity(input.len());
    let mut local_peak: f32 = 0.0;
    for &s in input {
        let f: f32 = f32::from_sample(s);
        let a = f.abs();
        if a > local_peak {
            local_peak = a;
        }
        out.push(f);
    }

    // Forward to writer thread if recording is active (try_send: never blocks).
    if !out.is_empty() {
        if let Ok(guard) = sample_tx_slot.try_lock() {
            if let Some(tx) = guard.as_ref() {
                let len = out.len() as u64;
                match tx.try_send(out) {
                    Ok(()) => {
                        state.counters.mic_buffers_sent.fetch_add(1, Ordering::Relaxed);
                        state.counters.mic_samples_sent.fetch_add(len, Ordering::Relaxed);
                    }
                    Err(_) => {
                        state.counters.mic_buffers_dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    // Update + maybe emit peak.
    let mut ps = peak_state.lock().unwrap();
    if local_peak > ps.peak {
        ps.peak = local_peak;
    }
    let should_emit = ps
        .last_emit
        .map(|t| t.elapsed() >= EMIT_PERIOD)
        .unwrap_or(true);
    if should_emit {
        let p = ps.peak;
        ps.peak = 0.0;
        ps.last_emit = Some(Instant::now());
        drop(ps);
        let _ = app.emit(
            "level",
            LevelPayload {
                channel: "mic",
                peak: p,
            },
        );
    }
}

fn writer_loop(
    rx: mpsc::Receiver<Vec<f32>>,
    mut writer: WavWriter<BufWriter<File>>,
) -> Result<(), String> {
    while let Ok(buf) = rx.recv() {
        for s in buf {
            writer
                .write_sample(s)
                .map_err(|e| format!("write_sample: {e}"))?;
        }
    }
    writer
        .finalize()
        .map_err(|e| format!("WavWriter::finalize: {e}"))?;
    Ok(())
}
