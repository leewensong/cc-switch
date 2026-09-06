import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { Layers, Loader2, TriangleAlert } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Input } from "@/components/ui/input";
import { FullScreenPanel } from "@/components/common/FullScreenPanel";
import type { Provider } from "@/types";
import type { AppId } from "@/lib/api";
import {
  fetchModelsForConfig,
  showFetchModelsError,
} from "@/lib/api/model-fetch";
import {
  extractCodexModelName,
  getApiKeyFromConfig,
} from "@/utils/providerConfigUtils";
import { PROVIDER_TYPES, isOAuthProviderType } from "@/config/constants";

const MAX_MEMBERS = 3;
const MIN_MEMBERS = 2;

interface CompositeProviderDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  appId: AppId;
  providers: Provider[];
  onSubmit: (provider: Omit<Provider, "id">) => Promise<void> | void;
}

type MemberModelsState =
  | { status: "loading"; models: [] }
  | { status: "loaded"; models: { model: string; displayName?: string }[] }
  | { status: "error"; models: [] };

function codexMemberModels(provider: Provider) {
  const config = provider.settingsConfig as Record<string, unknown>;
  const catalogModels = (
    config?.modelCatalog as { models?: { model?: string; displayName?: string; display_name?: string }[] } | undefined
  )?.models;

  const models = Array.isArray(catalogModels)
    ? catalogModels
        .map((entry) => {
          const model = typeof entry?.model === "string" ? entry.model.trim() : "";
          if (!model) return null;
          const displayName =
            (typeof entry?.displayName === "string" && entry.displayName.trim()) ||
            (typeof entry?.display_name === "string" && entry.display_name.trim()) ||
            model;
          return { model, displayName };
        })
        .filter((entry): entry is { model: string; displayName: string } => entry !== null)
    : [];

  if (models.length > 0) return models;

  const topLevelModel =
    typeof config?.model === "string" ? config.model.trim() : "";
  if (topLevelModel) return [{ model: topLevelModel, displayName: topLevelModel }];

  const configModel = extractCodexModelName(
    typeof config?.config === "string" ? config.config : undefined,
  );
  if (configModel) return [{ model: configModel, displayName: configModel }];

  return [];
}

