// Running lab scripts on the Bootcamp mock servers and judging the outcome. Shared by the lab UI and
// the curriculum test that runs every lab against a real core.

import type { Cell, QueryResult } from "../lib/types";
import type { Lab, LabEngine } from "./types";

export type CallFn = <T = unknown>(method: string, params?: object) => Promise<T>;

export const MOCK: Record<LabEngine, string> = { postgres: "lab-postgres", redis: "lab-redis", mongodb: "lab-mongodb" };

export type Output = { kind: "table"; result: QueryResult } | { kind: "text"; text: string } | { kind: "error"; text: string };

export function cellText(c: Cell | unknown): string {
  if (c === null || c === undefined) return "NULL";
  if (typeof c === "boolean") return c ? "t" : "f";
  if (typeof c === "object") return JSON.stringify(c);
  return String(c);
}

/** Cells equal as text, or as numbers when both parse (64.80 = 64.8). */
function same(a: string, b: string): boolean {
  if (a === b) return true;
  const x = Number(a);
  const y = Number(b);
  return a.trim() !== "" && b.trim() !== "" && !Number.isNaN(x) && !Number.isNaN(y) && Math.abs(x - y) < 1e-6 * Math.max(1, Math.abs(x));
}

export function matchesExpect(actual: string, expect: string): boolean {
  if (expect.length > 2 && expect.startsWith("/") && expect.endsWith("/")) return new RegExp(expect.slice(1, -1)).test(actual);
  const a = actual.split("|");
  const e = expect.split("|");
  return a.length === e.length && a.every((v, i) => same(v, e[i]));
}

function replyText(v: unknown): string {
  if (v === null || v === undefined) return "(nil)";
  if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") return String(v);
  return JSON.stringify(v);
}

/** Run a script on the lab server; returns what to show and the text used for checks. */
export async function labExecute(call: CallFn, engine: LabEngine, script: string): Promise<{ output: Output; texts: string[] }> {
  const connection = `mock:${MOCK[engine]}`;
  if (engine === "postgres") {
    const results = await call<QueryResult[]>("db.query", { connection, sql: script, max_rows: 200 });
    const last = results[results.length - 1];
    if (!last) return { output: { kind: "text", text: "No statements." }, texts: [] };
    const texts = last.rows.map((r) => r.map(cellText).join("|"));
    return { output: last.columns.length ? { kind: "table", result: last } : { kind: "text", text: last.message ?? `${last.affected ?? 0} rows affected` }, texts };
  }
  if (engine === "redis") {
    const lines = script.split("\n").map((l) => l.trim()).filter((l) => l && !l.startsWith("#"));
    const outs: string[] = [];
    const log: string[] = [];
    for (const line of lines) {
      const r = await call("redis.command", { connection, command: line });
      outs.push(replyText(r));
      log.push(`> ${line}\n${replyText(r)}`);
    }
    return { output: { kind: "text", text: log.join("\n") }, texts: [outs.join("|")] };
  }
  const r = await call<{ outputs: { statement: string; value?: unknown; documents?: unknown[]; message?: string; error?: { message: string } }[] }>("mongo.run", {
    connection,
    database: "lab",
    command: script,
    max_documents: 50,
  });
  const err = r.outputs.find((o) => o.error);
  if (err) throw new Error(err.error!.message);
  const outs = r.outputs.map((o) => (o.value !== undefined ? replyText(o.value) : o.documents ? JSON.stringify(o.documents) : (o.message ?? "")));
  const log = r.outputs.map((o, i) => `> ${o.statement}\n${o.documents ? JSON.stringify(o.documents, null, 2) : outs[i]}`);
  return { output: { kind: "text", text: log.join("\n") }, texts: [outs.join("|")] };
}


/** Judge an attempt: compare the user's last result with the solution's, or the resulting state. */
export async function judge(call: CallFn, lab: Lab, userTexts: string[]): Promise<{ ok: boolean; text: string }> {
  if (lab.check.kind === "result") {
    const expected = (await labExecute(call, lab.engine, lab.solution)).texts;
    const ordered = /order\s+by/i.test(lab.solution);
    const a = ordered ? userTexts : [...userTexts].sort();
    const b = ordered ? expected : [...expected].sort();
    const ok = a.length === b.length && a.every((row, i) => matchesExpect(row, b[i]));
    return {
      ok,
      text: ok
        ? "Your result matches."
        : userTexts.length !== expected.length
          ? `Expected ${expected.length} row${expected.length === 1 ? "" : "s"}, got ${userTexts.length}.`
          : "Same number of rows, but different values or order.",
    };
  }
  const r = await labExecute(call, lab.engine, lab.check.query);
  const ok = matchesExpect(r.texts[0] ?? "", lab.check.expect);
  return { ok, text: ok ? "The data is exactly as it should be." : "Not there yet — run your statements, then check again (Reset starts over)." };
}

/** The setup script as the separate calls it is run in. */
export function setupSteps(lab: Lab): string[] {
  return lab.engine === "redis" ? lab.setup.split("\n").filter(Boolean) : [lab.setup];
}
