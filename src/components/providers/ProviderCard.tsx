import { useMemo, useState, useEffect, type ReactNode } from "react";
import {
  AlertTriangle,
  GripVertical,
  ChevronDown,
  ChevronUp,
  Cpu,
  Layers,
} from "lucide-react";
import { useTranslation } from "react-i18next";
import { useQuery } from "@tanstack/react-query";
import type {
  DraggableAttributes,
  DraggableSyntheticListeners,
} from "@dnd-kit/core";
import type { CompositeConfig, Provider } from "@/types";
import type { AppId } from "@/lib/api";
import { authApi } from "@/lib/api";
import { cn } from "@/lib/utils";
import { ProviderIconBox } from "@/components/ProviderIconBox";
import { HoverTip } from "@/components/ui/hover-tip";
import UsageFooter from "@/components/UsageFooter";
import SubscriptionQuotaFooter from "@/components/SubscriptionQuotaFooter";
import CopilotQuotaFooter from "@/components/CopilotQuotaFooter";
import CodexOauthQuotaFooter from "@/components/CodexOauthQuotaFooter";
import XaiOauthQuotaFooter from "@/components/XaiOauthQuotaFooter";
import AgyAccountQuotaFooter from "@/components/AgyAccountQuotaFooter";
import KimiAccountQuotaFooter from "@/components/KimiAccountQuotaFooter";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { PROVIDER_TYPES, TEMPLATE_TYPES } from "@/config/constants";
import {
  extractCodexExperimentalBearerToken,
  extractProviderBaseUrl,
} from "@/utils/providerConfigUtils";
import { resolveManagedAccountId } from "@/lib/authBinding";
import { isMac } from "@/lib/platform";
import {
  isOfficialAccount,
  resolveCodexOfficialIdentity,
} from "@/utils/providerCapabilities";
import { useProviderHealth } from "@/lib/query/failover";
import { useUsageQuery } from "@/lib/query/queries";
import { resolveProviderIcon } from "@/utils/providerIcon";
import { isAdditiveAppId, isProxyAppId } from "@/config/appConfig";
import { isLocalProvider } from "@/lib/api/local";
import { copyText } from "@/lib/clipboard";
import {
  buildProviderImportLink,
  ProviderImportLinkError,
} from "@/utils/providerImportLink";
import { toast } from "sonner";
import { ProviderCardActions } from "./ProviderCardActions";
import type { CardChip, CardPresentation, CardTone } from "./presentation";

interface DragHandleProps {
  attributes: DraggableAttributes;
  listeners: DraggableSyntheticListeners;
  isDragging: boolean;
}

interface ProviderCardProps {
  provider: Provider;
  appId: AppId;
  /** 卡片怎么画：模式色、状态 / 按钮、徽标（由列表按模式算好） */
  presentation: CardPresentation;
  /** 用来决定额度是否自动查询（共存式应用看 isInConfig） */
  isCurrent: boolean;
  isInConfig?: boolean;
  onEdit: (provider: Provider) => void;
  onDelete: (provider: Provider) => void;
  onConfigureUsage: (provider: Provider) => void;
  onOpenWebsite: (url: string) => void;
  onDuplicate?: (provider: Provider) => void;
  onTest?: (provider: Provider) => void;
  onOpenTerminal?: (provider: Provider) => void;
  isTesting?: boolean;
  dragHandleProps?: DragHandleProps;
}

/** 当前那张：模式色边框 + 淡底；共存式应用的默认那家同直连色。 */
const TONE_CLASS: Record<CardTone, string> = {
  direct: "border-direct-border bg-direct-soft",
  route: "border-route-border bg-route-soft",
  stack: "border-stack-border bg-stack-soft",
};

const CHIP_CLASS: Record<CardChip["tone"], string> = {
  outline: "border border-border-strong text-fg-2",
  direct: "bg-direct-soft text-direct-text",
  route: "bg-route-soft text-route-text",
  stack: "bg-stack-soft text-stack-text",
  success: "bg-success-soft text-success-text",
  warning: "bg-warning-soft text-warning-text",
  danger: "bg-danger-soft text-danger-text",
};