export function CompositeProviderDialog({
  open,
  onOpenChange,
  appId,
  providers,
  onSubmit,
}: CompositeProviderDialogProps) {
  const { t } = useTranslation();
  const [name, setName] = useState("");
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [memberModels, setMemberModels] = useState<
    Record<string, MemberModelsState>
  >({});
  const [isSubmitting, setIsSubmitting] = useState(false);

  const candidates = useMemo(
    () =>
      providers.filter(
        (provider) =>
          provider.category !== "official" &&
          provider.meta?.providerType !== PROVIDER_TYPES.COMPOSITE &&
          !isOAuthProviderType(provider.meta?.providerType),
      ),
    [providers],
  );

  useEffect(() => {
    if (open) {
      setName("");
      setSelectedIds([]);
      setMemberModels({});
      setIsSubmitting(false);
    }
  }, [open]);

  const loadMemberModels = useCallback(
    async (provider: Provider) => {
      const providerId = provider.id;

      if (appId === "codex") {
        const models = codexMemberModels(provider);
        setMemberModels((prev) => ({
          ...prev,
          [providerId]: models.length
            ? { status: "loaded", models }
            : { status: "error", models: [] },
        }));
        return;
      }

      const config = provider.settingsConfig as Record<string, unknown>;
      const baseUrl =
        (config?.env as Record<string, unknown> | undefined)?.ANTHROPIC_BASE_URL;
      const apiKey = getApiKeyFromConfig(JSON.stringify(provider.settingsConfig), "claude");

      if (typeof baseUrl !== "string" || !baseUrl.trim() || !apiKey) {
        setMemberModels((prev) => ({
          ...prev,
          [providerId]: { status: "error", models: [] },
        }));
        showFetchModelsError(null, t, {
          hasApiKey: Boolean(apiKey),
          hasBaseUrl: typeof baseUrl === "string" && Boolean(baseUrl.trim()),
        });
        return;
      }

      setMemberModels((prev) => ({
        ...prev,
        [providerId]: { status: "loading", models: [] },
      }));

      try {
        const fetched = await fetchModelsForConfig(
          baseUrl.trim(),
          apiKey,
          provider.meta?.isFullUrl === true,
          undefined,
          provider.meta?.customUserAgent,
          { apiFormat: provider.meta?.apiFormat },
        );
        const models = fetched
          .map((entry) => ({ model: entry.id, displayName: entry.id }))
          .filter((entry) => entry.model);
        setMemberModels((prev) => ({
          ...prev,
          [providerId]: models.length
            ? { status: "loaded", models }
            : { status: "error", models: [] },
        }));
      } catch (err) {
        setMemberModels((prev) => ({
          ...prev,
          [providerId]: { status: "error", models: [] },
        }));
        showFetchModelsError(err, t);
      }
    },
    [appId, t],
  );

  const toggleMember = useCallback(
    (provider: Provider) => {
      if (selectedIds.includes(provider.id)) {
        setSelectedIds(selectedIds.filter((id) => id !== provider.id));
        setMemberModels((prevModels) => {
          const next = { ...prevModels };
          delete next[provider.id];
          return next;
        });
        return;
      }
      if (selectedIds.length >= MAX_MEMBERS) return;
      setSelectedIds([...selectedIds, provider.id]);
      void loadMemberModels(provider);
    },
    [selectedIds, loadMemberModels],
  );

  const selectedProviders = useMemo(
    () =>
      selectedIds
        .map((id) => candidates.find((provider) => provider.id === id))
        .filter((provider): provider is Provider => Boolean(provider)),
    [selectedIds, candidates],
  );

  const canSubmit =
    Boolean(name.trim()) &&
    selectedProviders.length >= MIN_MEMBERS &&
    selectedProviders.length <= MAX_MEMBERS &&
    selectedProviders.every(
      (provider) => (memberModels[provider.id]?.models.length ?? 0) > 0,
    ) &&
    !isSubmitting;

  const handleSubmit = useCallback(async () => {
    if (!canSubmit) return;
    setIsSubmitting(true);
    try {
      await onSubmit({
        name: name.trim(),
        settingsConfig: {
          composite: {
            members: selectedProviders.map((provider) => ({
              providerId: provider.id,
              providerName: provider.name,
              models: memberModels[provider.id].models,
            })),
          },
        },
        meta: { providerType: PROVIDER_TYPES.COMPOSITE },
      });
      onOpenChange(false);
    } catch (error) {
      console.error("[CompositeProviderDialog] Failed to create composite", error);
    } finally {
      setIsSubmitting(false);
    }
  }, [canSubmit, name, selectedProviders, memberModels, onSubmit, onOpenChange]);

  const totalModels = selectedProviders.reduce(
    (sum, provider) => sum + (memberModels[provider.id]?.models.length ?? 0),
    0,
  );

  return (
    <FullScreenPanel
      isOpen={open}
      title={t("provider.composite.createTitle")}
      onClose={() => onOpenChange(false)}
      footer={
        <Button
          onClick={() => void handleSubmit()}
          disabled={!canSubmit}
          className="bg-primary text-primary-foreground hover:bg-primary/90"
        >
          {isSubmitting && <Loader2 className="h-4 w-4 mr-2 animate-spin" />}
          <Layers className="h-4 w-4 mr-2" />
          {t("provider.composite.submit")}
        </Button>
      }
    >
      <div className="space-y-2">
        <label className="text-sm font-medium" htmlFor="composite-name">
          {t("provider.composite.nameLabel")}
        </label>
        <Input
          id="composite-name"
          value={name}
          onChange={(event) => setName(event.target.value)}
          placeholder={t("provider.composite.namePlaceholder")}
        />
      </div>

      <div className="space-y-2">
        <div className="text-sm font-medium">
          {t("provider.composite.selectMembers", {
            min: MIN_MEMBERS,
            max: MAX_MEMBERS,
          })}
        </div>
        <div className="rounded-lg border divide-y">
          {candidates.length === 0 && (
            <div className="p-4 text-sm text-muted-foreground">
              {t("provider.composite.noCandidates")}
            </div>
          )}
          {candidates.map((provider) => {
            const isSelected = selectedIds.includes(provider.id);
            const disabled = !isSelected && selectedIds.length >= MAX_MEMBERS;
            const state = memberModels[provider.id];
            return (
              <div key={provider.id} className="flex items-center gap-3 px-3 py-2.5">
                <Checkbox
                  checked={isSelected}
                  disabled={disabled}
                  onCheckedChange={() => toggleMember(provider)}
                  aria-label={provider.name}
                />
                <div className="flex-1 min-w-0">
                  <div className="text-sm font-medium truncate">{provider.name}</div>
                  {isSelected && state && (
                    <div className="text-xs text-muted-foreground flex items-center gap-1.5">
                      {state.status === "loading" && (
                        <>
                          <Loader2 className="h-3 w-3 animate-spin" />
                          {t("provider.composite.memberModelsLoading")}
                        </>
                      )}
                      {state.status === "error" && (
                        <>
                          <TriangleAlert className="h-3 w-3" />
                          {t("provider.composite.memberModelsFailed")}
                        </>
                      )}
                      {state.status === "loaded" &&
                        t("provider.composite.modelsCount", {
                          count: state.models.length,
                        })}
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      </div>

      <div className="rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2.5 text-sm text-amber-700 dark:text-amber-400">
        {t("provider.composite.routingHint")}
      </div>

      {selectedProviders.length > 0 && (
        <div className="text-xs text-muted-foreground">
          {t("provider.composite.summary", {
            providers: selectedProviders.length,
            models: totalModels,
          })}
        </div>
      )}
    </FullScreenPanel>
  );
}