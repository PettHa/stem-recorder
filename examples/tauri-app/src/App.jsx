import { useEffect, useRef, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { listen } from '@tauri-apps/api/event';
import {
  EVENT_LEVEL,
  listDevices,
  startMonitor,
  stopMonitor,
  startRecord,
  stopRecord,
  getStats,
  verifyWav,
} from 'stem-recorder-api';
import { Wordmark } from './Wordmark.jsx';
import { LevelBar } from './LevelBar.jsx';
import './styles/app.css';

function basename(p) {
  if (!p) return '';
  const parts = p.replace(/\\/g, '/').split('/');
  return parts[parts.length - 1] || p;
}

function formatElapsed(ms) {
  const s = Math.floor(ms / 1000);
  const mm = String(Math.floor(s / 60)).padStart(2, '0');
  const ss = String(s % 60).padStart(2, '0');
  return `${mm}:${ss}`;
}

export default function App() {
  const [theme, setTheme] = useState('light');
  const [devices, setDevices] = useState({ mics: [], default_render: null });
  const [micDevice, setMicDevice] = useState('');
  const [outputDir, setOutputDir] = useState('');
  const [filenameBase, setFilenameBase] = useState('');
  const [recording, setRecording] = useState(false);
  const [monitoring, setMonitoring] = useState(false);
  const [error, setError] = useState(null);
  const [result, setResult] = useState(null);
  const [startedAt, setStartedAt] = useState(null);
  const [elapsed, setElapsed] = useState(0);
  const [stats, setStats] = useState(null);
  const [verify, setVerify] = useState(null); // { mic: WavInfo, sys: WavInfo|null }

  // Refs for live audio levels (avoids re-rendering on every event ~20/s/channel)
  const micPeakRef = useRef(0);
  const sysPeakRef = useRef(0);

  useEffect(() => {
    document.documentElement.className = theme === 'dark' ? 'theme-dark' : 'theme-light';
  }, [theme]);

  const refreshDevices = (preserveMicSelection = true) => {
    return listDevices()
      .then((d) => {
        setDevices(d);
        setMicDevice((prev) => {
          if (preserveMicSelection && prev && d.mics.some((m) => m.name === prev)) {
            return prev;
          }
          const def = d.mics.find((m) => m.is_default);
          return def?.name ?? d.mics[0]?.name ?? '';
        });
      })
      .catch((e) => setError(String(e)));
  };

  useEffect(() => {
    refreshDevices(false);
  }, []);

  useEffect(() => {
    const onFocus = () => { if (!recording) refreshDevices(); };
    const onVis = () => {
      if (document.visibilityState === 'visible' && !recording) refreshDevices();
    };
    window.addEventListener('focus', onFocus);
    document.addEventListener('visibilitychange', onVis);
    return () => {
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onVis);
    };
  }, [recording]);

  useEffect(() => {
    if (recording) return undefined;
    const id = setInterval(() => {
      if (document.visibilityState === 'visible') refreshDevices();
    }, 2500);
    return () => clearInterval(id);
  }, [recording]);

  // Start monitor whenever a mic is selected (or changes). Rust handles
  // tearing down the old stream automatically.
  useEffect(() => {
    if (!micDevice) return undefined;
    let cancelled = false;
    startMonitor(micDevice)
      .then(() => {
        if (!cancelled) setMonitoring(true);
      })
      .catch((e) => {
        if (!cancelled) {
          setMonitoring(false);
          setError(String(e));
        }
      });
    return () => {
      cancelled = true;
    };
  }, [micDevice]);

  // Subscribe to level + sys-error events from Rust.
  const [sysActive, setSysActive] = useState(true);
  useEffect(() => {
    const unlisteners = [];
    listen(EVENT_LEVEL, (event) => {
      const { channel, peak } = event.payload ?? {};
      if (channel === 'mic') micPeakRef.current = peak;
      else if (channel === 'sys') sysPeakRef.current = peak;
    }).then((fn) => unlisteners.push(fn));
    listen('sys-error', (event) => {
      setSysActive(false);
      setError(`system audio: ${event.payload}`);
    }).then((fn) => unlisteners.push(fn));
    return () => unlisteners.forEach((fn) => fn());
  }, []);

  // Recording elapsed timer
  useEffect(() => {
    if (!recording || !startedAt) return;
    const id = setInterval(() => setElapsed(Date.now() - startedAt), 250);
    return () => clearInterval(id);
  }, [recording, startedAt]);

  // Live stats poller (drops + buffer counts) while recording.
  useEffect(() => {
    if (!recording) return undefined;
    const id = setInterval(() => {
      getStats().then(setStats).catch(() => {});
    }, 250);
    return () => clearInterval(id);
  }, [recording]);

  const pickFolder = async () => {
    const picked = await open({ directory: true, multiple: false });
    if (typeof picked === 'string') setOutputDir(picked);
  };

  const toggleRecord = async () => {
    setError(null);
    if (recording) {
      try {
        const r = await stopRecord();
        setResult(r);
        // Verify both stems immediately — surfaces silent stems, truncation,
        // sample-count drift right next to the recording in the UI.
        const expectedSecs = startedAt ? (Date.now() - startedAt) / 1000 : null;
        const [mic, sys] = await Promise.all([
          r.mic_path ? verifyWav(r.mic_path).catch((e) => ({ error: String(e) })) : null,
          r.sys_path ? verifyWav(r.sys_path).catch((e) => ({ error: String(e) })) : null,
        ]);
        setVerify({ mic, sys, expectedSecs });
      } catch (e) {
        setError(String(e));
      } finally {
        setRecording(false);
        setStartedAt(null);
      }
      return;
    }
    if (!outputDir) {
      setError('Pick an output folder first.');
      return;
    }
    if (!monitoring) {
      setError('Audio monitor not ready yet — wait a moment.');
      return;
    }
    try {
      await startRecord({
        output_dir: outputDir,
        filename_base: filenameBase.trim(),
      });
      setResult(null);
      setVerify(null);
      setStats(null);
      setStartedAt(Date.now());
      setElapsed(0);
      setRecording(true);
    } catch (e) {
      setError(String(e));
    }
  };

  const stemStatLine = (channel) => {
    if (recording && stats) {
      const sent = stats[`${channel}_buffers_sent`] ?? 0;
      const dropped = stats[`${channel}_buffers_dropped`] ?? 0;
      const samples = stats[`${channel}_samples_sent`] ?? 0;
      const cls = dropped > 0 ? 'stat bad' : 'stat ok';
      return (
        <div className={cls}>
          {sent.toLocaleString()} buf · {samples.toLocaleString()} samp ·{' '}
          {dropped > 0 ? `${dropped} DROPPED` : '0 drops'}
        </div>
      );
    }
    if (verify) {
      const info = verify[channel];
      if (!info) return null;
      if (info.error) return <div className="stat bad">verify: {info.error}</div>;
      const expected = verify.expectedSecs;
      const drift = expected != null ? info.duration_secs - expected : null;
      const driftBad = drift != null && Math.abs(drift) > 0.25;
      const pass = !info.silent && !driftBad;
      return (
        <div className={pass ? 'stat ok' : 'stat warn'}>
          {info.duration_secs.toFixed(2)}s · {info.sample_rate / 1000}kHz/{info.channels}ch ·{' '}
          peak {(20 * Math.log10(Math.max(info.max_peak, 1e-6))).toFixed(1)} dB
          {info.silent && ' · SILENT'}
          {driftBad && ` · drift ${drift > 0 ? '+' : ''}${drift.toFixed(2)}s`}
        </div>
      );
    }
    return null;
  };

  const kickerClass = error
    ? 'kicker error'
    : recording
    ? 'kicker recording'
    : monitoring
    ? 'kicker monitoring'
    : 'kicker';
  const kickerText = error
    ? 'error'
    : recording
    ? `recording · ${formatElapsed(elapsed)}`
    : monitoring
    ? 'listening · idle'
    : 'starting…';

  return (
    <div className="shell">
      <header className="topbar">
        <Wordmark />
        <button
          type="button"
          className="theme-toggle"
          onClick={() => setTheme((t) => (t === 'dark' ? 'light' : 'dark'))}
        >
          {theme === 'dark' ? 'light' : 'dark'}
        </button>
      </header>

      <span className={kickerClass}>
        <span className="dot" />
        {kickerText}
      </span>

      <section className="form-grid">
        <label htmlFor="mic">microphone</label>
        <select
          id="mic"
          value={micDevice}
          onChange={(e) => setMicDevice(e.target.value)}
          disabled={recording}
        >
          {devices.mics.length === 0 && <option>— no mics found —</option>}
          {devices.mics.map((m) => (
            <option key={m.name} value={m.name}>
              {m.name}
              {m.is_default ? '  (default)' : ''}
            </option>
          ))}
        </select>

        <label>system audio</label>
        <div className="path-row">
          <code className="code sys-render">{devices.default_render ?? '—'}</code>
          <button
            type="button"
            className="cmd sm"
            onClick={() => refreshDevices()}
            disabled={recording}
            title="re-query devices"
          >
            <span className="d">$</span>refresh
          </button>
        </div>

        <label htmlFor="folder">output folder</label>
        <div className="path-row">
          <input
            id="folder"
            type="text"
            readOnly
            value={outputDir}
            placeholder="(none selected)"
          />
          <button type="button" className="cmd sm" onClick={pickFolder} disabled={recording}>
            <span className="d">$</span>browse
          </button>
        </div>

        <label htmlFor="base">filename</label>
        <input
          id="base"
          type="text"
          value={filenameBase}
          onChange={(e) => setFilenameBase(e.target.value)}
          placeholder="(auto: timestamp)"
          disabled={recording}
        />
      </section>

      <section className="stems">
        <div className="stem mic">
          <div className="k">mic · stem 1</div>
          <div className="v">{micDevice || '—'}</div>
          <LevelBar peakRef={micPeakRef} colorVar="--mc-rust" active={monitoring} />
          <div className="sub">{result?.mic_path ? basename(result.mic_path) : 'writes <name>.mic.wav'}</div>
          {stemStatLine('mic')}
        </div>
        <div className="stem sys">
          <div className="k">system · stem 2</div>
          <div className="v">{devices.default_render || '—'}</div>
          <LevelBar peakRef={sysPeakRef} colorVar="--mc-tarn" active={monitoring && sysActive} />
          <div className="sub">{result?.sys_path ? basename(result.sys_path) : 'writes <name>.sys.wav'}</div>
          {stemStatLine('sys')}
        </div>
      </section>

      {error && <div className="error">{error}</div>}

      <div className="controls">
        <button
          type="button"
          className={recording ? 'cmd recording' : 'cmd'}
          onClick={toggleRecord}
        >
          <span className="d">$</span>
          {recording ? 'stop' : 'record'}
          <span className="cu" />
        </button>
        <div className="meta">
          {result && !recording ? (
            <>
              saved · <b>{result.mic_path ? basename(result.mic_path).replace(/\.mic\.wav$/, '') : '?'}</b>
            </>
          ) : recording ? (
            <>writing two WAV files in parallel</>
          ) : monitoring ? (
            <>monitoring — bars show live levels before you record</>
          ) : (
            <>two separate WAV stems — mix later in your DAW</>
          )}
        </div>
      </div>

      <div className="hint">
        records <b>your mic</b> and <b>whatever Windows is playing</b> as two separate WAV files.{' '}
        loopback uses WASAPI on the default render endpoint — pick speakers or headphones in
        Windows sound settings, not the app.
      </div>
    </div>
  );
}
