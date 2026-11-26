import { useMutation, useQueryClient } from "@tanstack/react-query";
import { RefreshCw } from "lucide-react";
import { useState } from "react";
import { Banner, Button, CodeBox, Dialog, Field, Section, Select, Skeleton, toast } from "@/components/ui";
import type * as G from "@/gen";
import { api } from "@/lib/api";
import { until } from "@/lib/format";
import { keys, useCodexSettings, useWorkspaces } from "@/lib/queries";
import { ChatGptMark, ManageChatGptUsage } from "./plan";

function connectionStatus(c: G.CodexConnection): string {
  if (!c.connected) return "Disconnected";
  if (!c.plan_enabled) return "ChatGPT plan permission needed";
  if (!c.paused) return "Using ChatGPT plan";
  const reset = c.reset_at != null && c.reset_at > Date.now() ? ` · resets in ${until(c.reset_at)}` : "";
  return `Usage limit reached${reset}`;
}

export function CodexSettings() {
  const settings = useCodexSettings();
  const workspaces = useWorkspaces().data?.workspaces;
  const client = useQueryClient();
  const [reconnect, setReconnect] = useState<G.CodexConnection>();
  const [disconnect, setDisconnect] = useState<G.CodexConnection>();
  const changed = () => client.invalidateQueries({ queryKey: keys.codex });
  const select = useMutation({
    mutationFn: ({ workspace, connection }: { workspace?: string; connection: string }) =>
      api.codexSelect(workspace, connection || null),
    onSuccess: changed,
    onError: (e) => toast(e.message, "error"),
  });
  const remove = useMutation({
    mutationFn: api.codexDisconnect,
    onSettled: () => {
      setDisconnect(undefined);
      return changed();
    },
    onError: (e) => toast(e.message, "error"),
  });
  const retry = useMutation({
    mutationFn: api.codexRetry,
    onSuccess: changed,
    onError: (e) => toast(e.message, "error"),
  });
  const data = settings.data;
  const origin = window.location.origin;
  const command = `fctl codex connect --server ${origin}`;
  const accounts = data?.connections ?? [];
  const accountName = (c: G.CodexConnection) => {
    if (!c.email) return c.id;
    return accounts.some((other) => other.id !== c.id && other.email === c.email)
      ? `${c.email} · ${c.id.slice(-6)}`
      : c.email;
  };
  const options = accounts.map((c) => ({
    value: c.id,
    label: accountName(c),
    description: connectionStatus(c),
    disabled: !c.connected || !c.plan_enabled,
  }));

  return (
    <Section
      title="Codex"
      description="Codex sessions use your ChatGPT plan. Choose the account they run on."
      actions={
        <Button
          type="button"
          size="sm"
          icon={<RefreshCw className="size-3.5" />}
          loading={settings.isFetching}
          onClick={() => void settings.refetch()}
        >
          Refresh
        </Button>
      }
    >
      {!data &&
        (settings.error ? (
          <Banner tone="error">{settings.error.message}</Banner>
        ) : (
          <Skeleton className="h-16" />
        ))}
      {data?.enabled && (
        <div className="flex flex-col gap-5">
          {accounts.length > 0 && (
            <ul className="divide-y divide-line border-y border-line">
              {accounts.map((c) => (
                <li key={c.id} className="flex flex-wrap items-center gap-x-2 gap-y-1 py-3">
                  <span className="flex size-8 shrink-0 items-center justify-center rounded-md border border-line bg-raised">
                    <ChatGptMark />
                  </span>
                  <div className="min-w-0 flex-1">
                    <p className="truncate text-[13.5px] text-ink" title={accountName(c)}>
                      {accountName(c)}
                    </p>
                    <p className="text-[12px] text-ink-3">{connectionStatus(c)}</p>
                  </div>
                  {(!c.connected || !c.plan_enabled) && (
                    <Button type="button" size="sm" variant="ghost" onClick={() => setReconnect(c)}>
                      {c.connected ? "Enable plan" : "Reconnect"}
                    </Button>
                  )}
                  {c.connected && c.plan_enabled && c.paused && (
                    <Button
                      type="button"
                      size="sm"
                      variant="ghost"
                      disabled={retry.isPending}
                      onClick={() => retry.mutate(c.id)}
                    >
                      Retry
                    </Button>
                  )}
                  {c.connected && (
                    <Button
                      type="button"
                      size="sm"
                      variant="ghost"
                      onClick={() => setDisconnect(c)}
                      aria-label={`Disconnect ${accountName(c)}`}
                    >
                      Disconnect
                    </Button>
                  )}
                </li>
              ))}
            </ul>
          )}
          <div className="text-[13px] text-ink-3">
            <ManageChatGptUsage />
          </div>
          <div className="flex flex-col gap-2 text-[13.5px] text-ink-3">
            <p>Run this on a computer with a browser to connect a ChatGPT account.</p>
            <CodeBox value={command} />
            <p>
              If you haven't signed in to this fctl instance, run{" "}
              <code className="font-mono text-[12px]">fctl login {origin}</code> first.
            </p>
          </div>
          {accounts.length > 0 && (
            <div className="flex flex-col gap-4">
              <Field label="Default account">
                {(id) => (
                  <Select
                    id={id}
                    className="w-full"
                    value={data.default_connection_id ?? ""}
                    options={[{ value: "", label: "No default account" }, ...options]}
                    onChange={(connection) => select.mutate({ connection })}
                    disabled={select.isPending}
                  />
                )}
              </Field>
              {workspaces
                ?.filter((w) => !w.blocked)
                .map((w) => (
                  <Field key={w.id} label={w.name}>
                    {(id) => (
                      <Select
                        id={id}
                        className="w-full"
                        value={
                          data.workspace_connections.find((c) => c.workspace_id === w.id)?.connection_id ?? ""
                        }
                        options={[{ value: "", label: "Use the default account" }, ...options]}
                        onChange={(connection) => select.mutate({ workspace: w.id, connection })}
                        disabled={select.isPending}
                      />
                    )}
                  </Field>
                ))}
            </div>
          )}
        </div>
      )}
      <Dialog
        open={!!reconnect}
        onClose={() => setReconnect(undefined)}
        title={reconnect?.connected ? "Enable ChatGPT plan usage" : "Reconnect ChatGPT"}
        description="Run this on a computer with a browser."
      >
        {reconnect && (
          <CodeBox
            value={`${command} --connection ${reconnect.id}${reconnect.plan_enabled ? "" : " --enable-plan"}`}
          />
        )}
      </Dialog>
      <Dialog
        open={!!disconnect}
        onClose={() => setDisconnect(undefined)}
        title={`Disconnect ${disconnect ? accountName(disconnect) : "ChatGPT"}?`}
        description="Codex sessions using this connection will stop."
      >
        <div className="flex justify-end gap-2">
          <Button type="button" variant="ghost" onClick={() => setDisconnect(undefined)}>
            Cancel
          </Button>
          <Button
            type="button"
            variant="danger"
            loading={remove.isPending}
            onClick={() => disconnect && remove.mutate(disconnect.id)}
          >
            Disconnect
          </Button>
        </div>
      </Dialog>
    </Section>
  );
}
