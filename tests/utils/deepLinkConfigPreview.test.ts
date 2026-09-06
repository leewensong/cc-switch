import { describe, expect, it } from "vitest";
import { parseDeepLinkConfigPreview } from "@/utils/deepLinkConfigPreview";

const encodeBase64 = (value: string) =>
  btoa(String.fromCharCode(...new TextEncoder().encode(value)));

const encodeUrlSafeBase64WithoutPadding = (value: string) =>
  encodeBase64(value)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/, "");

const grokConfig = `[models]
default = "grok-4.5"

[model."grok-4.5"]
model = "grok-4.5"
base_url = "https://relay.example/v1"
name = "Relay"
api_key = "secret-grok-key"
api_backend = "responses"
context_window = 500000
`;

describe("parseDeepLinkConfigPreview", () => {
  it("shows every exported Claude Desktop direct model route", () => {
    const preview = parseDeepLinkConfigPreview({
      app: "claude-desktop",
      config: encodeBase64(
        JSON.stringify({
          env: { ANTHROPIC_BASE_URL: "https://api.example.com/v1" },
          claudeDesktopModelRoutes: {
            "claude-sonnet-4-6": {
              model: "claude-sonnet-4-6",
              labelOverride: "Sonnet 4.6",
              supports1m: true,
            },
            "claude-sonnet-5": { model: "claude-sonnet-5" },
          },
        }),
      ),
      configFormat: "json",
    });

    expect(preview?.claudeDesktopModelRoutes).toEqual([
      {
        routeId: "claude-sonnet-4-6",
        labelOverride: "Sonnet 4.6",
        supports1m: true,
      },
      { routeId: "claude-sonnet-5" },
    ]);
  });

  it("shows the same accepted Desktop routes as the importer", () => {
    const ignoredRoutes = Object.fromEntries(
      Array.from({ length: 64 }, (_, index) => [
        `not-a-claude-route-${index}`,
        { model: `not-a-claude-route-${index}` },
      ]),
    );
    const preview = parseDeepLinkConfigPreview({
      app: "claude-desktop",
      config: encodeBase64(
        JSON.stringify({
          env: {},
          claudeDesktopModelRoutes: {
            ...ignoredRoutes,
            "claude-opus-4-8": {
              model: "ignored-by-importer",
              labelOverride: "unsafe\nlabel",
              supports1m: true,
            },
          },
        }),
      ),
      configFormat: "json",
    });

    expect(preview?.claudeDesktopModelRoutes).toEqual([
      { routeId: "claude-opus-4-8", supports1m: true },
    ]);
  });

  it("deduplicates trimmed route IDs before applying the importer limit", () => {
    const duplicateRoutes = Object.fromEntries(
      Array.from({ length: 64 }, (_, index) => [
        `${" ".repeat(index + 1)}claude-sonnet-4-6`,
        {
          model: "ignored-by-importer",
          labelOverride: `Sonnet ${index}`,
        },
      ]),
    );
    const preview = parseDeepLinkConfigPreview({
      app: "claude-desktop",
      config: encodeBase64(
        JSON.stringify({
          env: {},
          claudeDesktopModelRoutes: {
            ...duplicateRoutes,
            "claude-opus-4-8": { model: "claude-opus-4-8" },
          },
        }),
      ),
      configFormat: "json",
    });

    expect(preview?.claudeDesktopModelRoutes).toEqual([
      { routeId: "claude-sonnet-4-6", labelOverride: "Sonnet 63" },
      { routeId: "claude-opus-4-8" },
    ]);
  });

  it("previews direct Grok Build TOML and masks its API key", () => {
    const preview = parseDeepLinkConfigPreview({
      app: "grokbuild",
      config: encodeBase64(grokConfig),
      configFormat: "toml",
    });

    expect(preview?.type).toBe("grokbuild");
    expect(preview?.tomlConfig).toContain("https://relay.example/v1");
    expect(preview?.tomlConfig).toContain("secr************");
    expect(preview?.tomlConfig).not.toContain("secret-grok-key");
  });

  it("previews wrapped Grok Build config JSON", () => {
    const preview = parseDeepLinkConfigPreview({
      app: "grokbuild",
      config: encodeBase64(JSON.stringify({ config: grokConfig })),
      configFormat: "json",
    });

    expect(preview?.type).toBe("grokbuild");
    expect(preview?.tomlConfig).toContain('default = "grok-4.5"');
    expect(preview?.tomlConfig).not.toContain("secret-grok-key");
  });

  it("previews URL-safe, unpadded, and space-normalized Grok Build TOML", () => {
    let config = "";
    for (let shift = 0; shift < 12; shift += 1) {
      config = `${grokConfig}\n# ${"p".repeat(shift)}🚀`;
      const candidate = encodeBase64(config);
      if (candidate.includes("+") && /=+$/.test(candidate)) break;
    }
    const standard = encodeBase64(config);
    const encoded = encodeUrlSafeBase64WithoutPadding(config);
    const encodedWithSpaces = standard.replace(/\+/g, " ");
    expect(standard).toContain("+");
    expect(standard).toMatch(/=+$/);
    expect(encoded).toContain("-");
    expect(encoded).not.toMatch(/=$/);
    expect(encodedWithSpaces).toContain(" ");

    const urlSafePreview = parseDeepLinkConfigPreview({
      app: "grokbuild",
      config: encoded,
      configFormat: "toml",
    });
    const spaceNormalizedPreview = parseDeepLinkConfigPreview({
      app: "grokbuild",
      config: encodedWithSpaces,
      configFormat: "toml",
    });

    for (const preview of [urlSafePreview, spaceNormalizedPreview]) {
      expect(preview?.type).toBe("grokbuild");
      expect(preview?.tomlConfig).toContain("https://relay.example/v1");
      expect(preview?.tomlConfig).not.toContain("secret-grok-key");
    }
  });

  it("masks authentication headers in nested TOML", () => {
    const config = `${grokConfig}
[mcp.servers.example]
url = "https://mcp.example"
headers = { Authorization = "Bearer top-secret", Cookie = "session=secret", credential = "credential-secret", auth = "auth-secret", safe_header = "visible" }
`;
    const preview = parseDeepLinkConfigPreview({
      app: "grokbuild",
      config: encodeBase64(config),
      configFormat: "toml",
    });

    expect(preview?.tomlConfig).not.toContain("Bearer top-secret");
    expect(preview?.tomlConfig).not.toContain("session=secret");
    expect(preview?.tomlConfig).not.toContain("credential-secret");
    expect(preview?.tomlConfig).not.toContain("auth-secret");
    expect(preview?.tomlConfig).toContain("visible");
  });

  it("inherits sensitivity through nested tables and arrays of tables", () => {
    const config = `${grokConfig}
[model."grok-4.5".auth]
value = "nested-auth-secret"
short = "tiny"
empty = ""
nested = { value = "inline-nested-secret" }

[[model."grok-4.5".credentials]]
value = "array-table-secret"
`;
    const preview = parseDeepLinkConfigPreview({
      app: "grokbuild",
      config: encodeBase64(config),
      configFormat: "toml",
    });

    expect(preview?.tomlConfig).not.toContain("nested-auth-secret");
    expect(preview?.tomlConfig).not.toContain("inline-nested-secret");
    expect(preview?.tomlConfig).not.toContain("array-table-secret");
    expect(preview?.tomlConfig).toContain("nest************");
    expect(preview?.tomlConfig).toContain("inli************");
    expect(preview?.tomlConfig).toContain("arra************");
    expect(preview?.tomlConfig).toContain('short = "****"');
    expect(preview?.tomlConfig).toContain('empty = ""');
  });

  it("also masks secrets in Codex TOML previews", () => {
    const preview = parseDeepLinkConfigPreview({
      app: "codex",
      config: encodeBase64(
        JSON.stringify({
          auth: { OPENAI_API_KEY: "secret-auth-key" },
          config: 'experimental_bearer_token = "secret-config-key"',
        }),
      ),
      configFormat: "json",
    });

    expect(preview?.type).toBe("codex");
    expect(preview?.tomlConfig).not.toContain("secret-config-key");
  });
});
