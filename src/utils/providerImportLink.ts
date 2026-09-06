import type { AppId } from "@/lib/api";
import type { ClaudeDesktopModelRoute, Provider } from "@/types";
import { isOAuthProviderType } from "@/config/constants";
import { parseGrokBuildConfig } from "@/utils/grokBuildConfig";
import {
  extractCodexBaseUrl,
  extractCodexModelName,
  extractCodexWireApi,
} from "@/utils/providerConfigUtils";

const MAX_IMPORT_LINK_LENGTH = 16 * 1024;
const MAX_CLAUDE_DESKTOP_MODEL_ROUTES = 64;

const EXPORTED_CLAUDE_ENV_KEYS = [
  "ANTHROPIC_AUTH_TOKEN",
  "ANTHROPIC_API_KEY",
  "ANTHROPIC_BASE_URL",
  "ANTHROPIC_MODEL",
  "ANTHROPIC_DEFAULT_HAIKU_MODEL",
  "ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME",
  "ANTHROPIC_DEFAULT_SONNET_MODEL",
  "ANTHROPIC_DEFAULT_SONNET_MODEL_NAME",
  "ANTHROPIC_DEFAULT_OPUS_MODEL",
  "ANTHROPIC_DEFAULT_OPUS_MODEL_NAME",
  "ANTHROPIC_DEFAULT_FABLE_MODEL",
  "ANTHROPIC_DEFAULT_FABLE_MODEL_NAME",
] as const;

type ProviderImportLinkErrorCode =
  | "unsupported-app"
  | "proxy-mode"
  | "unsupported-api-format"
  | "unsupported-configuration"
  | "ambiguous-credentials"
  | "too-many-model-routes"
  | "invalid-model-route"
  | "missing-endpoint"
  | "invalid-endpoint"
  | "missing-api-key"
  | "desktop-api-key-field"
  | "link-too-long";

