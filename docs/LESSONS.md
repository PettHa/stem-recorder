# Lessons learned

Engineering notes accumulated while building stem-recorder. Each entry is something
that cost real time to figure out and is non-obvious from upstream docs. Treat as
load-bearing — removing a workaround usually means hitting the bug again.

---

## 1. WASAPI loopback: Direction::Render device + Direction::Capture init

For Windows system-audio loopback recording via [wasapi-rs](https://docs.rs/wasapi),
you must mix the two `Direction` values:

```rust
// Get the SPEAKER endpoint (= playback device)
let device = enumerator.get_default_device(&Direction::Render)?;
let mut audio_client = device.get_iaudioclient()?;

// But initialize as CAPTURE — this is what trips the loopback flag internally.
audio_client.initialize_client(&format, &Direction::Capture, &mode)?;

let capture_client = audio_client.get_audiocaptureclient()?;
```

Passing `Direction::Render` to `initialize_client` returns
**`AUDCLNT_E_WRONG_ENDPOINT_TYPE` (HRESULT 0x88890003)** because
`get_audiocaptureclient` cannot be called on a render-initialized client.

The wasapi-rs `record.rs` example has a one-line comment about this in passing
but the `loopback.rs` example is actually mic→speakers passthrough, not what
its name suggests. Don't be misled.

See: [`src/loopback.rs`](../src/loopback.rs).

## 2. WASAPI loopback emits NO samples when nothing is playing

The Windows audio engine doesn't generate samples on the render endpoint when
playback is silent. `read_from_device_to_deque` returns zero bytes and
`h_event.wait_for_event` keeps timing out.

**Consequence for live VU meters:** the UI will sit at zero peak forever and
look broken even though monitoring is technically active.

**Fix:** emit a heartbeat `level` event with `peak=0` every ~50 ms regardless
of whether real samples arrived. UI then distinguishes "monitor running, no
audio" from "dead stream."

```rust
let _ = h_event.wait_for_event(150);
if last_emit.elapsed() >= Duration::from_millis(50) {
    let _ = app_handle.emit("level", LevelPayload { channel: "sys", peak });
    peak = 0.0;
    last_emit = Instant::now();
}
```

## 3. WASAPI init must be synchronous to the caller

If you spawn the WASAPI capture thread and let it set `WavSpec` lazily, the
frontend will race with you on `start_record` and hit "Sys spec not ready yet."

**Fix:** thread blocks on COM init, then sends `Ok(spec)` or `Err(msg)` back
via an `mpsc::channel`. Caller does `recv_timeout(3s)` and only returns from
`start_loopback_monitor` once spec is in hand.

```rust
let (init_tx, init_rx) = mpsc::channel::<Result<WavSpec, String>>();
let handle = thread::spawn(move || monitor_thread(/* … */, init_tx));
let spec = init_rx.recv_timeout(Duration::from_secs(3))??;
```

WASAPI types (`AudioClient`, etc) are not `Send`, so all WASAPI setup MUST
happen on the same thread that runs the capture loop. You can't init on the
main thread and then move the client to a worker.

---

## 4. Never do disk I/O from a realtime audio callback

cpal callbacks and the WASAPI capture loop have hard timing budgets (typically
~10 ms). `WavWriter::write_sample` looks instant but its underlying `BufWriter`
flushes every ~8 KB which causes a `write()` syscall. Syscalls can block,
especially under disk pressure or AV scans.

**Symptom:** mic quality subtly degrades during recording vs. monitoring-only.
Hard to A/B because the degradation only shows up under disk load.

**Fix:** lock-free producer → bounded SPSC channel → dedicated writer thread.

```
audio callback  →  mpsc::sync_channel(64)  →  writer thread → WavWriter
   (RT thread)        (try_send, never blocks)    (does ALL disk I/O)
```

The audio callback computes peak, allocates a `Vec<f32>`, and `try_send`s.
If the channel is full (writer fell behind), drop the buffer and increment
a counter — better than blocking the audio thread.

Critical: `try_send` not `send`. `send` would block on full channel which
defeats the entire purpose.

See: [`src/commands.rs`](../src/commands.rs) `capture_cb` + `writer_loop`.

## 5. Allocate-per-callback is acceptable; disk-per-callback is not

I worried about `Vec::with_capacity(input.len())` allocating ~4-8 KB per
callback (47 allocations/sec at 48 kHz with typical buffer sizes). It's
fine — modern allocators handle this in microseconds.

A proper rtrb-style pre-allocated ring buffer would be marginally better
but not worth the complexity for this app. If you ever go for it, also
swap the `mpsc::sync_channel` for `rtrb::RingBuffer` to eliminate the
mutex on `try_send`.

---

## 6. Disk-full silently truncates stems

When the writer thread's `write_sample` errors mid-recording, `writer_loop`
returns Err and the WavWriter is dropped WITHOUT `finalize()`. The on-disk
file then has:

- A valid placeholder WAV header (from `WavWriter::create`)
- Whatever samples got flushed to disk before the error
- An incorrect data-chunk-size field (still the placeholder 0)

Worse: write_sample fails INSTANTLY when disk is full, so 11 minutes of
audio can disappear with the file ending up containing only the last few
seconds (whatever fit during a brief window where space was reclaimed).

**Mitigations now in place:**
- `start_record` refuses to start if `< 5 GB` free on the output volume
  (see `MIN_FREE_BYTES` in `src/commands.rs`)

**Mitigations not yet implemented (potential future work):**
- Periodic header rewrite (re-seek + write correct sizes every 5 s) so
  crash-mid-recording leaves a valid WAV
- Emit `recording-aborted` event on first write failure so UI surfaces it
- Live disk-space meter in UI during recording

## 7. WAV header rewrite recovers truncated files

If you get a WAV that won't open (header says 0 bytes of data but file has
real bytes), rewrite the header in place from actual file size:

