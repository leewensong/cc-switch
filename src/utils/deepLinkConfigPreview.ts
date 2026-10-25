import { parse as parseToml, stringify as stringifyToml } from "smol-toml";
import type { DeepLinkImportRequest } from "@/lib/api/deeplink";
import { decodeBase64Utf8 } from "@/lib/utils/base64";
import { isSensitiveConfigKey, maskSensitiveValue } from "@/utils/deeplinkRisk";

export interface ParsedDeepLinkConfig {
  type: "claude" | "codex" | "gemini" | "grokbuild" | "agy";
  env?: Record<string, string>;
  model?: string;
  auth?: Record<string, string>;
  tomlConfig?: string;
  claudeDesktopModelRoutes?: Array<{
    routeId: string;
    labelOverride?: string;
    supports1m?: boolean;
  }>;
}

const maskStructuredSecrets = (
  value: unknown,
  key = "",
  inheritedSensitive = false,
): unknown => {
  const sensitive = inheritedSensitive || isSensitiveConfigKey(key);
  if (typeof value === "string") {
    return sensitive ? maskSensitiveValue(value) : value;
  }
  if (Array.isArray(value)) {
    return value.map((item) => maskStructuredSecrets(item, key, sensitive));
  }
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>).map(
        ([childKey, childValue]) => [
          childKey,
          maskStructuredSecrets(childValue, childKey, sensitive),
        ],
      ),
    );
  }
  return value;
};

const sanitizeTomlForPreview = (configToml: string): string => {
  const parsed = parseToml(configToml) as Record<string, unknown>;
  return `${stringifyToml(maskStructuredSecrets(parsed) as Record<string, unknown>).trim()}\n`;
};

const isClaudeDesktopSafeRouteId = (value: string): boolean => {
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
};

const hasControlCharacter = (value: string) =>
  Array.from(value).some((character) => {
    const code = character.codePointAt(0) ?? 0;
    return code <= 0x1f || (code >= 0x7f && code <= 0x9f);
  });

const parseClaudeDesktopModelRoutes = (
  value: unknown,
): ParsedDeepLinkConfig["claudeDesktopModelRoutes"] => {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined;
  }

  const routes = new Map<
    string,
    NonNullable<ParsedDeepLinkConfig["claudeDesktopModelRoutes"]>[number]
  >();
  for (const [rawRouteId, rawRoute] of Object.entries(
    value as Record<string, unknown>,
  )) {
    if (routes.size >= 64) break;

    const routeId = rawRouteId.trim();
    if (
      !isClaudeDesktopSafeRouteId(routeId) ||
      !rawRoute ||
      typeof rawRoute !== "object" ||
      Array.isArray(rawRoute)
    ) {
      continue;
    }

    const route = rawRoute as Record<string, unknown>;
    const rawLabel =
      typeof route.labelOverride === "string"
        ? route.labelOverride.trim()
        : undefined;
    const labelOverride =
      rawLabel &&
      Array.from(rawLabel).length <= 256 &&
      !hasControlCharacter(rawLabel)
        ? rawLabel
        : undefined;
    routes.set(routeId, {
      routeId,
      ...(labelOverride ? { labelOverride } : {}),
      ...(typeof route.supports1m === "boolean"
        ? { supports1m: route.supports1m }
        : {}),
    });
  }

  return routes.size > 0 ? Array.from(routes.values()) : undefined;
};

export function parseDeepLinkConfigPreview(
  request: Pick<DeepLinkImportRequest, "app" | "config" | "configFormat">,
): ParsedDeepLinkConfig | null {
  if (!request.config) return null;

  try {
    const decoded = decodeBase64Utf8(request.config);
    const format = request.configFormat?.trim().toLowerCase();

    if (request.app === "grokbuild" && format === "toml") {
      return {
        type: "grokbuild",
        tomlConfig: sanitizeTomlForPreview(decoded),
      };
    }

    const parsed = JSON.parse(decoded) as Record<string, unknown>;
    if (request.app === "claude" || request.app === "claude-desktop") {
      return {
        type: "claude",
        env: (parsed.env as Record<string, string>) || {},
        ...(request.app === "claude-desktop"
          ? {
              claudeDesktopModelRoutes: parseClaudeDesktopModelRoutes(
                parsed.claudeDesktopModelRoutes,
              ),
            }
          : {}),
      };
    }
    if (request.app === "codex") {
      const config = typeof parsed.config === "string" ? parsed.config : "";
      return {
        type: "codex",
        auth: (parsed.auth as Record<string, string>) || {},
        tomlConfig: config ? sanitizeTomlForPreview(config) : "",
      };
    }
    if (request.app === "agy") {
      const env = parsed.env ?? parsed;
      if (!env || typeof env !== "object" || Array.isArray(env)) return null;
      const values = env as Record<string, unknown>;
      const config = parsed.config as
        | { model?: { name?: unknown } }
        | undefined;
      const model = config?.model?.name ?? values.GEMINI_MODEL;
      return {
        type: "agy",
        env: Object.fromEntries(
          ["GEMINI_API_KEY", "GOOGLE_GEMINI_BASE_URL", "GEMINI_BASE_URL"]
            .filter((key) => typeof values[key] === "string")
            .map((key) => [key, values[key] as string]),
        ),
        model: typeof model === "string" ? model : undefined,
      };
    }
    if (request.app === "gemini") {
      return {
        type: "gemini",
        env: parsed as Record<string, string>,
      };
    }
    if (request.app === "grokbuild") {
      const config =
        typeof parsed.config === "string"
          ? parsed.config
          : stringifyToml(parsed);
      return {
        type: "grokbuild",
        tomlConfig: sanitizeTomlForPreview(config),
      };
    }
    return null;
  } catch (error) {
    console.error("Failed to parse deep link config preview:", error);
    return null;
  }
}
