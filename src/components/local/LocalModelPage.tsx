import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { Loader2, Play, Square, RefreshCw, FolderPlus, Trash2, Cpu } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import type { LocalDiagnostics, LocalSettings } from "@/lib/api/local";
import { cn } from "@/lib/utils";
import {
  LOCAL_STATUS_EVENT,
  localApi,
  type LocalLogEntry,
  type LocalModelRecord,
  type LocalServiceStatus,
} from "@/lib/api/local";
import { listen } from "@tauri-apps/api/event";

const EMPTY_STATUS: LocalServiceStatus = {
  state: "stopped",
  phaseDetail: "",
  baseUrl: "http://127.0.0.1:8091",
  mlxPort: 8090,
  gatewayPort: 8091,
};

const STATE_COLOR: Record<string, string> = {
  stopped: "bg-muted text-muted-foreground",
  validating: "bg-amber-500/10 text-amber-600 dark:text-amber-400",
  startingMlx: "bg-amber-500/10 text-amber-600 dark:text-amber-400",
  warmingUp: "bg-amber-500/10 text-amber-600 dark:text-amber-400",
  startingGateway: "bg-amber-500/10 text-amber-600 dark:text-amber-400",
  ready: "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400",
  degraded: "bg-orange-500/10 text-orange-600 dark:text-orange-400",
  stopping: "bg-muted text-muted-foreground",
  failed: "bg-red-500/10 text-red-600 dark:text-red-400",
};

function formatBytes(bytes: number): string {
  if (bytes >= 1_073_741_824) {
    return `${(bytes / 1_073_741_824).toFixed(1)} GB`;
  }
  if (bytes >= 1_048_576) {
    return `${(bytes / 1_048_576).toFixed(0)} MB`;
  }
  return `${(bytes / 1024).toFixed(0)} KB`;
}

function formatTs(ts: number): string {
  return new Date(ts).toLocaleTimeString();
}

