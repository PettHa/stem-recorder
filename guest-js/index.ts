import { invoke } from "@tauri-apps/api/core";

export const COMMAND = {
  LIST_DEVICES: "plugin:stem-recorder|list_devices",
  START_MONITOR: "plugin:stem-recorder|start_monitor",
  STOP_MONITOR: "plugin:stem-recorder|stop_monitor",
  START_RECORD: "plugin:stem-recorder|start_record",
  STOP_RECORD: "plugin:stem-recorder|stop_record",
  GET_STATS: "plugin:stem-recorder|get_stats",
  VERIFY_WAV: "plugin:stem-recorder|verify_wav",
} as const;

export const EVENT_LEVEL = "level";

export interface DeviceInfo {
  name: string;
  is_default: boolean;
}

export interface DeviceList {
  mics: DeviceInfo[];
  default_render: string | null;
}

export interface StartRecordArgs {
  output_dir: string;
  filename_base: string;
}

export interface StopRecordResult {
  mic_path: string;
  sys_path: string | null;
}

export interface LevelPayload {
  channel: "mic" | "sys";
  peak: number;
}

export const listDevices = () => invoke<DeviceList>(COMMAND.LIST_DEVICES);

export const startMonitor = (micDevice?: string | null) =>
  invoke<void>(COMMAND.START_MONITOR, { micDevice: micDevice ?? null });

export const stopMonitor = () => invoke<void>(COMMAND.STOP_MONITOR);

export const startRecord = (args: StartRecordArgs) =>
  invoke<void>(COMMAND.START_RECORD, { args });

export const stopRecord = () => invoke<StopRecordResult>(COMMAND.STOP_RECORD);

export interface Stats {
  recording: boolean;
  mic_buffers_sent: number;
  mic_buffers_dropped: number;
  mic_samples_sent: number;
  sys_buffers_sent: number;
  sys_buffers_dropped: number;
  sys_samples_sent: number;
}

export interface WavInfo {
  path: string;
  sample_rate: number;
  channels: number;
  bits_per_sample: number;
  sample_count: number;
  frame_count: number;
  duration_secs: number;
  max_peak: number;
  mean_abs: number;
  silent: boolean;
}

export const getStats = () => invoke<Stats>(COMMAND.GET_STATS);
export const verifyWav = (path: string) => invoke<WavInfo>(COMMAND.VERIFY_WAV, { path });
