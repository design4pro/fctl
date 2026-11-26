// The model and effort level sessions run with. A project sets them for its
// sessions (ProjectSettings.model and .effort), and a session can ask for
// others as it starts (CreateSessionRequest), which the composer does. A model
// is Claude's default, the latest Opus, Sonnet, Haiku or Fable by Claude
// Code's alias for it, or a model by its ID; an effort level is Claude's
// default (the model's), or low … max.

import type * as G from "@/gen";
import { projectGithub, within } from "./github-access";

export const AGENTS: ReadonlyArray<{ value: G.AgentKind; label: string }> = [
  { value: "claude", label: "Claude" },
  { value: "codex", label: "Codex" },
];

export const planName = (agent?: G.AgentKind) => (agent === "codex" ? "ChatGPT plan" : "Claude subscription");

/** Claude Code's aliases for the latest model of each family, as lists show them. */
export const ALIASES = [
  { value: "opus", label: "Opus" },
  { value: "sonnet", label: "Sonnet" },
  { value: "haiku", label: "Haiku" },
  { value: "fable", label: "Fable" },
] as const;

type Alias = (typeof ALIASES)[number]["value"];

/** What the project's model field shows: "" for Claude's default, an alias, or "id" for a model ID. */
export type ModelChoice = "" | Alias | "id";

/** The choice that shows `model`: its alias, or "id" for anything else. */
export function modelChoice(model?: string): ModelChoice {
  const m = model?.trim();
  if (!m) return "";
  return ALIASES.find((a) => a.value === m)?.value ?? "id";
}

/** The model to store for `choice`, with `id` the model ID typed for it. */
export function chosenModel(choice: ModelChoice, id?: string): string | undefined {
  if (choice === "id") return id?.trim() ? id : undefined;
  return choice || undefined;
}

/** Claude's effort levels, from the quickest to the most thorough. */
export const EFFORTS = [
  { value: "low", label: "Low" },
  { value: "medium", label: "Medium" },
  { value: "high", label: "High" },
  { value: "xhigh", label: "Extra high" },
  { value: "max", label: "Max" },
] as const;

export const CODEX_EFFORTS = [
  { value: "none", label: "None" },
  { value: "minimal", label: "Minimal" },
  ...EFFORTS,
] as const;

export const effortLevels = (agent?: G.AgentKind) => (agent === "codex" ? CODEX_EFFORTS : EFFORTS);

/**
 * A Claude model's ID (`claude-sonnet-5-5`): its family and version. A snapshot's ID, dated
 * (`claude-opus-4-1-20250805`), doesn't match: by its version alone, it would read as the
 * undated one.
 */
const MODEL_ID = /^claude-(opus|sonnet|haiku|fable)-(\d{1,3})(?:-(\d{1,2}))?$/;

/**
 * A model by its alias's name, by its family and version where it is named by a Claude model's
 * undated ID (`claude-sonnet-5-5` is Sonnet 5.5), or as it is; "" is Claude's default.
 */
export function modelLabel(model: string, agent?: G.AgentKind): string {
  if (agent === "codex") return model || "Codex's default";
  if (!model) return "Claude's default";
  const alias = ALIASES.find((a) => a.value === model);
  if (alias) return alias.label;
  const id = MODEL_ID.exec(model);
  if (!id) return model;
  const [, family = "", major, minor] = id;
  const name = ALIASES.find((a) => a.value === family)?.label ?? family;
  return `${name} ${major}${minor ? `.${minor}` : ""}`;
}

/** An effort level by its name; "" is Claude's default. */
export const effortLabel = (effort: string, agent?: G.AgentKind) =>
  effort ? (effortLevels(agent).find((e) => e.value === effort)?.label ?? effort) : modelLabel("", agent);

/**
 * The model and effort level of a session about to start ("" is Claude's default), and what its
 * agent may do on GitHub.
 */
export interface Choice {
  agent_kind?: G.AgentKind;
  model: string;
  effort: string;
  github: G.GithubAccess;
  /**
   * Its agent reaches GitHub's issues whatever the project's tracker says (`offersIssues`), where
   * the form offers it.
   */
  issues?: boolean;
}

type Project = Pick<G.Project, "settings" | "last_model" | "last_effort">;

/** What the project's sessions run with unless they ask for something else. */
export function projectChoice(p: Project, agent = p.settings.agent_kind): Choice {
  return {
    ...(agent ? { agent_kind: agent } : {}),
    model: (agent === "codex" ? p.settings.codex_model : p.settings.model)?.trim() ?? "",
    effort: (agent === "codex" ? p.settings.codex_effort : p.settings.effort)?.trim() ?? "",
    github: projectGithub(p),
  };
}

/**
 * What the composer shows in project `p`: what was picked in it (`picked`), else what you asked
 * for when you last started a session there, else the project's. A session can ask for a model
 * or an effort level but not for Claude's default, which stands only where the project has it.
 * Its agent does on GitHub what was picked, as far as the project allows it, else the most the
 * project allows: each session asks anew.
 */