export function LocalModelPage() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<LocalServiceStatus>(EMPTY_STATUS);
  const [models, setModels] = useState<LocalModelRecord[]>([]);
  const [logs, setLogs] = useState<LocalLogEntry[]>([]);
  const [busy, setBusy] = useState(false);
  const [scanning, setScanning] = useState(false);
  const [portSettings, setPortSettings] = useState<LocalSettings | null>(null);
  const [savingPorts, setSavingPorts] = useState(false);
  const [diag, setDiag] = useState<LocalDiagnostics | null>(null);
  const logsRef = useRef<HTMLDivElement>(null);
  // 日志增量游标用 ref 承载：轮询闭包读取，不进入 effect 依赖
  //（否则每次日志更新都会重挂 effect → 反复全量 rescan → UI 卡死）
  const logSinceRef = useRef(0);

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await localApi.status());
    } catch {
      /* app 未就绪时静默 */
    }
  }, []);

  const refreshModels = useCallback(async () => {
    try {
      setModels(await localApi.listModels());
    } catch {
      /* ignore */
    }
  }, []);

  useEffect(() => {
    void refreshStatus();
    void localApi.getSettings().then(setPortSettings).catch(() => undefined);
    // 打开页面即扫描一次（扫描结果 = 权威集合；后台执行不阻塞 UI）
    void (async () => {
      try {
        setModels(await localApi.rescan());
      } catch {
        void refreshModels();
      }
    })();
    const unlisten = listen<LocalServiceStatus>(LOCAL_STATUS_EVENT, (event) => {
      setStatus(event.payload);
    });
    const timer = setInterval(() => {
      void (async () => {
        try {
          const entries = await localApi.logs(logSinceRef.current);
          if (entries.length > 0) {
            setLogs((prev) => [...prev, ...entries].slice(-2000));
            logSinceRef.current = entries[entries.length - 1].id;
          }
        } catch {
          /* ignore */
        }
      })();
    }, 1000);
    return () => {
      void unlisten.then((fn) => fn());
      clearInterval(timer);
    };
  }, [refreshStatus, refreshModels]);

  useEffect(() => {
    const timer = setInterval(() => {
      void localApi.diagnostics().then(setDiag).catch(() => undefined);
    }, 2000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    const el = logsRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
    }
  }, [logs]);

  const busyState = ["validating", "startingMlx", "warmingUp", "startingGateway", "stopping"].includes(
    status.state,
  );

  const handleStart = async (modelPath?: string) => {
    const target = modelPath ?? status.modelPath;
    if (!target) {
      toast.warning(t("localModel.noModel"));
      return;
    }
    setBusy(true);
    try {
      setStatus(await localApi.start(target));
      toast.success(t("localModel.started"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleStop = async () => {
    setBusy(true);
    try {
      setStatus(await localApi.stop());
      toast.success(t("localModel.stopped"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setBusy(false);
    }
  };

  const handleRescan = async () => {
    setScanning(true);
    try {
      setModels(await localApi.rescan());
      toast.success(t("localModel.rescanDone"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setScanning(false);
    }
  };

  const handleAddRoot = async () => {
    try {
      const path = await import("@/lib/api/settings").then((m) =>
        m.settingsApi.pickDirectory(),
      );
      if (!path) return;
      const added = await localApi.addRoot(path);
      await refreshModels();
      toast.success(t("localModel.rootAdded", { count: added }));
    } catch (e) {
      toast.error(String(e));
    }
  };

  const handleRemoveRef = async (model: LocalModelRecord) => {
    try {
      await localApi.removeRef(model.path);
      await refreshModels();
      toast.success(t("localModel.refRemoved"));
    } catch (e) {
      toast.error(String(e));
    }
  };

  const handleTest = async () => {
    try {
      const message = await localApi.test();
      toast.success(message);
    } catch (e) {
      toast.error(String(e));
    }
  };

  const handleSavePorts = async () => {
    if (!portSettings) return;
    setSavingPorts(true);
    try {
      await localApi.setSettings(portSettings);
      toast.success(t("localModel.settingsSaved"));
    } catch (e) {
      toast.error(String(e));
    } finally {
      setSavingPorts(false);
    }
  };

  const mainButton = (() => {
    if (status.state === "ready" || status.state === "degraded") {
      return (
        <Button variant="destructive" onClick={handleStop} disabled={busy}>
          <Square className="w-4 h-4 mr-2" />
          {t("localModel.stopService")}
        </Button>
      );
    }
    return (
      <Button onClick={() => void handleStart()} disabled={busy || busyState}>
        {busyState ? <Loader2 className="w-4 h-4 mr-2 animate-spin" /> : <Play className="w-4 h-4 mr-2" />}
        {busyState ? t("localModel.starting") : t("localModel.startService")}
      </Button>
    );
  })();

  return (
    <div className="space-y-4 p-6 max-w-5xl">
      {/* 状态卡 */}
      <Card>
        <CardHeader className="flex flex-row items-center justify-between">
          <CardTitle className="text-base flex items-center gap-2">
            <Cpu className="w-4 h-4" />
            {t("localModel.title")}
            <Badge variant="secondary" className={cn(STATE_COLOR[status.state])}>
              {t(`localModel.state.${status.state}`)}
            </Badge>
          </CardTitle>
          <div className="flex items-center gap-2">
            {status.state === "ready" && (
              <Button variant="outline" size="sm" onClick={handleTest}>
                {t("localModel.test")}
              </Button>
            )}
            {mainButton}
          </div>
        </CardHeader>
        <CardContent className="space-y-2 text-sm">
          <div className="flex flex-wrap gap-x-6 gap-y-1">
            <span className="text-muted-foreground">{t("localModel.currentModel")}:</span>
            <span className="font-medium">{status.modelName ?? "—"}</span>
            <span className="text-muted-foreground">{t("localModel.baseUrl")}:</span>
            <code className="font-mono text-xs bg-muted px-1.5 py-0.5 rounded">
              {status.baseUrl}
            </code>
          </div>
          {status.phaseDetail && (
            <div className="text-muted-foreground flex items-center gap-1.5">
              {busyState && <Loader2 className="w-3.5 h-3.5 animate-spin" />}
              {status.phaseDetail}
            </div>
          )}
          {status.lastError && (
            <div className="text-red-600 dark:text-red-400 text-xs">
              {status.lastError}
            </div>
          )}
          {status.state === "ready" && (
            <div className="flex items-center gap-1.5">
              <span
                className={cn(
                  "inline-block w-2 h-2 rounded-full",
                  (status.activeRequests ?? 0) > 0
                    ? "bg-green-500 animate-pulse"
                    : "bg-emerald-900/50",
                )}
              />
              {(status.activeRequests ?? 0) > 0
                ? t("localModel.busy", { count: status.activeRequests })
                : t("localModel.idle")}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 模型库 */}
      <Card>
        <CardHeader className="flex-row items-center justify-between">
          <CardTitle className="text-base">{t("localModel.library")}</CardTitle>
          <div className="flex items-center gap-2">
            <Button variant="outline" size="sm" onClick={handleAddRoot}>
              <FolderPlus className="w-4 h-4 mr-1.5" />
              {t("localModel.addRoot")}
            </Button>
            <Button variant="outline" size="sm" onClick={handleRescan} disabled={scanning}>
              {scanning ? (
                <Loader2 className="w-4 h-4 mr-1.5 animate-spin" />
              ) : (
                <RefreshCw className="w-4 h-4 mr-1.5" />
              )}
              {t("localModel.rescan")}
            </Button>
          </div>
        </CardHeader>
        <CardContent>
          {models.length === 0 ? (
            <div className="text-sm text-muted-foreground py-6 text-center">
              {t("localModel.empty")}
            </div>
          ) : (
            <div className="space-y-2">
              {models.map((model) => {
                const isCurrent = status.modelPath === model.path;
                return (
                  <div
                    key={model.path}
                    className={cn(
                      "flex items-center gap-3 rounded-lg border px-3 py-2.5",
                      isCurrent && "border-emerald-500/40 bg-emerald-500/5",
                    )}
                  >
                    <div className="flex-1 min-w-0">
                      <div className="font-medium text-sm truncate">
                        {model.name}
                        {isCurrent && (
                          <span className="ml-2 text-xs text-emerald-600 dark:text-emerald-400">
                            {t("localModel.current")}
                          </span>
                        )}
                      </div>
                      <div className="text-xs text-muted-foreground flex flex-wrap gap-x-3 gap-y-0.5 mt-0.5">
                        <span>{model.quantization}</span>
                        <span>{formatBytes(model.sizeBytes)}</span>
                        <span>
                          {model.contextLength
                            ? `${Math.round(model.contextLength / 1000)}k ctx`
                            : t("localModel.ctxUnknown")}
                        </span>
                        <span>{t(`localModel.fit.${model.memoryFit}`)}</span>
                        {!model.hasChatTemplate && (
                          <span className="text-amber-600 dark:text-amber-400">
                            {t("localModel.noTemplate")}
                          </span>
                        )}
                      </div>
                    </div>
                    <Button
                      variant={isCurrent ? "outline" : "default"}
                      size="sm"
                      disabled={busy || busyState}
                      onClick={() => void handleStart(model.path)}
                    >
                      {isCurrent ? t("localModel.restart") : t("localModel.use")}
                    </Button>
                    <Button
                      variant="ghost"
                      size="icon"
                      className="text-muted-foreground h-8 w-8"
                      title={t("localModel.removeRef")}
                      onClick={() => void handleRemoveRef(model)}
                    >
                      <Trash2 className="w-4 h-4" />
                    </Button>
                  </div>
                );
              })}
            </div>
          )}
        </CardContent>
      </Card>

      {/* 服务设置：端口可配置（网关对 Agent 的 Base URL 不变） */}
      {portSettings && (
        <Card>
          <CardHeader>
            <CardTitle className="text-base">{t("localModel.settings")}</CardTitle>
          </CardHeader>
          <CardContent>
            <div className="space-y-4">
            <div className="grid grid-cols-2 gap-4 max-w-md">
              <div className="space-y-1.5">
                <Label>{t("localModel.backend")}</Label>
                <Select
                  value={portSettings.backend ?? "mlx"}
                  onValueChange={(v) =>
                    setPortSettings({ ...portSettings, backend: v })
                  }
                >
                  <SelectTrigger className="w-full">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="mlx">
                      {t("localModel.backendMlx")}
                    </SelectItem>
                    <SelectItem value="ollama">
                      {t("localModel.backendOllama")}
                    </SelectItem>
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  {t("localModel.backendHint")}
                </p>
              </div>
              {portSettings.backend === "ollama" && (
                <div className="space-y-1.5">
                  <Label htmlFor="ollama-model">
                    {t("localModel.ollamaModel")}
                  </Label>
                  <Input
                    id="ollama-model"
                    value={portSettings.ollama_model}
                    placeholder="qwen3.8:27b-mtp-q4_K_M"
                    onChange={(e) =>
                      setPortSettings({
                        ...portSettings,
                        ollama_model: e.target.value,
                      })
                    }
                  />
                  <p className="text-xs text-muted-foreground">
                    {t("localModel.ollamaModelHint")}
                  </p>
                </div>
              )}
              <div className="space-y-1.5">
                <Label htmlFor="mlx-port">{t("localModel.mlxPort")}</Label>
                <Input
                  id="mlx-port"
                  type="number"
                  value={portSettings.mlx_port}
                  min={1024}
                  max={65535}
                  onChange={(e) =>
                    setPortSettings({
                      ...portSettings,
                      mlx_port: Number(e.target.value),
                    })
                  }
                />
                <p className="text-xs text-muted-foreground">
                  {t("localModel.mlxPortHint")}
                </p>
              </div>
              <div className="space-y-1.5">
                <Label htmlFor="gw-port">{t("localModel.gatewayPort")}</Label>
                <Input
                  id="gw-port"
                  type="number"
                  value={portSettings.gateway_port}
                  min={1024}
                  max={65535}
                  onChange={(e) =>
                    setPortSettings({
                      ...portSettings,
                      gateway_port: Number(e.target.value),
                    })
                  }
                />
                <p className="text-xs text-muted-foreground">
                  {t("localModel.gatewayPortHint")}
                </p>
              </div>
            </div>
            <div className="mt-3 flex items-center gap-3">
              <Button
                size="sm"
                onClick={handleSavePorts}
                disabled={savingPorts}
              >
                {t("common.save")}
              </Button>
              <span className="text-xs text-muted-foreground">
                {t("localModel.portRestartHint")}
              </span>
            </div>
            </div>
          </CardContent>
        </Card>
      )}

      {/* 调试信息：连接、活跃请求与卡点 */}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">
            {t("localModel.debug.title")}
            {diag && (
              <span className="ml-2 text-xs font-normal text-muted-foreground">
                {t("localModel.debug.refreshNote")}
              </span>
            )}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          {!diag ? (
            <div className="text-muted-foreground text-xs">
              {t("localModel.debug.loading")}
            </div>
          ) : (
            <>
              {/* 健康与计数 */}
              <div className="flex flex-wrap gap-x-6 gap-y-1">
                <span>
                  {t("localModel.debug.gateway")}:{" "}
                  <span className={diag.gatewayHealthy ? "text-green-600" : "text-red-500"}>
                    {diag.gatewayHealthy ? "OK" : "DOWN"}
                  </span>
                </span>
                <span>
                  {t("localModel.debug.mlx")}:{" "}
                  <span className={diag.mlxHealthy ? "text-green-600" : "text-red-500"}>
                    {diag.mlxHealthy ? "OK" : "DOWN"}
                  </span>
                </span>
                {diag.metrics && (
                  <>
                    <span>{t("localModel.debug.total")}: {diag.metrics.totalRequests}</span>
                    <span className="text-emerald-600">
                      {t("localModel.debug.completed")}: {diag.metrics.completedRequests}
                    </span>
                    <span className="text-red-500">
                      {t("localModel.debug.failed")}: {diag.metrics.failedRequests}
                    </span>
                    <span className="text-amber-500">
                      {t("localModel.debug.active")}: {diag.metrics.activeRequests}
                    </span>
                    <span>{t("localModel.debug.recv")}: {formatBytes(diag.metrics.requestBytesReceived)}</span>
                    <span>{t("localModel.debug.sent")}: {formatBytes(diag.metrics.responseBytesSent)}</span>
                  </>
                )}
                {!diag.metrics && diag.gatewayHealthy && (
                  <span className="text-muted-foreground">
                    {t("localModel.debug.noMetrics")}
                  </span>
                )}
              </div>

              {/* 活跃请求（卡点） */}
              {diag.metrics && diag.metrics.active.length > 0 && (
                <div className="rounded border border-amber-500/30 bg-amber-500/5 p-2 space-y-1">
                  {diag.metrics.active.map((req) => (
                    <div key={req.id} className="text-xs flex flex-wrap gap-x-4 gap-y-0.5">
                      <span className="font-mono">#{req.id}</span>
                      <span>{req.protocol}{req.streaming ? " · stream" : ""}</span>
                      <span className="text-amber-600">
                        {t("localModel.debug.elapsed")} {Math.round(req.elapsedSeconds ?? 0)}s
                      </span>
                      <span>{t("localModel.debug.reqBytes")}: {formatBytes(req.requestBytes ?? 0)}</span>
                      <span className="text-muted-foreground">
                        {t("localModel.debug.respBytes")}: {formatBytes(req.responseBytes ?? 0)}
                        {req.responseBytes === 0 && ` · ${t("localModel.debug.waitingPrefill")}`}
                      </span>
                    </div>
                  ))}
                  <div className="text-xs text-amber-600/80">
                    {t("localModel.debug.hint")}
                  </div>
                </div>
              )}

              {/* 最近日志（服务侧输出） */}
              <div className="h-32 overflow-y-auto rounded bg-black/80 text-green-300/80 text-xs font-mono p-2 space-y-0.5">
                {diag.recentLogs.length === 0 ? (
                  <span className="opacity-50">{t("localModel.debug.noLogs")}</span>
                ) : (
                  diag.recentLogs.slice(-30).map((entry) => (
                    <div key={entry.id} className="truncate">
                      <span className="opacity-50 mr-2">{new Date(entry.ts).toLocaleTimeString()}</span>
                      <span className="opacity-60 mr-2">[{entry.source}]</span>
                      {entry.line}
                    </div>
                  ))
                )}
              </div>
            </>
          )}
        </CardContent>
      </Card>

      {/* 活动日志 */}
      <Card>
        <CardHeader className="flex-row items-center justify-between">
          <CardTitle className="text-base">{t("localModel.activity")}</CardTitle>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setLogs([]);
              logSinceRef.current = 0;
              void localApi.clearLogs();
            }}
          >
            {t("localModel.clearLogs")}
          </Button>
        </CardHeader>
        <CardContent>
          <div
            ref={logsRef}
            className="h-56 overflow-y-auto rounded bg-black/90 text-muted-foreground text-xs font-mono p-3 space-y-0.5"
          >
            {logs.length === 0 ? (
              <div className="text-muted-foreground/60">{t("localModel.noLogs")}</div>
            ) : (
              logs.map((entry) => (
                <div key={entry.id} className={cn(
                  entry.level === "error" && "text-red-400",
                  entry.level === "warn" && "text-amber-400",
                  entry.source === "mlx-err" && "text-orange-300",
                  entry.source === "gateway-err" && "text-orange-300",
                )}>
                  <span className="opacity-50 mr-2">{formatTs(entry.ts)}</span>
                  <span className="opacity-70 mr-2">[{entry.source}]</span>
                  {entry.line}
                </div>
              ))
            )}
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