```pwsh
$path = "C:\path\to\broken.wav"
$bytes = [System.IO.File]::ReadAllBytes($path)
$fileSize = $bytes.Length

# patch RIFF size (file - 8) at offset 4
[BitConverter]::GetBytes([uint32]($fileSize - 8)).CopyTo($bytes, 4)

# find "data" chunk marker
$dataPos = -1
for ($i = 12; $i -lt $bytes.Length - 4; $i++) {
  if ($bytes[$i] -eq 0x64 -and $bytes[$i+1] -eq 0x61 -and
      $bytes[$i+2] -eq 0x74 -and $bytes[$i+3] -eq 0x61) {
    $dataPos = $i; break
  }
}
$dataSize = $fileSize - ($dataPos + 8)
[BitConverter]::GetBytes([uint32]$dataSize).CopyTo($bytes, $dataPos + 4)

[System.IO.File]::WriteAllBytes($path -replace '\.wav$', '.fixed.wav', $bytes)
```

Audacity can also import as raw PCM (skip 44/60 bytes of header) if you
know sample rate + channel count + bit depth.

`ffmpeg -i broken.wav -c copy fixed.wav` works for some corruption modes
but not all.

---

## 8. Windows GNU toolchain: dlltool.exe + cdylib pitfall

`rustup` ships GNU toolchain by default but doesn't include `dlltool.exe`.
Cargo fails with "error calling dlltool 'dlltool.exe': program not found"
when building anything that needs Windows import libraries (most things).

**Fix:** install MSYS2 and prepend `C:\msys64\mingw64\bin` to PATH for
cargo invocations:

```pwsh
$env:Path = "C:\msys64\mingw64\bin;" + $env:Path
cargo build
```

Or set it per-shell as a profile.

### cdylib "export ordinal too large"

The Tauri example template defaults to `crate-type = ["staticlib", "cdylib", "rlib"]`.
With GNU toolchain on Windows, linking the cdylib variant for a sufficiently
large dependency tree (i.e. anything pulling in tauri + wry) fails with:

```
ld.exe: error: export ordinal too large: 125236
collect2.exe: error: ld returned 1 exit status
```

The GNU PE linker can't handle export tables past ~65535 ordinals. There
is no flag to relax this.

**Fix:** narrow crate-type to just `["rlib"]` in the example's `Cargo.toml`.
The main binary (`main.rs`) builds normally; we don't actually need a cdylib
for desktop Tauri.

```toml
[lib]
name = "stem_recorder_app_lib"
crate-type = ["rlib"]
```

The only thing you'd lose is mobile (iOS/Android) support, which needs cdylib.

### Switching to MSVC

If you install Visual Studio Build Tools, you can use the MSVC toolchain
instead. None of the above issues apply with MSVC. Run:

```pwsh
rustup default stable-x86_64-pc-windows-msvc
```

If you do this, you can also drop the WebView2Loader.dll workaround
(see next section).

---

## 9. Tauri NSIS installer doesn't bundle WebView2Loader.dll on windows-gnu

Tauri 2's NSIS bundler is supposed to ship `WebView2Loader.dll` next to the
exe inside the installer. On the `windows-gnu` target this silently fails
even on `tauri-cli@2.11.2` (long past the `tauri-bundler@2.2.2` changelog
note that supposedly fixed it).

**Symptom:** users get a Windows System Error dialog at app launch:
> The code execution cannot proceed because WebView2Loader.dll was not found.
> Reinstalling the program may fix this problem.

**Diagnosis:** string-search the produced `setup.exe` for "WebView2Loader"
(both ASCII and UTF-16). If absent, the DLL is definitively not bundled
(NSIS uses LZMA but file names appear in plaintext metadata).

```pwsh
$bytes = [System.IO.File]::ReadAllBytes("setup.exe")
$pat = [System.Text.Encoding]::Unicode.GetBytes("WebView2Loader")
# … scan for $pat in $bytes …
```

**Workaround:** commit a copy of `WebView2Loader.dll` into `src-tauri/` and
reference it from `tauri.conf.json` `bundle.resources`. Tauri unconditionally
copies resources into the installer payload.

```jsonc
{
  "bundle": {
    "resources": ["WebView2Loader.dll"]
  }
}
```

The DLL is a small (~157 KB) Microsoft-provided helper from the WebView2 SDK
and is explicitly redistributable per the SDK license.

If/when you switch to MSVC toolchain, Tauri's auto-bundling works correctly
and you can remove both the committed DLL and the `bundle.resources` entry.

## 10. embedBootstrapper installs the runtime, not the loader

`bundle.windows.webviewInstallMode.type = "embedBootstrapper"` makes the
installer ALSO run Microsoft's WebView2 runtime installer. This is useful
for distribution to machines that may not have WebView2 Runtime installed.

It does NOT solve the missing-`WebView2Loader.dll`-in-app-folder problem.
That's a separate concern — the loader DLL is needed by the app to FIND the
runtime, not by the runtime itself.

You generally want both:
- `webviewInstallMode: embedBootstrapper` for runtime install
- `bundle.resources: ["WebView2Loader.dll"]` for the loader (see Lesson 9)

---

## 11. Cleaning up old Tauri dev sessions

`npm run tauri dev` leaves both the vite dev server and the compiled app
running. Restarting fails with either "Port 1420 is already in use" or
"failed to remove file ... stem-recorder-app.exe" (file locked by running
process).

**Cleanup:**

```pwsh
# Kill the running app
Get-Process -Name stem-recorder-app -ErrorAction SilentlyContinue |
  Stop-Process -Force

# Kill the vite dev server on port 1420
$conn = Get-NetTCPConnection -LocalPort 1420 -ErrorAction SilentlyContinue |
  Select-Object -First 1
if ($conn) { Stop-Process -Id $conn.OwningProcess -Force }
```

(Do NOT `Stop-Process -Name cargo` — that nukes any other concurrent cargo
builds on the machine. Auto-mode classifier will reject this anyway.)

---

## 12. Stress-test instrumentation principles

The drop counters + `verifyWav` command landed because audio bugs are subtle:
"works on my machine" recordings sound fine until you A/B them against the
input. The stats catch four classes of failure that look identical on
casual listening:

1. **Buffer drops** (`dropped > 0`): writer thread fell behind. Means
   user-perceptible glitches/pops in the WAV.
2. **Silent stem** (`max_peak < 0.0005`): stream was open but never received
   audio. Often indicates a WASAPI init failure that didn't propagate to
   the frontend.
3. **Duration drift** (`|verify.duration - elapsed| > 250 ms`): samples
   went missing somewhere in the pipeline. Could be drops counted elsewhere
   or actual lost frames.
4. **Truncated file**: WAV duration is much shorter than recording elapsed
   time. Likely disk-full mid-recording — see Lesson 6.

Surfacing these in the UI right after `stopRecord` means the user notices
problems while the context is fresh, not later when they try to mix.

Pattern: counters in `Arc<AtomicU64>` (cloneable across threads), reset at
`start_record`, polled at 4 Hz from frontend during recording, displayed
under each stem tile.
