// Windows-only WASAPI loopback monitor: keeps a render-endpoint loopback stream
// open continuously, emits peak-level events to the frontend ~20× per second,
// and (when recording) forwards converted f32 samples to a dedicated writer
// thread via a bounded SPSC channel. All disk I/O lives off the audio path.

#![cfg(target_os = "windows")]

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use hound::{SampleFormat, WavSpec};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Runtime};
use wasapi::{
    initialize_mta, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat,
};

pub type SampleTxSlot = Arc<Mutex<Option<mpsc::SyncSender<Vec<f32>>>>>;

#[derive(Serialize, Clone)]
struct LevelPayload {
    channel: &'static str,
    peak: f32,
}

pub struct SysCounters {
    pub buffers_sent: Arc<AtomicU64>,
    pub buffers_dropped: Arc<AtomicU64>,
    pub samples_sent: Arc<AtomicU64>,
}

pub struct LoopbackMonitor {
    pub handle: JoinHandle<()>,
    pub stop_flag: Arc<AtomicBool>,
    pub sample_tx_slot: SampleTxSlot,
    pub spec: WavSpec,
}

/// Spawns the WASAPI loopback monitor thread and BLOCKS until it has either
/// reported its WaveSpec back (success) or returned an init error.
/// Output is always normalized to 32-bit float regardless of the engine's
/// native sample type, so the writer thread can stay format-agnostic.
pub fn start_loopback_monitor<R: Runtime>(
    app_handle: AppHandle<R>,
    counters: SysCounters,
) -> Result<LoopbackMonitor, String> {
    let sample_tx_slot: SampleTxSlot = Arc::new(Mutex::new(None));
    let stop_flag = Arc::new(AtomicBool::new(true));

    let tx_clone = sample_tx_slot.clone();
    let stop_clone = stop_flag.clone();
    let (init_tx, init_rx) = mpsc::channel::<Result<WavSpec, String>>();

    let handle = thread::spawn(move || {
        if let Err(e) = monitor_thread(app_handle, stop_clone, tx_clone, init_tx, counters) {
            eprintln!("loopback monitor thread failed: {e}");
        }
    });

    let spec = init_rx
        .recv_timeout(Duration::from_secs(3))
        .map_err(|e| format!("loopback init timeout: {e}"))??;

    Ok(LoopbackMonitor {
        handle,
        stop_flag,
        sample_tx_slot,
        spec,
    })
}

fn monitor_thread<R: Runtime>(
    app_handle: AppHandle<R>,
    stop_flag: Arc<AtomicBool>,
    sample_tx_slot: SampleTxSlot,
    init_tx: mpsc::Sender<Result<WavSpec, String>>,
    counters: SysCounters,
) -> Result<(), String> {
    let init_failed = |msg: String, tx: &mpsc::Sender<Result<WavSpec, String>>| -> String {
        let _ = tx.send(Err(msg.clone()));
        msg
    };

    if let Err(e) = initialize_mta().ok() {
        return Err(init_failed(format!("initialize_mta: {e:?}"), &init_tx));
    }
    let enumerator = match DeviceEnumerator::new() {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("DeviceEnumerator: {e:?}"), &init_tx)),
    };
    let device = match enumerator.get_default_device(&Direction::Render) {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("get_default_device: {e:?}"), &init_tx)),
    };
    let mut audio_client = match device.get_iaudioclient() {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("get_iaudioclient: {e:?}"), &init_tx)),
    };
    let format = match audio_client.get_mixformat() {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("get_mixformat: {e:?}"), &init_tx)),
    };
    let (def_time, _min_time) = match audio_client.get_device_period() {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("get_device_period: {e:?}"), &init_tx)),
    };

    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: def_time,
    };
    // Render-endpoint device + Capture-direction init = WASAPI loopback.
    if let Err(e) = audio_client.initialize_client(&format, &Direction::Capture, &mode) {
        return Err(init_failed(format!("initialize_client: {e:?}"), &init_tx));
    }
    let h_event = match audio_client.set_get_eventhandle() {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("set_get_eventhandle: {e:?}"), &init_tx)),
    };
    let capture_client = match audio_client.get_audiocaptureclient() {
        Ok(v) => v,
        Err(e) => return Err(init_failed(format!("get_audiocaptureclient: {e:?}"), &init_tx)),
    };

    // We always advertise the WAV as 32-bit float (lossless from any source).
    let device_channels = format.get_nchannels() as u16;
    let device_rate = format.get_samplespersec();
    let out_spec = WavSpec {
        channels: device_channels,
        sample_rate: device_rate,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let bytes_per_sample = (format.get_bitspersample() / 8) as usize;
    let is_float = matches!(format.get_subformat(), Ok(SampleType::Float));

    if let Err(e) = audio_client.start_stream() {
        return Err(init_failed(format!("start_stream: {e:?}"), &init_tx));
    }
    let _ = init_tx.send(Ok(out_spec));

    let mut sample_queue: VecDeque<u8> = VecDeque::new();
    let mut peak: f32 = 0.0;
    let mut last_emit = Instant::now();
    let emit_period = Duration::from_millis(50);

    // Reuse a scratch Vec across iterations to avoid per-loop allocation in
    // the steady state (only grows on the first call past current capacity).
    let mut scratch: Vec<f32> = Vec::with_capacity(8192);

    while stop_flag.load(Ordering::SeqCst) {
        capture_client
            .read_from_device_to_deque(&mut sample_queue)
            .map_err(|e| format!("read_from_device_to_deque: {e:?}"))?;

        scratch.clear();
        while sample_queue.len() >= bytes_per_sample {
            let mut buf = [0u8; 4];
            for b in buf.iter_mut().take(bytes_per_sample) {
                *b = sample_queue.pop_front().unwrap();
            }
            let value_f32: f32 = if is_float {
                f32::from_le_bytes(buf)
            } else if bytes_per_sample == 2 {
                i16::from_le_bytes([buf[0], buf[1]]) as f32 / i16::MAX as f32
            } else if bytes_per_sample == 4 {
                i32::from_le_bytes(buf) as f32 / i32::MAX as f32
            } else {
                0.0
            };
            let abs = value_f32.abs();
            if abs > peak {
                peak = abs;
            }
            scratch.push(value_f32);
        }

        // Forward to writer thread if recording is active. try_send never
        // blocks the WASAPI loop; if the writer is behind we drop the buffer
        // (preferable to glitching the capture thread).
        if !scratch.is_empty() {
            if let Ok(guard) = sample_tx_slot.try_lock() {
                if let Some(tx) = guard.as_ref() {
                    let len = scratch.len() as u64;
                    match tx.try_send(std::mem::take(&mut scratch)) {
                        Ok(()) => {
                            counters.buffers_sent.fetch_add(1, Ordering::Relaxed);
                            counters.samples_sent.fetch_add(len, Ordering::Relaxed);
                        }
                        Err(_) => {
                            counters.buffers_dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }

        if last_emit.elapsed() >= emit_period {
            let _ = app_handle.emit(
                "level",
                LevelPayload {
                    channel: "sys",
                    peak,
                },
            );
            peak = 0.0;
            last_emit = Instant::now();
        }

        let _ = h_event.wait_for_event(150);
    }

    audio_client
        .stop_stream()
        .map_err(|e| format!("stop_stream: {e:?}"))?;
    Ok(())
}
