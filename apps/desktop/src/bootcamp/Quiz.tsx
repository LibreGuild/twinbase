// One quiz question of any type, with instant feedback. Wrong answers can be retried; the player
// gives full XP only for a first-try answer.

import { ArrowDown, ArrowUp, Check, RotateCcw, X } from "lucide-react";
import { useMemo, useState } from "react";
import { Markdown } from "../components/Markdown";
import type { Question } from "./types";

/** Deterministic shuffle so a question looks the same when revisited. */
function shuffled<T>(items: T[], seed: string): T[] {
  let h = 2166136261;
  for (let i = 0; i < seed.length; i++) h = Math.imul(h ^ seed.charCodeAt(i), 16777619);
  const out = items.map((v, i) => ({ v, k: Math.imul(h ^ (i + 1), 2654435761) >>> 0 }));
  out.sort((a, b) => a.k - b.k);
  const res = out.map((o) => o.v);
  // Never show an ordering question already solved.
  if (res.length > 1 && res.every((v, i) => v === items[i])) res.push(res.shift()!);
  return res;
}

const norm = (s: string) => s.trim().toLowerCase().replace(/\s+/g, " ").replace(/[.;]$/, "");

export type QuizResult = "correct" | "wrong";

export function QuizCard({ q, onResult, answered }: { q: Question; onResult: (r: QuizResult) => void; answered: boolean }) {
  const [state, setState] = useState<"open" | "correct" | "wrong">("open");
  const [choice, setChoice] = useState<number | null>(null);
  const [multi, setMulti] = useState<Set<number>>(new Set());
  const [blank, setBlank] = useState("");
  const rights = useMemo(() => (q.type === "match" ? shuffled(q.pairs.map((p) => p[1]), q.prompt) : []), [q]);
  const [matchSel, setMatchSel] = useState<{ left?: number }>({ left: 0 });
  const [matches, setMatches] = useState<Record<number, number>>({});
  const [order, setOrder] = useState<string[]>(() => (q.type === "order" ? shuffled(q.items, q.prompt) : []));

  const done = state === "correct" || answered;
  const submit = (ok: boolean) => {
    setState(ok ? "correct" : "wrong");
    onResult(ok ? "correct" : "wrong");
  };
  const retry = () => {
    setState("open");
    setChoice(null);
    setMulti(new Set());
    setBlank("");
    setMatches({});
    setMatchSel({ left: 0 });
  };

  let body: React.ReactNode = null;
  let canSubmit = false;
  let check = () => submit(false);
  switch (q.type) {
    case "choice":
    case "truefalse": {
      const options = q.type === "truefalse" ? ["True", "False"] : q.options;
      const correct = q.type === "truefalse" ? (q.answer ? 0 : 1) : q.answer;
      body = (
        <div className="qz-options">
          {options.map((o, i) => (
            <button
              key={i}
              className={`qz-option${choice === i ? " sel" : ""}${done && i === correct ? " right" : ""}${state === "wrong" && choice === i ? " wrong" : ""}`}
              disabled={done}
              onClick={() => setChoice(i)}
            >
              <span className="qz-letter">{q.type === "truefalse" ? (i === 0 ? "T" : "F") : String.fromCharCode(65 + i)}</span>
              <Markdown source={o} className="qz-text" />
            </button>
          ))}
        </div>
      );
      canSubmit = choice !== null;
      check = () => submit(choice === correct);
      break;
    }
    case "multi":
      body = (
        <div className="qz-options">
          {q.options.map((o, i) => (
            <button
              key={i}
              className={`qz-option${multi.has(i) ? " sel" : ""}${done && q.answers.includes(i) ? " right" : ""}${state === "wrong" && multi.has(i) && !q.answers.includes(i) ? " wrong" : ""}`}
              disabled={done}
              onClick={() =>
                setMulti((m) => {
                  const n = new Set(m);
                  if (n.has(i)) n.delete(i);
                  else n.add(i);
                  return n;
                })
              }
            >
              <span className="qz-check">{multi.has(i) ? <Check size={12} /> : null}</span>
              <Markdown source={o} className="qz-text" />
            </button>
          ))}
          <div className="faint xs">Select every correct answer.</div>
        </div>
      );
      canSubmit = multi.size > 0;
      check = () => submit(multi.size === q.answers.length && q.answers.every((a) => multi.has(a)));
      break;
    case "blank": {
      const [before, after] = q.prompt.split("___");
      body = (
        <div className="qz-blank">
          <Markdown source={before} className="qz-inline" />
          <input
            className={`input mono qz-blank-input${state === "wrong" ? " wrong" : ""}${done ? " right" : ""}`}
            value={done && state !== "correct" ? q.answers[0] : blank}
            disabled={done}
            autoFocus
            onChange={(e) => setBlank(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && blank.trim() && check()}
            placeholder="type one word"
            aria-label="Answer"
          />
          <Markdown source={after ?? ""} className="qz-inline" />
        </div>
      );
      canSubmit = !!blank.trim();
      check = () => submit(q.answers.some((a) => norm(a) === norm(blank)));
      break;
    }
    case "match": {
      const allSet = Object.keys(matches).length === q.pairs.length;
      body = (
        <div className="qz-match">
          <div className="col">
            {q.pairs.map(([l], i) => (
              <button
                key={i}
                className={`qz-chip${matchSel.left === i ? " sel" : ""}${matches[i] !== undefined ? " paired" : ""}${state !== "open" && matches[i] !== undefined ? (rights[matches[i]] === q.pairs[i][1] ? " right" : " wrong") : ""}`}
                disabled={done}
                onClick={() => setMatchSel({ left: i })}
              >
                {matches[i] !== undefined && <span className="qz-pair-n">{matches[i] + 1}</span>}
                {l}
              </button>
            ))}
          </div>
          <div className="col">
            {rights.map((r, j) => {
              const owner = Object.entries(matches).find(([, v]) => v === j)?.[0];
              return (
                <button
                  key={j}
                  className={`qz-chip right-col${owner !== undefined ? " paired" : ""}`}
                  disabled={done || matchSel.left === undefined}
                  onClick={() => {
                    if (matchSel.left === undefined) return;
                    const next = { ...matches };
                    for (const [k, v] of Object.entries(next)) if (v === j) delete next[Number(k)];
                    next[matchSel.left] = j;
                    setMatches(next);
                    const free = q.pairs.findIndex((_, i) => next[i] === undefined);
                    setMatchSel(free >= 0 ? { left: free } : {});
                  }}
                >
                  <span className="qz-pair-n">{j + 1}</span>
                  {r}
                </button>
              );
            })}
          </div>
          <div className="faint xs" style={{ gridColumn: "1 / -1" }}>
            Pick an item on the left, then its partner on the right.
          </div>
        </div>
      );
      canSubmit = allSet;
      check = () => submit(q.pairs.every(([, r], i) => rights[matches[i]] === r));
      break;
    }
    case "order": {
      const move = (i: number, d: number) => {
        const n = [...order];
        const j = i + d;
        if (j < 0 || j >= n.length) return;
        [n[i], n[j]] = [n[j], n[i]];
        setOrder(n);
      };
      body = (
        <ol className="qz-order">
          {order.map((it, i) => (
            <li key={it} className={state !== "open" ? (q.items[i] === it ? "right" : "wrong") : ""}>
              <span className="qz-pos">{i + 1}</span>
              <span className="grow">{it}</span>
              <button className="icon-btn" disabled={done || i === 0} onClick={() => move(i, -1)} aria-label="Move up">
                <ArrowUp size={13} />
              </button>
              <button className="icon-btn" disabled={done || i === order.length - 1} onClick={() => move(i, 1)} aria-label="Move down">
                <ArrowDown size={13} />
              </button>
            </li>
          ))}
        </ol>
      );
      canSubmit = true;
      check = () => submit(order.every((it, i) => it === q.items[i]));
      break;
    }
  }

  return (
    <div className="qz">
      {q.type !== "blank" && <Markdown source={q.prompt} className="qz-prompt" />}
      {q.type === "blank" && <div className="qz-prompt faint small">Fill in the blank</div>}
      {body}
      {state === "open" && !answered && (
        <div className="row" style={{ marginTop: 14 }}>
          <span className="grow" />
          <button className="btn primary" disabled={!canSubmit} onClick={check}>
            Check answer
          </button>
        </div>
      )}
      {(state !== "open" || answered) && (
        <div className={`qz-feedback ${state === "wrong" ? "wrong" : "right"}`}>
          <div className="qz-fb-head">
            {state === "wrong" ? <X size={16} /> : <Check size={16} />}
            <strong>{state === "wrong" ? "Not quite." : "Correct!"}</strong>
            <span className="grow" />
            {state === "wrong" && (
              <button className="btn sm" onClick={retry}>
                <RotateCcw size={13} /> Try again
              </button>
            )}
          </div>
          <Markdown source={q.explain} />
        </div>
      )}
    </div>
  );
}