export function CardChipBadge({
  chip,
  icon,
}: {
  chip: CardChip;
  /** 可选的小图标（本地模型 / 复合供应商这类自带徽标的类型用） */
  icon?: ReactNode;
}) {
  return (
    <span
      title={chip.title}
      className={cn(
        "inline-flex h-[18px] shrink-0 items-center gap-1 whitespace-nowrap rounded-full px-1.5 text-badge",
        CHIP_CLASS[chip.tone],
        chip.title && "cursor-help",
      )}
    >
      {icon}
      {chip.label}
    </span>
  );
}

function HealthChip({
  consecutiveFailures,
  isHealthy,
}: {
  consecutiveFailures: number;
  isHealthy?: boolean;
}) {
  const { t } = useTranslation();
  const chip: CardChip =
    consecutiveFailures === 0
      ? { key: "health", label: t("health.operational"), tone: "success" }
      : isHealthy !== false
        ? { key: "health", label: t("health.degraded"), tone: "warning" }
        : { key: "health", label: t("health.circuitOpen"), tone: "danger" };
  return <CardChipBadge chip={chip} />;
}

/** 判断是否为官方供应商（无自定义 base URL / API key，直连官方 API） */
function isOfficialProvider(provider: Provider, appId: AppId): boolean {
  if (appId === "kimi") return isOfficialAccount(appId, provider);
  if (provider.category === "official") {
    return true;
  }

  const config = provider.settingsConfig as Record<string, any>;
  if (appId === "claude") {
    const baseUrl = config?.env?.ANTHROPIC_BASE_URL;
    return !baseUrl || (typeof baseUrl === "string" && baseUrl.trim() === "");
  }
  if (appId === "codex") {
    // 无 OPENAI_API_KEY → 使用 Codex CLI 内置 OAuth（官方）
    const apiKey = config?.auth?.OPENAI_API_KEY;
    const bearerToken =
      typeof config?.config === "string"
        ? extractCodexExperimentalBearerToken(config.config)
        : undefined;
    return (
      !bearerToken &&
      (!apiKey || (typeof apiKey === "string" && apiKey.trim() === ""))
    );
  }
  if (appId === "gemini" || appId === "agy") {
    // 无 GEMINI_API_KEY 且无 GOOGLE_GEMINI_BASE_URL → Google OAuth 官方模式
    const apiKey = config?.env?.GEMINI_API_KEY;
    const baseUrl = config?.env?.GOOGLE_GEMINI_BASE_URL;
    return (
      (!apiKey || (typeof apiKey === "string" && apiKey.trim() === "")) &&
      (!baseUrl || (typeof baseUrl === "string" && baseUrl.trim() === ""))
    );
  }
  return false;
}

/** Antigravity 卡片直接显示当前模型，省得进编辑才知道。 */
function extractAgyModel(provider: Provider): string | null {
  const config = provider.settingsConfig as Record<string, any>;
  const model = config?.config?.model?.name;
  if (typeof model === "string" && model.trim()) return model.trim();
  const envModel = config?.env?.GEMINI_MODEL;
  return typeof envModel === "string" && envModel.trim()
    ? envModel.trim()
    : null;
}

const extractApiUrl = (provider: Provider, fallbackText: string) => {
  if (provider.notes?.trim()) {
    return provider.notes.trim();
  }

  if (provider.websiteUrl) {
    return provider.websiteUrl;
  }

  return extractProviderBaseUrl(provider.settingsConfig) ?? fallbackText;
};

