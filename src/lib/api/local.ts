import { invoke } from "@tauri-apps/api/core";

/** 与后端 LocalServiceStatus 对齐。 */
export type LocalRuntimeState =
  | "stopped"
  | "validating"
  | "startingMlx"
  | "warmingUp"
  | "startingGateway"
  | "ready"
  | "degraded"
  | "stopping"
  | "failed";

export interface LocalServiceStatus {
  state: LocalRuntimeState;
  phaseDetail: string;
  modelPath?: string;
  modelName?: string;
  baseUrl: string;
  mlxPort: number;
  gatewayPort: number;
  pidMlx?: number;
  pidGateway?: number;
  lastError?: string;
  startedAt?: number;
  /** 当前活跃请求数（服务就绪时由网关 /studio/metrics 轮询） */
  activeRequests?: number;
}

export interface LocalLogEntry {
  id: number;
  source: string;
  level: string;
  line: string;
  ts: number;
}

export interface LocalModelRecord {
  path: string;
  name: string;
  source: "lm_studio" | "hugging_face" | "custom";
  sizeBytes: number;
  modelType: string;
  quantization: string;
  contextLength?: number;
  kvBytesPerToken?: number;
  hasTokenizer: boolean;
  hasChatTemplate: boolean;
  memoryFit: "comfortable" | "tight" | "too_large";
  verifyLevel: number;
  fingerprint?: string;
  lastUsedAt?: number;
}

// 字段名与 Rust `LocalSettings` 的 serde 输出一致（snake_case）：
// 该结构体没有 `rename_all = "camelCase"`，settings 文件本身也是 snake_case。
export interface LocalSettings {
  schema_version: number;
  python_path: string;
  proxy_path: string;
  selected_model_path?: string;
  custom_model_roots: string[];
  host: string;
  mlx_port: number;
  gateway_port: number;
  max_tokens_fallback: number;
  prefill_step_size: number;
  automatic_prompt_cache: boolean;
  prompt_cache_size: number;
  prompt_concurrency: number;
  decode_concurrency: number;
  upstream_timeout_secs: number;
  /** 推理后端：mlx | ollama */
  backend?: string;
  ollama_base_url?: string;
  ollama_model?: string;
  model_overrides: Record<string, { context_budget?: number; max_tokens?: number }>;
  auto_start_with_app: boolean;
  hidden_cards: Record<string, boolean>;
}

export const localApi = {
  listModels: () =>
    invoke<LocalModelRecord[]>("local_models_list"),
  rescan: () =>
    invoke<LocalModelRecord[]>("local_models_rescan"),
  addRoot: (path: string) =>
    invoke<number>("local_models_add_root", { path }),
  removeRoot: (path: string) =>
    invoke<void>("local_models_remove_root", { path }),
  removeRef: (path: string) =>
    invoke<void>("local_models_remove_ref", { path }),
  start: (modelPath: string) =>
    invoke<LocalServiceStatus>("local_service_start", { modelPath }),
  stop: () =>
    invoke<LocalServiceStatus>("local_service_stop"),
  status: () =>
    invoke<LocalServiceStatus>("local_service_status"),
  logs: (sinceId: number) =>
    invoke<LocalLogEntry[]>("local_service_logs", { sinceId }),
  clearLogs: () =>
    invoke<void>("local_service_clear_logs"),
  test: () =>
    invoke<string>("local_service_test"),
  getSettings: () =>
    invoke<LocalSettings>("local_settings_get"),
  setSettings: (settings: LocalSettings) =>
    invoke<void>("local_settings_set", { settings }),
  getSettingsPath: () =>
    invoke<string>("get_local_settings_path"),
  diagnostics: () =>
    invoke<LocalDiagnostics>("local_service_diagnostics"),
};

export interface ActiveRequestInfo {
  id: number;
  protocol: string;
  streaming: boolean;
  elapsedSeconds?: number;
  requestBytes?: number;
  responseBytes?: number;
}

export interface GatewayMetrics {
  uptimeSeconds: number;
  activeRequests: number;
  totalRequests: number;
  completedRequests: number;
  failedRequests: number;
  requestBytesReceived: number;
  responseBytesSent: number;
  active: ActiveRequestInfo[];
}

export interface LocalDiagnostics {
  status: LocalServiceStatus;
  metrics?: GatewayMetrics;
  recentLogs: LocalLogEntry[];
  gatewayHealthy: boolean;
  mlxHealthy: boolean;
}

/** 状态的本地化映射（i18n key 由页面处理；这里返回语义分类）。 */
export function isLocalRuntimeRunning(state: LocalRuntimeState): boolean {
  return !["stopped", "stopping", "failed"].includes(state);
}

export function isLocalRuntimeStarting(state: LocalRuntimeState): boolean {
  return ["validating", "startingMlx", "warmingUp", "startingGateway"].includes(state);
}

/** 是否为 Local MLX 供应商卡片。 */
export function isLocalProvider(provider: {
  id: string;
  meta?: { providerType?: string };
}): boolean {
  return (
    provider.id === "local-mlx" ||
    provider.meta?.providerType === "local_mlx"
  );
}

/** 本地服务的状态事件名（与后端一致）。 */
export const LOCAL_STATUS_EVENT = "local-model://status";
export const LOCAL_AUTO_SWITCH_EVENT = "local-model://auto-switched";
