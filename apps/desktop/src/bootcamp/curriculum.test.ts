import { describe, expect, it } from "vitest";
import { BADGES, CHAPTERS, LESSONS } from "./curriculum";
import { judge, labExecute, MOCK, setupSteps, type CallFn } from "./labRunner";
import type { Lab } from "./types";

describe("curriculum", () => {
  it("has unique lessons, valid questions and known badges", () => {
    const ids = CHAPTERS.flatMap((c) => c.lessons.map((l) => l.id));
    expect(new Set(ids).size).toBe(ids.length);
    expect(LESSONS.size).toBe(ids.length);
    for (const ch of CHAPTERS) {
      expect(BADGES.some((b) => b.id === ch.badge), ch.id).toBe(true);
      for (const l of ch.lessons) {
        expect(l.steps.length, l.id).toBeGreaterThan(2);
        for (const s of l.steps) {
          if (s.kind !== "quiz") continue;
          const q = s.q;
          const where = `${l.id}: ${q.prompt}`;
          if (q.type === "choice") expect(q.answer, where).toBeLessThan(q.options.length);
          if (q.type === "multi") expect(q.answers.every((a) => a < q.options.length) && q.answers.length > 0, where).toBe(true);
          if (q.type === "blank") expect(q.prompt.split("___").length, where).toBe(2);
          if (q.type === "match") expect(new Set(q.pairs.map((p) => p[1])).size, where).toBe(q.pairs.length);
          if (q.type === "order") expect(new Set(q.items).size, where).toBe(q.items.length);
        }
      }
    }
  });
});

// Every lab's solution must pass and its starter must not. Needs a running core with its dev API:
// TWINBASE_TEST_CORE="http://127.0.0.1:47811|<token>" (use an isolated TWINBASE_HOME).
const core = (globalThis as { process?: { env: Record<string, string | undefined> } }).process?.env.TWINBASE_TEST_CORE;
const labs: { id: string; lab: Lab }[] = CHAPTERS.flatMap((c) => c.lessons.flatMap((l) => l.steps.flatMap((s) => (s.kind === "lab" ? [{ id: l.id, lab: s.lab }] : []))));

describe.skipIf(!core)("labs on a live core", () => {
  const [base, token] = (core ?? "|").split("|");
  const call: CallFn = async <T,>(method: string, params: object = {}) => {
    const res = await fetch(`${base}/api/${method}`, { method: "POST", headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}`, Origin: "http://127.0.0.1:5173" }, body: JSON.stringify(params) });
    const body = (await res.json()) as { ok: boolean; result?: T; error?: { message: string } };
    if (!body.ok) throw new Error(body.error?.message ?? "error");
    return body.result as T;
  };
  const run = async (lab: Lab, script: string) => {
    for (const s of setupSteps(lab)) await labExecute(call, lab.engine, s);
    try {
      return (await labExecute(call, lab.engine, script)).texts;
    } catch {
      return null;
    }
  };

  it.each(labs)("$id", async ({ lab }) => {
    await call("bootcamp.open");
    await call("mock.start", { id: MOCK[lab.engine] }).catch(() => undefined);
    const solved = await run(lab, lab.solution);
    expect(solved, "solution runs").not.toBeNull();
    expect(await judge(call, lab, solved!), "solution passes").toMatchObject({ ok: true });
    const started = await run(lab, lab.starter);
    const verdict = started === null ? { ok: false } : await judge(call, lab, started);
    expect(verdict.ok, "starter alone must not pass").toBe(false);
  }, 30_000);
});
