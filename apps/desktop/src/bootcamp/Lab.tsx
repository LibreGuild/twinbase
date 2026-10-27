// A hands-on lab: the learner's statements run on one of the Bootcamp's mock servers, then the
// result (or the resulting state) is compared with the reference solution.

import { Beaker, Check, Eye, FlaskConical, Lightbulb, Play, RotateCcw, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { Markdown } from "../components/Markdown";
import { SqlEditor, type SqlEditorHandle } from "../components/SqlEditor";
import { Spinner } from "../components/ui";
import { call, errorMessage } from "../lib/api";
import { useStore } from "../lib/store";
import { cellText, judge, labExecute, MOCK, setupSteps, type Output } from "./labRunner";
import type { Lab, LabEngine } from "./types";

async function ensureRunning(engine: LabEngine) {
  const id = MOCK[engine];
  const ws = useStore.getState().workspace;
  const m = ws?.mocks.find((x) => x.id === id);
  if (!m) throw new Error("The Bootcamp lab servers are missing. Reopen the Bootcamp from the rail.");
  if (!m.running) await call("mock.start", { id });
}

const execute = (engine: LabEngine, script: string) => labExecute(call, engine, script);

export function LabCard({ lab, solved, onSolved }: { lab: Lab; solved: boolean; onSolved: (usedHelp: boolean) => void }) {
  const [code, setCode] = useState(lab.starter);
  const [busy, setBusy] = useState<string | null>("Preparing the lab server…");
  const [output, setOutput] = useState<Output | null>(null);
  const [verdict, setVerdict] = useState<{ ok: boolean; text: string } | null>(solved ? { ok: true, text: "Solved." } : null);
  const [hint, setHint] = useState(false);
  const [showSolution, setShowSolution] = useState(false);
  const [fails, setFails] = useState(0);
  const lastTexts = useRef<string[] | null>(null);
  const editor = useRef<SqlEditorHandle>(null);

  const reset = useCallback(async () => {
    setBusy("Resetting the lab data…");
    setOutput(null);
    lastTexts.current = null;
    try {
      await ensureRunning(lab.engine);
      for (const s of setupSteps(lab)) await execute(lab.engine, s);
    } catch (e) {
      setOutput({ kind: "error", text: errorMessage(e) });
    }
    setBusy(null);
  }, [lab]);

  useEffect(() => {
    void reset();
  }, [reset]);

  const run = async () => {
    setBusy("Running…");
    setVerdict(solved ? { ok: true, text: "Solved." } : null);
    try {
      const r = await execute(lab.engine, code);
      setOutput(r.output);
      lastTexts.current = r.texts;
    } catch (e) {
      setOutput({ kind: "error", text: errorMessage(e) });
      lastTexts.current = null;
    }
    setBusy(null);
  };

  const check = async () => {
    setBusy("Checking…");
    try {
      if (lab.check.kind === "result" && !lastTexts.current) {
        const r = await execute(lab.engine, code);
        setOutput(r.output);
        lastTexts.current = r.texts;
      }
      const { ok, text } = await judge(call, lab, lastTexts.current ?? []);
      setVerdict({ ok, text });
      if (ok) onSolved(hint || showSolution);
      else setFails((f) => f + 1);
    } catch (e) {
      setVerdict({ ok: false, text: errorMessage(e) });
      setFails((f) => f + 1);
    }
    setBusy(null);
  };

  return (
    <div className="lab">
      <div className="lab-head">
        <span className="lab-icon">
          <FlaskConical size={16} />
        </span>
        <div className="grow">
          <div className="lab-kicker">Lab · {lab.engine === "postgres" ? "PostgreSQL" : lab.engine === "redis" ? "Redis" : "MongoDB"} mock server</div>
          <div className="lab-title">{lab.title}</div>
        </div>
        <button className="btn sm ghost" onClick={reset} disabled={!!busy} title="Restore the lab's starting data">
          <RotateCcw size={13} /> Reset
        </button>
      </div>
      <Markdown source={lab.task} className="lab-task" />
      <div className="lab-editor">
        {lab.engine === "postgres" ? (
          <SqlEditor ref={editor} value={code} kind="postgres" onChange={setCode} onRun={run} onRunCurrent={run} minHeight={120} />
        ) : (
          <textarea
            className="lab-textarea mono"
            value={code}
            spellCheck={false}
            onChange={(e) => setCode(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                void run();
              }
            }}
            rows={Math.max(4, code.split("\n").length + 1)}
            aria-label="Lab commands"
          />
        )}
      </div>
      <div className="row wrap" style={{ marginTop: 10 }}>
        <button className="btn" onClick={run} disabled={!!busy}>
          <Play size={13} /> Run <kbd className="kbd">⌘↵</kbd>
        </button>
        <button className="btn primary" onClick={check} disabled={!!busy}>
          <Beaker size={13} /> Check
        </button>
        {busy && (
          <span className="faint small row" style={{ gap: 6 }}>
            <Spinner size={12} /> {busy}
          </span>
        )}
        <span className="grow" />
        <button className="btn sm ghost" onClick={() => setHint(true)} disabled={hint}>
          <Lightbulb size={13} /> Hint
        </button>
        {(fails >= 2 || showSolution) && (
          <button className="btn sm ghost" onClick={() => (setShowSolution(true), setCode(lab.solution), editor.current?.setValue(lab.solution))}>
            <Eye size={13} /> Show solution
          </button>
        )}
      </div>
      {hint && <Markdown source={`💡 ${lab.hint}`} className="note-box lab-hint" />}
      {verdict && (
        <div className={`lab-verdict ${verdict.ok ? "ok" : "bad"}`}>
          {verdict.ok ? <Check size={15} /> : <X size={15} />} {verdict.text}
        </div>
      )}
      {output && (
        <div className="lab-output">
          {output.kind === "table" ? (
            <div className="lab-table-wrap">
              <table className="bw-table">
                <thead>
                  <tr>
                    {output.result.columns.map((c, i) => (
                      <th key={i}>{c.name}</th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {output.result.rows.slice(0, 50).map((r, i) => (
                    <tr key={i}>
                      {r.map((c, j) => (
                        <td key={j} className={c === null ? "null" : ""}>
                          {cellText(c)}
                        </td>
                      ))}
                    </tr>
                  ))}
                </tbody>
              </table>
              <div className="faint xs">{output.result.rows.length} rows</div>
            </div>
          ) : (
            <pre className={output.kind === "error" ? "err" : ""}>{output.text}</pre>
          )}
        </div>
      )}
    </div>
  );
}
