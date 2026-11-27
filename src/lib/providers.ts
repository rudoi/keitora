// The agent providers Keitora can run in a worktree, and the static facts the UI needs
// about each. Every place that lists or names providers goes through here, so adding a
// provider is a type error at each spot that needs a decision.

import { CLI_KINDS, type CliKind } from "./ipc";

export { CLI_KINDS };

/** `s` as a provider, or null when it names none. */
export function parseCliKind(s: string | null | undefined): CliKind | null {
  return CLI_KINDS.find((c) => c === s) ?? null;
}

/** A record with one entry per provider, each `f(cli)`. */
export function byCli<T>(f: (cli: CliKind) => T): Record<CliKind, T> {
  return Object.fromEntries(CLI_KINDS.map((c) => [c, f(c)])) as Record<CliKind, T>;
}

/** Availability before the first fetch: Claude is never unavailable (its launch falls back to
 *  the bare `claude`), every other provider is off until discovery says otherwise. */
export function initialAvailability(): Record<CliKind, boolean> {
  return byCli((cli) => cli === "claude");
}

/** `fetched` with Claude forced available, whatever the backend reported. */
export function withClaudeAvailable(fetched: Record<CliKind, boolean>): Record<CliKind, boolean> {
  return { ...fetched, claude: true };
}

const TITLES: Record<CliKind, string> = {
  claude: "Claude",
  codex: "Codex",
  antigravity: "Antigravity",
};

const BINARIES: Record<CliKind, string> = {
  claude: "claude",
  codex: "codex",
  antigravity: "agy",
};

/** Display name, for sentence-initial copy: "Claude" / "Codex" / "Antigravity". */
export function cliTitle(cli: CliKind): string {
  return TITLES[cli];
}

/** Executable name: "claude" / "codex" / "agy". */
export function cliBinary(cli: CliKind): string {
  return BINARIES[cli];
}

/** The only definition of the pairs the backend can transfer a conversation between.
 *  Keep in sync with `is_transfer_pair` in src-tauri/src/transfer.rs. */
export const TRANSFER_PAIRS: readonly (readonly [from: CliKind, to: CliKind])[] = [
  ["claude", "codex"],
  ["codex", "claude"],
];

/** The providers a `from` session can be transferred to (conversation carried over). */
export function transferTargets(from: CliKind): CliKind[] {
  return TRANSFER_PAIRS.filter(([f]) => f === from).map(([, to]) => to);
}

/** The providers a `from` worktree can only be switched to (conversation not carried over):
 *  every other provider that is not a transfer target. */
export function switchTargets(from: CliKind): CliKind[] {
  const transfers = transferTargets(from);
  return CLI_KINDS.filter((c) => c !== from && !transfers.includes(c));
}