export function ProviderCard({
  provider,
  appId,
  presentation,
  isCurrent,
  isInConfig = true,
  onEdit,
  onDelete,
  onConfigureUsage,
  onOpenWebsite,
  onDuplicate,
  onTest,
  onOpenTerminal,
  isTesting,
  dragHandleProps,
}: ProviderCardProps) {
  const { t } = useTranslation();
  const codexOfficialIdentity = resolveCodexOfficialIdentity(appId, provider);
  const managedCodexAccountId = resolveManagedAccountId(
    provider.meta,
    "codex_oauth",
  )?.trim();
  const {
    data: codexAuthStatus,
    isSuccess: isCodexAuthStatusSuccess,
    isError: isCodexAuthStatusError,
  } = useQuery({
    queryKey: ["managed-auth-status", "codex_oauth"],
    queryFn: () => authApi.authGetStatus("codex_oauth"),
    enabled:
      codexOfficialIdentity === "managed_account" &&
      Boolean(managedCodexAccountId),
    staleTime: 30_000,
  });
  const managedCodexAccount = codexAuthStatus?.accounts.find(
    (account) => account.id === managedCodexAccountId,
  );
  const manualNote = provider.notes?.trim() || undefined;
  const providerNameIncludesAccountLogin = Boolean(
    managedCodexAccount?.login &&
      (provider.name.trim() === managedCodexAccount.login ||
        provider.name.trim() ===
          `OpenAI Official (${managedCodexAccount.login})`),
  );

  const { data: health } = useProviderHealth(
    provider.id,
    appId,
    Boolean(presentation.showHealth) && isProxyAppId(appId),
  );

  const fallbackUrlText = t("provider.notConfigured", {
    defaultValue: "未配置接口地址",
  });

  const displayUrl = useMemo(() => {
    return extractApiUrl(provider, fallbackUrlText);
  }, [provider, fallbackUrlText]);

  const agyModel = useMemo(
    () => (appId === "agy" ? extractAgyModel(provider) : null),
    [appId, provider],
  );

  const isClickableUrl = useMemo(() => {
    if (provider.notes?.trim()) {
      return false;
    }
    if (displayUrl === fallbackUrlText) {
      return false;
    }
    return true;
  }, [provider.notes, displayUrl, fallbackUrlText]);

  const isBoundCodexOfficial = codexOfficialIdentity === "managed_account";
  const usageEnabled =
    provider.meta?.usage_script?.enabled ?? isBoundCodexOfficial;
  const isOfficial = isOfficialProvider(provider, appId);
  const supportsOfficialSubscription =
    isOfficial &&
    ["claude", "codex", "gemini", "grokbuild", "agy"].includes(appId);
  const isOfficialSubscriptionUsage =
    provider.meta?.usage_script?.templateType ===
    TEMPLATE_TYPES.OFFICIAL_SUBSCRIPTION;
  const officialSubscriptionEnabled =
    supportsOfficialSubscription && usageEnabled && isOfficialSubscriptionUsage;

  const isCopilot =
    provider.meta?.providerType === PROVIDER_TYPES.GITHUB_COPILOT ||
    provider.meta?.usage_script?.templateType === "github_copilot";
  const isCodexOauth =
    appId === "codex"
      ? isBoundCodexOfficial
      : provider.meta?.providerType === PROVIDER_TYPES.CODEX_OAUTH;
  // xAI OAuth (SuperGrok 反代)：额度经自管 OAuth token 自动显示，与 codex_oauth 同构
  const isXaiOauth = provider.meta?.providerType === PROVIDER_TYPES.XAI_OAUTH;
  // 复合供应商：目录里聚合了别家的模型，本身没有可直连的地址
  const isComposite = provider.meta?.providerType === PROVIDER_TYPES.COMPOSITE;
  const compositeConfig = isComposite
    ? ((provider.settingsConfig as Record<string, unknown>)?.composite as
        | CompositeConfig
        | undefined)
    : undefined;
  const compositeMembers = compositeConfig?.members ?? [];
  const compositeModelCount = compositeMembers.reduce(
    (sum, member) => sum + (member.models?.length ?? 0),
    0,
  );
  // Antigravity 官方卡绑了授权中心的账号：额度查绑定的号（只有 macOS 能切号，其他平台忽略绑定）
  const agyBoundAccountId =
    appId === "agy" && isOfficial && isMac()
      ? resolveManagedAccountId(provider.meta, "agy_oauth")?.trim() || null
      : null;
  // 获取用量数据以判断是否有多套餐
  // 累加模式应用：使用 isInConfig 代替 isCurrent
  const shouldAutoQuery = isAdditiveAppId(appId) ? isInConfig : isCurrent;
  const autoQueryInterval = shouldAutoQuery
    ? provider.meta?.usage_script?.autoQueryInterval || 0
    : 0;

  // Local 卡片：本地推理不计费，本地网关没有 usage 端点 → 不查用量
  const isLocal = isLocalProvider(provider);
  const localUsageDisabled = isLocal;
  // 脚本用量只在「已启用 + 非官方 + 非官方订阅模板 + 非本地」时才查询；展开判定必须复用同一谓词，
  // 因为禁用的 React Query observer 仍会返回同 key 的旧缓存。
  const scriptUsageActive =
    usageEnabled &&
    !isOfficial &&
    !isOfficialSubscriptionUsage &&
    !localUsageDisabled;
  const { data: usage } = useUsageQuery(provider.id, appId, {
    enabled: scriptUsageActive,
    autoQueryInterval: localUsageDisabled ? 0 : autoQueryInterval,
  });

  const isTokenPlan =
    provider.meta?.usage_script?.templateType === "token_plan";
  // 官方订阅的额度窗口不能按普通多套餐展开；缓存残留的旧脚本结果同样不认。
  const hasMultiplePlans =
    scriptUsageActive &&
    !isTokenPlan &&
    usage?.success &&
    usage.data &&
    usage.data.length > 1;

  const [isExpanded, setIsExpanded] = useState(false);
  const expandLabel = isExpanded
    ? t("usage.collapse", { defaultValue: "收起" })
    : t("usage.expand", { defaultValue: "展开" });
  // 复合供应商的成员列表单独展开（跟多套餐展开互不影响）
  const [isCompositeExpanded, setIsCompositeExpanded] = useState(false);
  const [isCopyImportLinkConfirmOpen, setIsCopyImportLinkConfirmOpen] =
    useState(false);

  useEffect(() => {
    if (hasMultiplePlans) {
      setIsExpanded(true);
    }
  }, [hasMultiplePlans]);

  const handleOpenWebsite = () => {
    if (!isClickableUrl) {
      return;
    }
    onOpenWebsite(displayUrl);
  };

  const handleCopyImportLink = async () => {
    try {
      await copyText(buildProviderImportLink(appId, provider));
      toast.success(t("provider.copyImportLinkSuccess"));
    } catch (error) {
      const message =
        error instanceof ProviderImportLinkError
          ? t(`provider.copyImportLinkErrors.${error.code}`)
          : t("provider.copyImportLinkErrors.generic");
      toast.error(message);
    } finally {
      setIsCopyImportLinkConfirmOpen(false);
    }
  };

  // 导入链接只对「能导出成 deeplink 的应用 + 非官方 + 非复合」有意义
  const canCopyImportLink =
    ["claude", "claude-desktop", "gemini", "codex", "grokbuild"].includes(
      appId,
    ) &&
    !isOfficial &&
    !isComposite;

  // 徽标：上游算好的 chips 在前，本地模型 / 复合供应商这两个自有的类型在后
  const cardChips: { chip: CardChip; icon?: ReactNode }[] = [
    ...presentation.chips.map((chip) => ({ chip })),
    ...(isLocal
      ? [
          {
            chip: {
              key: "local",
              label: t("localModel.title"),
              tone: "success" as const,
            },
            icon: <Cpu className="h-3 w-3" />,
          },
        ]
      : []),
    ...(isComposite
      ? [
          {
            chip: {
              key: "composite",
              label: t("provider.composite.badge", {
                providers: compositeMembers.length,
                models: compositeModelCount,
              }),
              tone: "stack" as const,
            },
            icon: <Layers className="h-3 w-3" />,
          },
        ]
      : []),
  ];

  const compositeExpandLabel = isCompositeExpanded
    ? t("usage.collapse", { defaultValue: "收起" })
    : t("usage.expand", { defaultValue: "展开" });

  return (
    <div
      className={cn(
        "group relative rounded-panel border transition-[border-color,box-shadow,background-color] duration-150",
        presentation.tone
          ? TONE_CLASS[presentation.tone]
          : "border-border bg-surface hover:border-border-strong hover:shadow-v7-sm",
        dragHandleProps?.isDragging && "cursor-grabbing shadow-v7-md",
      )}
    >
      <div className="flex min-h-[58px] items-center gap-3 py-2.5 pe-3 ps-2">
        {dragHandleProps ? (
          <button
            type="button"
            className={cn(
              "flex h-6 w-4 shrink-0 cursor-grab items-center justify-center text-fg-3 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 active:cursor-grabbing",
              dragHandleProps.isDragging && "cursor-grabbing opacity-100",
            )}
            aria-label={t("provider.dragHandle")}
            {...dragHandleProps.attributes}
            {...dragHandleProps.listeners}
          >
            <GripVertical className="h-4 w-4" />
          </button>
        ) : (
          <span className="w-4 shrink-0" />
        )}

        <ProviderIconBox
          icon={resolveProviderIcon(appId, provider.icon, provider.iconColor)}
          name={provider.name}
          color={provider.iconColor}
          className={cn(
            // 底色固定为白，图标颜色也要固定：单色（currentColor）图标和首字母
            // fallback 不能继承深色模式下的浅色文字
            "bg-white text-neutral-900",
            presentation.dim && "opacity-60",
          )}
          fallbackClassName="bg-transparent text-neutral-600"
        />

        <div className="min-w-0 flex-1">
          <div className="flex min-w-0 items-center gap-1.5">
            <h3
              className={cn(
                "m-0 min-w-0 truncate text-strong",
                presentation.dim ? "text-fg-2" : "text-fg-1",
              )}
              title={provider.name}
            >
              {provider.name}
            </h3>
            {cardChips.map(({ chip, icon }) => (
              <CardChipBadge key={chip.key} chip={chip} icon={icon} />
            ))}
            {isComposite && compositeMembers.length > 0 && (
              <HoverTip content={compositeExpandLabel}>
                <button
                  type="button"
                  onClick={(e) => {
                    e.stopPropagation();
                    setIsCompositeExpanded(!isCompositeExpanded);
                  }}
                  className="shrink-0 rounded-control p-1 text-fg-3 transition-colors hover:bg-subtle hover:text-fg-1"
                  aria-label={compositeExpandLabel}
                  aria-expanded={isCompositeExpanded}
                >
                  {isCompositeExpanded ? (
                    <ChevronUp size={14} />
                  ) : (
                    <ChevronDown size={14} />
                  )}
                </button>
              </HoverTip>
            )}
            {presentation.showHealth && health && (
              <HealthChip
                consecutiveFailures={health.consecutive_failures}
                isHealthy={health.is_healthy}
              />
            )}
          </div>

          {codexOfficialIdentity && codexOfficialIdentity !== "api_key" ? (
            <div className="mt-0.5 flex min-w-0 items-center gap-2 text-caption text-fg-2">
              {codexOfficialIdentity === "native_login" ? (
                <span className="min-w-0 truncate" title={manualNote}>
                  {manualNote ??
                    t("codex.followCodexLoginDescription", {
                      defaultValue: "账号会随 Codex CLI 当前登录变化",
                    })}
                </span>
              ) : managedCodexAccount ? (
                <>
                  <span
                    className="min-w-0 truncate"
                    title={manualNote ?? managedCodexAccount.login}
                  >
                    {manualNote ??
                      (providerNameIncludesAccountLogin
                        ? t("codex.openAiAccount", {
                            defaultValue: "OpenAI 账号",
                          })
                        : managedCodexAccount.login)}
                  </span>
                  {managedCodexAccount.reauth_required && (
                    <span className="inline-flex shrink-0 items-center gap-1 text-warning-text">
                      <AlertTriangle className="h-3.5 w-3.5" />
                      {t("codexOauth.reauthBadge", "需要重新登录")}
                    </span>
                  )}
                </>
              ) : isCodexAuthStatusError ? (
                <span className="inline-flex min-w-0 items-center gap-1 text-warning-text">
                  <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
                  <span className="truncate">
                    {t("codex.accountStatusUnavailable", {
                      defaultValue: "无法读取账号信息",
                    })}
                  </span>
                </span>
              ) : isCodexAuthStatusSuccess ? (
                <>
                  <span className="inline-flex min-w-0 items-center gap-1 text-sm text-warning-text">
                    <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
                    <span className="truncate">
                      {t("codex.boundAccountUnavailable", {
                        defaultValue: "绑定的账号不可用",
                      })}
                    </span>
                  </span>
                  <button
                    type="button"
                    className="shrink-0 text-body font-medium text-action-text hover:underline"
                    onClick={() => onEdit(provider)}
                  >
                    {t("codex.chooseAccount", {
                      defaultValue: "选择账号",
                    })}
                  </button>
                </>
              ) : (
                <span className="min-w-0 truncate">
                  {t("codex.accountLoading", {
                    defaultValue: "正在加载账号…",
                  })}
                </span>
              )}
            </div>
          ) : agyModel ? (
            <div className="mt-0.5 flex min-w-0 items-center gap-1.5 text-caption text-fg-2">
              <span className="min-w-0 truncate" title={agyModel}>
                {agyModel}
              </span>
            </div>
          ) : displayUrl && !isComposite ? (
            <button
              type="button"
              onClick={handleOpenWebsite}
              className={cn(
                "mt-0.5 inline-flex max-w-full items-center overflow-hidden text-start text-caption text-fg-2",
                isClickableUrl
                  ? "cursor-pointer decoration-border-strong underline-offset-[3px] hover:underline"
                  : "cursor-default",
              )}
              title={displayUrl}
              disabled={!isClickableUrl}
            >
              <span className="min-w-0 truncate">{displayUrl}</span>
            </button>
          ) : null}
        </div>

        <div className="flex shrink-0 items-center gap-3">
          {/* 额度列 136 宽，带重置倒计时时 194 宽（见 QuotaLines） */}
          <div className="max-w-[208px] text-end">
            <div className="flex items-center justify-end gap-1">
              {isComposite ? null : isCopilot ? (
                <CopilotQuotaFooter
                  meta={provider.meta}
                  inline={true}
                  isCurrent={isCurrent}
                />
              ) : isCodexOauth ? (
                !isBoundCodexOfficial || usageEnabled ? (
                  <CodexOauthQuotaFooter
                    meta={provider.meta}
                    inline={true}
                    isCurrent={isCurrent}
                    autoQueryInterval={
                      isBoundCodexOfficial
                        ? (provider.meta?.usage_script?.autoQueryInterval ?? 5)
                        : undefined
                    }
                  />
                ) : null
              ) : isXaiOauth ? (
                <XaiOauthQuotaFooter
                  meta={provider.meta}
                  inline={true}
                  isCurrent={isCurrent}
                />
              ) : appId === "kimi" && isOfficial ? (
                // 每张 Kimi 官方卡就是授权中心里的一个账号，额度按卡上那个号查
                <KimiAccountQuotaFooter
                  accountId={provider.id}
                  name={provider.name}
                  inline={true}
                  isCurrent={isCurrent}
                />
              ) : isOfficial ? (
                officialSubscriptionEnabled ? (
                  agyBoundAccountId ? (
                    <AgyAccountQuotaFooter
                      accountId={agyBoundAccountId}
                      inline={true}
                      isCurrent={isCurrent}
                      autoQueryInterval={
                        provider.meta?.usage_script?.autoQueryInterval ?? 0
                      }
                    />
                  ) : (
                    <SubscriptionQuotaFooter
                      appId={appId}
                      inline={true}
                      isCurrent={isCurrent}
                      autoQueryInterval={
                        provider.meta?.usage_script?.autoQueryInterval ?? 0
                      }
                    />
                  )
                ) : null
              ) : hasMultiplePlans ? (
                <span className="text-caption text-fg-2">
                  {t("usage.multiplePlans", {
                    count: usage?.data?.length || 0,
                    defaultValue: `${usage?.data?.length || 0} 个套餐`,
                  })}
                </span>
              ) : !localUsageDisabled ? (
                <UsageFooter
                  provider={provider}
                  providerId={provider.id}
                  appId={appId}
                  usageEnabled={usageEnabled}
                  isCurrent={isCurrent}
                  isInConfig={isInConfig}
                  inline={true}
                />
              ) : null}
              {hasMultiplePlans && (
                <HoverTip content={expandLabel}>
                  <button
                    onClick={(e) => {
                      e.stopPropagation();
                      setIsExpanded(!isExpanded);
                    }}
                    className="shrink-0 rounded-control p-1 text-fg-3 transition-colors hover:bg-subtle hover:text-fg-1"
                    aria-label={expandLabel}
                    aria-expanded={isExpanded}
                  >
                    {isExpanded ? (
                      <ChevronUp size={14} />
                    ) : (
                      <ChevronDown size={14} />
                    )}
                  </button>
                </HoverTip>
              )}
            </div>
          </div>
          <ProviderCardActions
            providerName={provider.name}
            presentation={presentation}
            onEdit={() => onEdit(provider)}
            onDelete={() => onDelete(provider)}
            onDuplicate={
              onDuplicate && !(appId === "kimi" && isOfficial)
                ? () => onDuplicate(provider)
                : undefined
            }
            onCopyImportLink={
              canCopyImportLink
                ? () => setIsCopyImportLinkConfirmOpen(true)
                : undefined
            }
            onTest={
              // 连通检测对第三方/自定义/Copilot/Codex-OAuth 供应商开放。官方供应商一律不给：
              // 它们 base_url 故意留空、走客户端默认/OAuth 端点，没有可靠的探测目标。
              // 复合供应商没有自己的地址，也不给。
              onTest &&
              appId !== "mcode" &&
              appId !== "kimi" &&
              provider.category !== "official" &&
              !isComposite
                ? () => onTest(provider)
                : undefined
            }
            isTesting={isTesting}
            onConfigureUsage={
              (isOfficial && !supportsOfficialSubscription) ||
              isCopilot ||
              (isCodexOauth && !isBoundCodexOfficial) ||
              isXaiOauth ||
              isComposite
                ? undefined
                : () => onConfigureUsage(provider)
            }
            onOpenTerminal={
              onOpenTerminal ? () => onOpenTerminal(provider) : undefined
            }
          />
        </div>
      </div>

      {isComposite && isCompositeExpanded && compositeMembers.length > 0 && (
        <div className="mx-4 mb-3 border-t border-border pt-3">
          <div className="space-y-1 text-caption text-fg-2">
            {compositeMembers.map((member) => (
              <div
                key={member.providerId}
                className="flex items-center gap-1.5"
              >
                <Layers className="h-3 w-3 shrink-0" />
                <span className="truncate">
                  {member.providerName} ·{" "}
                  {t("provider.composite.modelsCount", {
                    count: member.models?.length ?? 0,
                  })}
                </span>
              </div>
            ))}
          </div>
        </div>
      )}

      {isExpanded && hasMultiplePlans && (
        <div className="mx-4 mb-3 border-t border-border pt-3">
          <UsageFooter
            provider={provider}
            providerId={provider.id}
            appId={appId}
            usageEnabled={usageEnabled}
            isCurrent={isCurrent}
            isInConfig={isInConfig}
            inline={false}
          />
        </div>
      )}

      <ConfirmDialog
        isOpen={isCopyImportLinkConfirmOpen}
        title={t("provider.copyImportLinkConfirmTitle")}
        message={t("provider.copyImportLinkConfirmMessage")}
        confirmText={t("provider.copyImportLinkConfirm")}
        variant="info"
        onConfirm={() => void handleCopyImportLink()}
        onCancel={() => setIsCopyImportLinkConfirmOpen(false)}
      />
    </div>
  );
}
