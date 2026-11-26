import { Link } from "@tanstack/react-router";
import { buttonClass } from "@/components/ui";
import type * as G from "@/gen";
import { cn } from "@/lib/cn";
import { until } from "@/lib/format";
import { useTheme } from "@/lib/theme";
import black from "./openai-blossom-black.svg";
import white from "./openai-blossom-white.svg";

export function ChatGptMark({ className }: { className?: string }) {
  const [theme] = useTheme();
  return <img src={theme === "light" ? black : white} alt="" className={cn("size-7 shrink-0", className)} />;
}

export function ManageChatGptUsage({ primary = false }: { primary?: boolean }) {
  return (
    <a
      href="https://chatgpt.com/settings/usage"
      target="_blank"
      rel="noreferrer"
      className={
        primary
          ? buttonClass({ size: "sm", variant: "primary" })
          : "underline underline-offset-2 hover:text-ink"
      }
    >
      Manage usage
    </a>
  );
}

export function CodexPlan({ connection }: { connection?: G.CodexConnection }) {
  return (
    <div className="flex items-center gap-1 text-[12px] text-ink-3">
      <ChatGptMark />
      {connection?.connected && connection.plan_enabled ? (
        <>
          {connection.paused ? "ChatGPT usage limit reached" : "Using ChatGPT plan"}
          <span className="ml-1">
            <ManageChatGptUsage />
          </span>
        </>
      ) : (
        <Link
          to="/settings"
          search={{ tab: "codex" }}
          className="underline underline-offset-2 hover:text-ink"
        >
          Connect your ChatGPT plan
        </Link>
      )}
    </div>
  );
}

export function CodexLimitNotice({ resetAt }: { resetAt?: number }) {
  const reset = resetAt !== undefined && resetAt > Date.now() ? `Resets in ${until(resetAt)}.` : undefined;
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-2 rounded-lg border border-amber/25 bg-amber-soft p-3 text-[13px] text-ink-2">
      <div className="flex min-w-0 flex-1 items-center gap-1">
        <ChatGptMark />
        <div>
          <p className="font-medium">ChatGPT usage limit reached</p>
          {reset && <p>{reset}</p>}
        </div>
      </div>
      <ManageChatGptUsage primary />
    </div>
  );
}
