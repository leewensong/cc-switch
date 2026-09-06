import { describe, expect, it } from "vitest";
import type { Provider } from "@/types";
import { decodeBase64Utf8 } from "@/lib/utils/base64";
import {
  buildProviderImportLink,
  ProviderImportLinkError,
} from "@/utils/providerImportLink";

function makeProvider(overrides: Partial<Provider> = {}): Provider {
  return {
    id: "provider-1",
    name: "Claude Desktop 测试",
    settingsConfig: {
      env: {
        ANTHROPIC_AUTH_TOKEN: "test-key+with&symbols",
        ANTHROPIC_BASE_URL: "https://api.example.com/v1",
        ANTHROPIC_MODEL: "claude-fable-5",
        ANTHROPIC_DEFAULT_HAIKU_MODEL: "claude-haiku-4-5",
        ANTHROPIC_DEFAULT_SONNET_MODEL: "claude-sonnet-5",
        ANTHROPIC_DEFAULT_OPUS_MODEL: "claude-opus-4-8",
        PRIVATE_UNRELATED_SETTING: "must-not-export",
      },
    },
    websiteUrl: "https://example.com",
    icon: "claude",
    ...overrides,
  };
}

describe("buildProviderImportLink", () => {
  it("builds a URL-encoded Claude Desktop import link with only safe config", () => {
    const link = buildProviderImportLink("claude-desktop", makeProvider());
    const url = new URL(link);

    expect(url.protocol).toBe("ccswitch:");
    expect(url.host).toBe("v1");
    expect(url.pathname).toBe("/import");
    expect(url.searchParams.get("resource")).toBe("provider");
    expect(url.searchParams.get("app")).toBe("claude-desktop");
    expect(url.searchParams.get("name")).toBe("Claude Desktop 测试");
    expect(url.searchParams.get("endpoint")).toBe("https://api.example.com/v1");
    expect(url.searchParams.get("apiKey")).toBe("test-key+with&symbols");
    expect(url.searchParams.get("enabled")).toBe("false");
    expect(url.searchParams.get("model")).toBe("claude-fable-5");
    expect(url.searchParams.get("haikuModel")).toBe("claude-haiku-4-5");
    expect(url.searchParams.get("sonnetModel")).toBe("claude-sonnet-5");
    expect(url.searchParams.get("opusModel")).toBe("claude-opus-4-8");

    const config = JSON.parse(
      decodeBase64Utf8(url.searchParams.get("config") ?? ""),
    );
    expect(config.env).toMatchObject({
      ANTHROPIC_AUTH_TOKEN: "test-key+with&symbols",
      ANTHROPIC_BASE_URL: "https://api.example.com/v1",
    });
    expect(config.env.PRIVATE_UNRELATED_SETTING).toBeUndefined();
  });

  it("uses direct Desktop model routes to fill missing model aliases", () => {
    const link = buildProviderImportLink(
      "claude-desktop",
      makeProvider({
        settingsConfig: {
          env: {
            ANTHROPIC_AUTH_TOKEN: "test-key",
            ANTHROPIC_BASE_URL: "https://api.example.com/v1",
          },
        },
        meta: {
          claudeDesktopMode: "direct",
          claudeDesktopModelRoutes: {
            "claude-fable-5": {
              model: "claude-fable-5",
              supports1m: true,
            },
            "claude-sonnet-5": {
              model: "claude-sonnet-5",
              labelOverride: "Sonnet 5",
            },
            "claude-sonnet-4-6": {
              model: "claude-sonnet-4-6",
              labelOverride: "Sonnet 4.6",
              supports1m: true,
            },
          },
        },
      }),
    );
    const url = new URL(link);
    const config = JSON.parse(
      decodeBase64Utf8(url.searchParams.get("config") ?? ""),
    );

    expect(url.searchParams.get("model")).toBe("claude-fable-5[1M]");
    expect(url.searchParams.get("sonnetModel")).toBe("claude-sonnet-5");
    expect(config.env.ANTHROPIC_DEFAULT_SONNET_MODEL_NAME).toBe("Sonnet 5");
    expect(config.claudeDesktopModelRoutes).toEqual({
      "claude-fable-5": {
        model: "claude-fable-5",
        supports1m: true,
      },
      "claude-sonnet-5": {
        model: "claude-sonnet-5",
        labelOverride: "Sonnet 5",
      },
      "claude-sonnet-4-6": {
        model: "claude-sonnet-4-6",
        labelOverride: "Sonnet 4.6",
        supports1m: true,
      },
    });
  });

  it("preserves a Claude API-key auth field through the sanitized config", () => {
    const link = buildProviderImportLink(
      "claude",
      makeProvider({
        settingsConfig: {
          env: {
            ANTHROPIC_API_KEY: "test-x-api-key",
            ANTHROPIC_BASE_URL: "https://api.example.com/v1",
          },
        },
      }),
    );
    const url = new URL(link);
    const config = JSON.parse(
      decodeBase64Utf8(url.searchParams.get("config") ?? ""),
    );

    expect(url.searchParams.get("apiKey")).toBe("test-x-api-key");
    expect(config.env.ANTHROPIC_API_KEY).toBe("test-x-api-key");
    expect(config.env.ANTHROPIC_AUTH_TOKEN).toBeUndefined();
  });

  it("exports a basic Gemini connection without its unrelated environment", () => {
    const link = buildProviderImportLink(
      "gemini",
      makeProvider({
        settingsConfig: {
          env: {
            GEMINI_API_KEY: "gemini-key",
            GOOGLE_GEMINI_BASE_URL: "https://generativelanguage.example/v1beta",
            GEMINI_MODEL: "gemini-3-pro",
            PRIVATE_UNRELATED_SETTING: "must-not-export",
          },
        },
      }),
    );
    const url = new URL(link);

    expect(url.searchParams.get("app")).toBe("gemini");
    expect(url.searchParams.get("endpoint")).toBe(
      "https://generativelanguage.example/v1beta",
    );
    expect(url.searchParams.get("apiKey")).toBe("gemini-key");
    expect(url.searchParams.get("model")).toBe("gemini-3-pro");
    expect(url.searchParams.get("config")).toBeNull();
  });

  it("supports Gemini's legacy GOOGLE_API_KEY credential field", () => {
    const link = buildProviderImportLink(
      "gemini",
      makeProvider({
        settingsConfig: {
          env: {
            GOOGLE_API_KEY: "legacy-gemini-key",
            GOOGLE_GEMINI_BASE_URL: "https://generativelanguage.example/v1beta",
          },
        },
      }),
    );

    expect(new URL(link).searchParams.get("apiKey")).toBe("legacy-gemini-key");
  });

  it("exports a direct Codex Responses connection without arbitrary TOML", () => {
    const link = buildProviderImportLink(
      "codex",
      makeProvider({
        settingsConfig: {
          auth: { OPENAI_API_KEY: "codex-key" },
          config: `model_provider = "custom"
model = "gpt-5.6"

[model_providers.custom]
base_url = "https://responses.example/v1"
wire_api = "responses"
custom_secret = "must-not-export"
`,
        },
        meta: { apiFormat: "openai_responses" },
      }),
    );
    const url = new URL(link);

    expect(url.searchParams.get("app")).toBe("codex");
    expect(url.searchParams.get("endpoint")).toBe(
      "https://responses.example/v1",
    );
    expect(url.searchParams.get("apiKey")).toBe("codex-key");
    expect(url.searchParams.get("model")).toBe("gpt-5.6");
    expect(url.searchParams.get("config")).toBeNull();
  });

  it("exports a direct Grok Build Responses connection without its TOML", () => {
    const link = buildProviderImportLink(
      "grokbuild",
      makeProvider({
        settingsConfig: {
          config: `[models]
default = "grok-4.5"

[model."grok-4.5"]
model = "grok-4.5"
base_url = "https://grok.example/v1"
name = "Grok relay"
api_key = "grok-key"
api_backend = "responses"
context_window = 500000
custom_secret = "must-not-export"
`,
        },
        meta: { apiFormat: "openai_responses" },
      }),
    );
    const url = new URL(link);

    expect(url.searchParams.get("app")).toBe("grokbuild");
    expect(url.searchParams.get("endpoint")).toBe("https://grok.example/v1");
    expect(url.searchParams.get("apiKey")).toBe("grok-key");
    expect(url.searchParams.get("model")).toBe("grok-4.5");
    expect(url.searchParams.get("config")).toBeNull();
  });

  it("rejects Claude Desktop proxy-mode providers", () => {
    expect(() =>
      buildProviderImportLink(
        "claude-desktop",
        makeProvider({ meta: { claudeDesktopMode: "proxy" } }),
      ),
    ).toThrow(ProviderImportLinkError);
    expect(() =>
      buildProviderImportLink(
        "claude-desktop",
        makeProvider({ meta: { claudeDesktopMode: "proxy" } }),
      ),
    ).toThrow("proxy-mode");
  });

  it("rejects provider configurations that need a proxy or managed auth", () => {
    expect(() =>
      buildProviderImportLink(
        "codex",
        makeProvider({
          settingsConfig: {
            auth: { OPENAI_API_KEY: "codex-key" },
            config: `model_provider = "custom"
[model_providers.custom]
base_url = "https://responses.example/v1"
wire_api = "responses"
`,
          },
          meta: { providerType: "codex_oauth" },
        }),
      ),
    ).toThrow("unsupported-configuration");

    expect(() =>
      buildProviderImportLink(
        "claude",
        makeProvider({
          meta: {
            authBinding: {
              source: "managed_account",
              authProvider: "github_copilot",
            },
          },
        }),
      ),
    ).toThrow("unsupported-configuration");
  });

  it("rejects Responses-looking configs whose effective format requires conversion", () => {
    expect(() =>
      buildProviderImportLink(
        "codex",
        makeProvider({
          settingsConfig: {
            auth: { OPENAI_API_KEY: "codex-key" },
            apiFormat: "openai_chat",
            config: `model_provider = "custom"
[model_providers.custom]
base_url = "https://responses.example/v1"
wire_api = "responses"
`,
          },
        }),
      ),
    ).toThrow("unsupported-api-format");

    expect(() =>
      buildProviderImportLink(
        "grokbuild",
        makeProvider({
          settingsConfig: {
            api_format: "anthropic",
            config: `[models]
default = "grok-4.5"

[model."grok-4.5"]
model = "grok-4.5"
base_url = "https://grok.example/v1"
api_key = "grok-key"
api_backend = "responses"
context_window = 500000
`,
          },
        }),
      ),
    ).toThrow("unsupported-api-format");
  });

  it("rejects Claude connections whose legacy settings require conversion", () => {
    expect(() =>
      buildProviderImportLink(
        "claude",
        makeProvider({
          settingsConfig: {
            env: {
              ANTHROPIC_AUTH_TOKEN: "test-key",
              ANTHROPIC_BASE_URL: "https://api.example.com/v1",
            },
            api_format: "openai_chat",
          },
        }),
      ),
    ).toThrow("unsupported-api-format");

    expect(() =>
      buildProviderImportLink(
        "claude-desktop",
        makeProvider({
          settingsConfig: {
            env: {
              ANTHROPIC_AUTH_TOKEN: "test-key",
              ANTHROPIC_BASE_URL: "https://api.example.com/v1",
            },
            openrouter_compat_mode: 1,
          },
        }),
      ),
    ).toThrow("unsupported-api-format");
  });

  it("honors an explicit native Claude format over legacy compatibility flags", () => {
    const link = buildProviderImportLink(
      "claude",
      makeProvider({
        settingsConfig: {
          env: {
            ANTHROPIC_AUTH_TOKEN: "test-key",
            ANTHROPIC_BASE_URL: "https://api.example.com/v1",
          },
          openrouter_compat_mode: true,
        },
        meta: { apiFormat: "anthropic" },
      }),
    );

    expect(new URL(link).searchParams.get("app")).toBe("claude");
  });

  it("rejects ambiguous Claude credentials instead of exporting both", () => {
    expect(() =>
      buildProviderImportLink(
        "claude",
        makeProvider({
          settingsConfig: {
            env: {
              ANTHROPIC_AUTH_TOKEN: "bearer-token",
              ANTHROPIC_API_KEY: "x-api-key",
              ANTHROPIC_BASE_URL: "https://api.example.com/v1",
            },
          },
        }),
      ),
    ).toThrow("ambiguous-credentials");
  });

  it("rejects Desktop providers with more routes than the importer accepts", () => {
    const routes = Object.fromEntries(
      Array.from({ length: 65 }, (_, index) => [
        `claude-sonnet-4-${index}`,
        { model: `claude-sonnet-4-${index}` },
      ]),
    );

    expect(() =>
      buildProviderImportLink(
        "claude-desktop",
        makeProvider({
          meta: {
            claudeDesktopMode: "direct",
            claudeDesktopModelRoutes: routes,
          },
        }),
      ),
    ).toThrow("too-many-model-routes");
  });

  it("rejects direct routes that would be changed or discarded on import", () => {
    expect(() =>
      buildProviderImportLink(
        "claude-desktop",
        makeProvider({
          meta: {
            claudeDesktopMode: "direct",
            claudeDesktopModelRoutes: {
              "claude-sonnet-4-6": {
                model: "different-upstream-model",
              },
            },
          },
        }),
      ),
    ).toThrow("invalid-model-route");
  });

  it("rejects endpoints that the deep-link parser would reject", () => {
    expect(() =>
      buildProviderImportLink(
        "claude",
        makeProvider({
          settingsConfig: {
            env: {
              ANTHROPIC_AUTH_TOKEN: "test-key",
              ANTHROPIC_BASE_URL: "api.example.com/v1",
            },
          },
        }),
      ),
    ).toThrow("invalid-endpoint");
  });

  it("never exports the free-form homepage field", () => {
    const link = buildProviderImportLink(
      "claude",
      makeProvider({
        websiteUrl:
          "https://user:password@example.com/callback?code=private-code&state=private-state",
      }),
    );

    expect(new URL(link).searchParams.get("homepage")).toBeNull();
  });
});