export function startChoice(p: Project, picked: Partial<Choice> = {}): Choice {
  const own = projectChoice(p, picked.agent_kind ?? p.settings.agent_kind);
  const sameAgent = (own.agent_kind ?? "claude") === (picked.agent_kind ?? "claude");
  const last = own.agent_kind !== "codex" && p.settings.agent_kind !== "codex";
  const one = (pick: string | undefined, last: string | undefined, theirs: string) =>
    pick !== undefined && (pick || !theirs) ? pick : last?.trim() || theirs;
  return {
    ...own,
    model: one(sameAgent ? picked.model : undefined, last ? p.last_model : undefined, own.model),
    effort: one(sameAgent ? picked.effort : undefined, last ? p.last_effort : undefined, own.effort),
    github: picked.github && within(picked.github, own.github) ? picked.github : own.github,
  };
}

/**
 * What a session started with `c` in `p` asks for: what differs from the project's settings,
 * so that a session follows them where you took them (also when it resumes after they change),
 * and the next one starts on them again.
 */
export function asked(
  p: Project,
  c: Choice,
): Pick<G.CreateSessionRequest, "agent_kind" | "model" | "effort" | "github_access" | "github_issues"> {
  const own = projectChoice(p, c.agent_kind ?? p.settings.agent_kind);
  const agent_kind =
    c.agent_kind && c.agent_kind !== (p.settings.agent_kind ?? "claude") ? c.agent_kind : undefined;
  return {
    ...(agent_kind ? { agent_kind } : {}),
    model: c.model && c.model !== own.model ? c.model : undefined,
    effort: c.effort && c.effort !== own.effort ? c.effort : undefined,
    github_access: c.github !== own.github ? c.github : undefined,
    github_issues: c.issues || undefined,
  };
}

/** A model or an effort level the composer offers: `own` is the project's. */
export interface Offer {
  value: string;
  label: string;
  own: boolean;
}

/**
 * The models the composer offers in `p`: Claude's default where the project has it, the
 * aliases, then the models it names by their IDs (the project's, yours, `shown`).
 */
export function modelOffers(p: Project, shown: string, agent = p.settings.agent_kind): Offer[] {
  const own = projectChoice(p, agent).model;
  const last = agent === "codex" || p.settings.agent_kind === "codex" ? "" : (p.last_model?.trim() ?? "");
  const ids = [own, last, shown].filter(
    (m, i, all) => m && (agent === "codex" || modelChoice(m) === "id") && all.indexOf(m) === i,
  );
  const aliases = agent === "codex" ? [] : ALIASES.map((a) => a.value);
  return [...(own ? [] : [""]), ...aliases, ...ids].map((value) => ({
    value,
    label: modelLabel(value, agent),
    own: value === own,
  }));
}

/** The effort levels the composer offers in `p`: Claude's default where the project has it, the levels, and `shown`. */
export function effortOffers(p: Project, shown: string, agent = p.settings.agent_kind): Offer[] {
  const own = projectChoice(p, agent).effort;
  const levels: string[] = effortLevels(agent).map((e) => e.value);
  const other = [own, shown].filter((e, i, all) => e && !levels.includes(e) && all.indexOf(e) === i);
  return [...(own ? [] : [""]), ...levels, ...other].map((value) => ({
    value,
    label: effortLabel(value, agent),
    own: value === own,
  }));
}

/** A model or an effort level a session's owner can change a session to while it runs. */
export interface Switch {
  value: string;
  label: string;
}

/**
 * The models a session's owner can switch it to: the one it works on (by the alias it was asked
 * for, by its ID on Claude's default once Claude has named it, "" before then), then Claude
 * Code's aliases.
 */
export function switchModels(current: string): Switch[] {
  const aliases: string[] = ALIASES.map((a) => a.value);
  return [...(aliases.includes(current) ? [] : [current]), ...aliases].map((value) => ({
    value,
    label: modelLabel(value),
  }));
}

/** The effort levels a session's owner can change it to: the one it works at, then the levels. */
export function switchEfforts(current: string): Switch[] {
  const levels: string[] = EFFORTS.map((e) => e.value);
  return [...(levels.includes(current) ? [] : [current]), ...levels].map((value) => ({
    value,
    label: effortLabel(value),
  }));
}

/**
 * Whether a change makes Claude read the session's conversation again without its prompt cache: a
 * new model always does, as the cache belongs to the model it was written for; a new effort level
 * only where Claude sends it for the whole conversation rather than with each turn (Claude Code
 * says which, `perTurnEffort`, and when it doesn't, it keeps the cache on the models that take it).
 */
export function costsCache(change: "model" | "effort", perTurnEffort?: boolean): boolean {
  return change === "model" || perTurnEffort === false;
}
