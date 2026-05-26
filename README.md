# stem-recorder

> Tauri 2 plugin that records your **microphone** and your **system audio (Windows WASAPI loopback)**
> as two separate WAV files — so you can mix them properly afterwards in a DAW.

Built because [VoiceMeeter](https://vb-audio.com/Voicemeeter/) solves the same problem but
requires installing virtual audio drivers that take over your whole system, and the UI is
from another century. stem-recorder is a single Tauri window that opens, listens, and writes
two clean 32-bit float WAVs.

## Status

| Platform | Mic capture | System loopback |
| -------- | :---------: | :-------------: |
| Windows  | ✅ (cpal)   | ✅ (wasapi-rs)  |
| macOS    | ✅ (cpal)   | ❌              |
| Linux    | ✅ (cpal)   | ❌              |

Forked from [`ayangweb/tauri-plugin-mic-recorder`](https://github.com/ayangweb/tauri-plugin-mic-recorder).
The mic-capture scaffolding is theirs (cpal + hound + plugin layout); everything else — loopback,
dual capture, live metering, SPSC writer threads, stress instrumentation — is new in this fork.

## Architecture

```
┌─ cpal callback (audio RT thread) ─┐    ┌─ WASAPI loopback thread ─┐
│   compute peak                    │    │   compute peak           │
│   convert to f32 Vec              │    │   convert bytes to f32   │
│   try_send → ───────────┐         │    │   try_send → ─┐          │
│   emit "level" event    │         │    │  emit "level"  │         │
└─────────────────────────┼─────────┘    └────────────────┼─────────┘
                          ▼                               ▼
                  SPSC(64 cap)                    SPSC(64 cap)
                          │                               │
                          ▼                               ▼
            ┌─ mic writer thread ─┐         ┌─ sys writer thread ─┐
            │   recv → write_sample│        │  recv → write_sample │
            │   → BufWriter(8KB)   │        │  → BufWriter(8KB)    │
            │   → <base>.mic.wav   │        │  → <base>.sys.wav    │
            └──────────────────────┘        └──────────────────────┘
```

Two audio capture threads (cpal-managed + our own WASAPI thread) do zero disk I/O. They
push converted f32 buffers into bounded SPSC channels; two dedicated writer threads drain
those channels and write to disk via `hound::WavWriter` → `BufWriter` → file. If a writer
falls behind (full channel), the capture side drops a buffer rather than blocking the
audio thread — a counter exposes this so you can verify drops stay at zero.

Both WAVs are written as **32-bit float** regardless of device sample format (lossless from
any input). RAM footprint is flat ≈ 300 KB per stem regardless of recording duration.

## Tauri integration

```rust
// src-tauri/src/lib.rs
pub fn run() {
    tauri::Builder::default()
        .plugin(stem_recorder::init())
        .plugin(tauri_plugin_dialog::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

```jsonc
// src-tauri/capabilities/default.json
{
    "permissions": ["core:default", "stem-recorder:default", "dialog:allow-open"]
}
```

```ts
import {
    listDevices, startMonitor, stopMonitor,
    startRecord, stopRecord,
    getStats, verifyWav,
} from "stem-recorder-api";

await startMonitor(micDeviceName);   // open persistent capture streams (peak events start)
// …user clicks record…
await startRecord({ output_dir: "C:/out", filename_base: "session-01" });
// …user clicks stop…
const { mic_path, sys_path } = await stopRecord();
const info = await verifyWav(mic_path);   // duration, sample count, peak — flag silent/truncated
```

The frontend listens for `level` events (`{ channel: "mic" | "sys", peak: f32 }`, ~20×/s)
to drive level meters before recording even starts.

## Methods

| Method          | Description                                                              |
| --------------- | ------------------------------------------------------------------------ |
| `listDevices`   | Enumerate input devices + report default render endpoint name.           |
| `startMonitor`  | Open persistent capture streams for selected mic + default loopback.     |
| `stopMonitor`   | Close capture streams.                                                   |
| `startRecord`   | Attach WAV writers (mic + sys) to the in-flight streams.                 |
| `stopRecord`    | Detach writers, finalize WAVs, return paths.                             |
| `getStats`      | Buffer counters: sent / dropped / sample count per channel (since start).|
| `verifyWav`     | Read back a WAV; report duration, sample count, max peak, silent flag.   |

## Events

| Event       | Payload                                          | Frequency   |
| ----------- | ------------------------------------------------ | ----------- |
| `level`     | `{ channel: "mic" \| "sys", peak: f32 }`         | ~20 Hz each |
| `sys-error` | `string` (loopback init failure detail)          | on failure  |

## Build prerequisites (Windows)

Cargo's GNU toolchain needs `dlltool.exe`, which doesn't ship with `rustup`. Install MSYS2
and add `C:\msys64\mingw64\bin` to your PATH, or set it per-shell:

```pwsh
$env:Path = "C:\msys64\mingw64\bin;" + $env:Path
```

## Demo app

```bash
cd examples/tauri-app
npm install
npm run tauri dev
```

UI is built on the [MarmotCrew](https://marmotcrew.com) design system (cream/charcoal,
mono headings, `$ command` aesthetic, per-channel crew hues — Rust for mic, Tarn for system).

## Caveats

- **Header finalize on stop.** WAV header is written at `stopRecord` time. If the app
  crashes mid-recording, audio bytes survive (kernel page cache) but the RIFF header
  stays at its `WavWriter::create` placeholder size — most players will refuse the file.
  Recover with `ffmpeg -i broken.wav -c copy fixed.wav`.
- **No per-process loopback yet.** Captures everything Windows is mixing through the
  default render endpoint. Per-process capture (Win 10 build 19041+) is on the roadmap.
- **System loopback follows Windows' default render device.** Change the default in
  Windows sound settings and the UI re-queries within ~2.5 s (focus + interval polling).

## Stress testing

Counters and WAV verification are surfaced in the demo app for production validation:
- Live `N buf · M samp · 0 drops` under each stem tile while recording — green = clean
- On stop, both stems are read back and reported as `Xs · sr/ch · peak dB` (silent/drift
  flagged in yellow/red)

A clean 5-minute test on a typical SSD records zero drops across all ~14,400 buffers per
stem.

## License

MIT, inherited from upstream.