export class ProviderImportLinkError extends Error {
  constructor(public readonly code: ProviderImportLinkErrorCode) {
    super(code);
    this.name = "ProviderImportLinkError";
  }
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function isHttpUrl(value: string): boolean {
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

function isValidEndpointList(value: string): boolean {
  const endpoints = value
    .split(",")
    .map((endpoint) => endpoint.trim())
    .filter(Boolean);
  return endpoints.length > 0 && endpoints.every(isHttpUrl);
}

function encodeBase64Utf8(value: string): string {
  const bytes = new TextEncoder().encode(value);
  let binary = "";
  for (const byte of bytes) {
    binary += String.fromCharCode(byte);
  }
  return btoa(binary);
}

function routeRole(
  routeId: string,
): "haiku" | "sonnet" | "opus" | "fable" | undefined {
  const route = routeId.toLowerCase();
  if (route.includes("haiku")) return "haiku";
  if (route.includes("sonnet")) return "sonnet";
  if (route.includes("opus")) return "opus";
  if (route.includes("fable")) return "fable";
  return undefined;
}

function routeModelValue(
  route: ClaudeDesktopModelRoute,
  routeId: string,
): string | undefined {
  const model = stringValue(route.model) ?? stringValue(routeId);
  if (!model) return undefined;
  return route.supports1m && !/\[1m\]$/i.test(model) ? `${model}[1M]` : model;
}

function isClaudeDesktopSafeRouteId(value: string): boolean {
  const normalized = value.trim().toLowerCase();
  if (normalized.includes("[1m]")) return false;

  const routeTail = normalized.startsWith("anthropic/claude-")
    ? normalized.slice("anthropic/claude-".length)
    : normalized.startsWith("claude-")
      ? normalized.slice("claude-".length)
      : undefined;
  return Boolean(
    routeTail &&
      ["sonnet-", "opus-", "haiku-", "fable-"].some(
        (role) => routeTail.startsWith(role) && routeTail.length > role.length,
      ),
  );
}

function addDirectDesktopModels(
  env: Record<string, string>,
  routes: Record<string, ClaudeDesktopModelRoute> | undefined,
) {
  if (!routes) return;

  for (const [routeId, route] of Object.entries(routes)) {
    const model = routeModelValue(route, routeId);
    const role = routeRole(routeId);
    if (!model || !role) continue;

    const modelKey = `ANTHROPIC_DEFAULT_${role.toUpperCase()}_MODEL`;
    const nameKey = `ANTHROPIC_DEFAULT_${role.toUpperCase()}_MODEL_NAME`;
    env[modelKey] ??= model;
    if (route.labelOverride?.trim()) {
      env[nameKey] ??= route.labelOverride.trim();
    }
  }

  if (!env.ANTHROPIC_MODEL) {
    env.ANTHROPIC_MODEL =
      env.ANTHROPIC_DEFAULT_FABLE_MODEL ??
      env.ANTHROPIC_DEFAULT_SONNET_MODEL ??
      env.ANTHROPIC_DEFAULT_OPUS_MODEL ??
      env.ANTHROPIC_DEFAULT_HAIKU_MODEL ??
      "";
    if (!env.ANTHROPIC_MODEL) delete env.ANTHROPIC_MODEL;
  }
}

function exportDirectDesktopRoutes(
  routes: Record<string, ClaudeDesktopModelRoute> | undefined,
): Record<string, ClaudeDesktopModelRoute> | undefined {
  if (!routes) return undefined;

  const exported: Record<string, ClaudeDesktopModelRoute> = {};
  for (const [rawRouteId, route] of Object.entries(routes)) {
    const routeId = stringValue(rawRouteId);
    const sourceModel = stringValue(route.model);
    if (
      !routeId ||
      !isClaudeDesktopSafeRouteId(routeId) ||
      (sourceModel && sourceModel !== routeId) ||
      (route.supports1m !== undefined && typeof route.supports1m !== "boolean")
    ) {
      throw new ProviderImportLinkError("invalid-model-route");
    }

    // Direct-mode providers may only expose the Claude Desktop route itself;
    // never turn an internal mapping into an exported upstream model mapping.
    const labelOverride = stringValue(route.labelOverride);
    if (
      labelOverride &&
      (Array.from(labelOverride).length > 256 ||
        /[\u0000-\u001F\u007F]/.test(labelOverride))
    ) {
      throw new ProviderImportLinkError("invalid-model-route");
    }
    exported[routeId] = {
      model: routeId,
      ...(labelOverride ? { labelOverride } : {}),
      ...(route.supports1m !== undefined
        ? { supports1m: route.supports1m }
        : {}),
    };
  }

  if (Object.keys(exported).length > MAX_CLAUDE_DESKTOP_MODEL_ROUTES) {
    throw new ProviderImportLinkError("too-many-model-routes");
  }

  return Object.keys(exported).length > 0 ? exported : undefined;
}

interface ProviderImportLinkPayload {
  endpoint: string;
  apiKey: string;
  model?: string;
  haikuModel?: string;
  sonnetModel?: string;
  opusModel?: string;
  config?: Record<string, unknown>;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

function requireEndpoint(value: unknown): string {
  const endpoint = stringValue(value);
  if (!endpoint) throw new ProviderImportLinkError("missing-endpoint");
  return endpoint;
}

function requireApiKey(value: unknown): string {
  const apiKey = stringValue(value);
  if (!apiKey) throw new ProviderImportLinkError("missing-api-key");
  return apiKey;
}

function assertBasicConnectionCanBeExported(provider: Provider) {
  if (
    provider.category === "official" ||
    provider.meta?.isFullUrl ||
    provider.meta?.authBinding?.source === "managed_account" ||
    isOAuthProviderType(provider.meta?.providerType)
  ) {
    throw new ProviderImportLinkError("unsupported-configuration");
  }
}

function effectiveSettingsApiFormat(provider: Provider): string | undefined {
  const settings = asRecord(provider.settingsConfig);
  return (
    stringValue(provider.meta?.apiFormat) ??
    stringValue(settings.api_format) ??
    stringValue(settings.apiFormat)
  );
}

function assertResponsesConnectionCanBeExported(provider: Provider) {
  const apiFormat = effectiveSettingsApiFormat(provider);
  if (apiFormat && apiFormat !== "openai_responses") {
    throw new ProviderImportLinkError("unsupported-api-format");
  }
}

function legacyOpenRouterCompatEnabled(value: unknown): boolean {
  if (typeof value === "boolean") return value;
  if (typeof value === "number") return Number.isInteger(value) && value !== 0;
  if (typeof value === "string") {
    return ["true", "1"].includes(value.trim().toLowerCase());
  }
  return false;
}

function effectiveClaudeApiFormat(
  provider: Provider,
): "anthropic" | "openai_chat" | "openai_responses" | "gemini_native" {
  const settings = asRecord(provider.settingsConfig);
  const declared =
    provider.meta?.apiFormat ??
    (typeof settings.api_format === "string" ? settings.api_format : undefined);
  if (
    declared === "openai_chat" ||
    declared === "openai_responses" ||
    declared === "gemini_native"
  ) {
    return declared;
  }
  if (declared !== undefined) return "anthropic";
  return legacyOpenRouterCompatEnabled(settings.openrouter_compat_mode)
    ? "openai_chat"
    : "anthropic";
}

function buildClaudePayload(
  appId: "claude" | "claude-desktop",
  provider: Provider,
): ProviderImportLinkPayload {
  if (
    appId === "claude-desktop" &&
    provider.meta?.claudeDesktopMode === "proxy"
  ) {
    throw new ProviderImportLinkError("proxy-mode");
  }
  if (effectiveClaudeApiFormat(provider) !== "anthropic") {
    throw new ProviderImportLinkError("unsupported-api-format");
  }

  const envObject = asRecord(provider.settingsConfig?.env);
  const env: Record<string, string> = {};
  for (const key of EXPORTED_CLAUDE_ENV_KEYS) {
    const value = stringValue(envObject[key]);
    if (value) env[key] = value;
  }

  const authToken = stringValue(env.ANTHROPIC_AUTH_TOKEN);
  const apiKeyField = stringValue(env.ANTHROPIC_API_KEY);
  if (authToken && apiKeyField) {
    // The two fields select different upstream auth headers. Choosing one
    // silently would both change behavior and leak an unnecessary credential.
    throw new ProviderImportLinkError("ambiguous-credentials");
  }
  if (authToken) {
    delete env.ANTHROPIC_API_KEY;
  } else if (apiKeyField) {
    delete env.ANTHROPIC_AUTH_TOKEN;
  }

  const desktopRoutes =
    appId === "claude-desktop"
      ? exportDirectDesktopRoutes(provider.meta?.claudeDesktopModelRoutes)
      : undefined;
  if (appId === "claude-desktop") {
    addDirectDesktopModels(env, desktopRoutes);
    if (!env.ANTHROPIC_AUTH_TOKEN) {
      // Claude Desktop's direct profile currently uses bearer authentication.
      throw new ProviderImportLinkError("desktop-api-key-field");
    }
  }

  return {
    endpoint: requireEndpoint(env.ANTHROPIC_BASE_URL),
    apiKey: requireApiKey(env.ANTHROPIC_AUTH_TOKEN ?? env.ANTHROPIC_API_KEY),
    model: stringValue(env.ANTHROPIC_MODEL),
    haikuModel: stringValue(env.ANTHROPIC_DEFAULT_HAIKU_MODEL),
    sonnetModel: stringValue(env.ANTHROPIC_DEFAULT_SONNET_MODEL),
    opusModel: stringValue(env.ANTHROPIC_DEFAULT_OPUS_MODEL),
    config: {
      env,
      ...(desktopRoutes ? { claudeDesktopModelRoutes: desktopRoutes } : {}),
    },
  };
}

function buildGeminiPayload(provider: Provider): ProviderImportLinkPayload {
  const env = asRecord(provider.settingsConfig?.env);
  return {
    endpoint: requireEndpoint(
      stringValue(env.GOOGLE_GEMINI_BASE_URL) ??
        stringValue(env.GEMINI_BASE_URL),
    ),
    apiKey: requireApiKey(
      stringValue(env.GEMINI_API_KEY) ?? stringValue(env.GOOGLE_API_KEY),
    ),
    model: stringValue(env.GEMINI_MODEL),
  };
}

function buildCodexPayload(provider: Provider): ProviderImportLinkPayload {
  assertResponsesConnectionCanBeExported(provider);

  const settings = asRecord(provider.settingsConfig);
  const config = stringValue(settings.config);
  if (
    !config ||
    stringValue(extractCodexWireApi(config))?.toLowerCase() !== "responses"
  ) {
    throw new ProviderImportLinkError("unsupported-configuration");
  }
  const auth = asRecord(settings.auth);
  return {
    endpoint: requireEndpoint(extractCodexBaseUrl(config)),
    apiKey: requireApiKey(auth.OPENAI_API_KEY),
    model: stringValue(extractCodexModelName(config)),
  };
}

function buildGrokBuildPayload(provider: Provider): ProviderImportLinkPayload {
  assertResponsesConnectionCanBeExported(provider);

  const config = stringValue(asRecord(provider.settingsConfig).config);
  const values = parseGrokBuildConfig(config, provider.name);
  if (
    values.apiBackend.trim().toLowerCase() !== "responses" ||
    stringValue(values.envKey)
  ) {
    throw new ProviderImportLinkError("unsupported-configuration");
  }
  return {
    endpoint: requireEndpoint(values.baseUrl),
    apiKey: requireApiKey(values.apiKey),
    model: stringValue(values.upstreamModel),
  };
}

function createProviderImportLink(
  appId: Exclude<AppId, "pi">,
  provider: Provider,
  payload: ProviderImportLinkPayload,
): string {
  if (!isValidEndpointList(payload.endpoint)) {
    throw new ProviderImportLinkError("invalid-endpoint");
  }

  const params = new URLSearchParams({
    resource: "provider",
    app: appId,
    name: provider.name,
    endpoint: payload.endpoint,
    apiKey: payload.apiKey,
    enabled: "false",
  });
  if (payload.config) {
    params.set("config", encodeBase64Utf8(JSON.stringify(payload.config)));
    params.set("configFormat", "json");
  }
  if (payload.model) params.set("model", payload.model);
  if (payload.haikuModel) params.set("haikuModel", payload.haikuModel);
  if (payload.sonnetModel) params.set("sonnetModel", payload.sonnetModel);
  if (payload.opusModel) params.set("opusModel", payload.opusModel);

  const icon = stringValue(provider.icon);
  // The importer can infer a homepage from the endpoint. Do not export this
  // free-form field: it can contain URL credentials, OAuth callback state, or
  // other private query parameters unrelated to the connection itself.
  if (icon) params.set("icon", icon);

  const link = `ccswitch://v1/import?${params.toString()}`;
  if (link.length > MAX_IMPORT_LINK_LENGTH) {
    throw new ProviderImportLinkError("link-too-long");
  }
  return link;
}

/**
 * Build a deliberately narrow provider deeplink for configurations whose
 * protocol can be represented by the v1 provider link. The link carries core
 * credentials and model selection only; it never exports scripts, notes,
 * custom headers, OAuth material, or arbitrary advanced configuration.
 */
export function buildProviderImportLink(
  appId: AppId,
  provider: Provider,
): string {
  if (
    appId !== "claude" &&
    appId !== "claude-desktop" &&
    appId !== "gemini" &&
    appId !== "codex" &&
    appId !== "grokbuild"
  ) {
    throw new ProviderImportLinkError("unsupported-app");
  }

  assertBasicConnectionCanBeExported(provider);

  const payload =
    appId === "claude" || appId === "claude-desktop"
      ? buildClaudePayload(appId, provider)
      : appId === "gemini"
        ? buildGeminiPayload(provider)
        : appId === "codex"
          ? buildCodexPayload(provider)
          : buildGrokBuildPayload(provider);
  return createProviderImportLink(appId, provider, payload);
}
