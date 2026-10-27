// A lesson: one card at a time (read, quiz, lab), XP as you go, and a finish screen with badges.

import { ArrowLeft, ArrowRight, BookOpen, CheckCircle2, Clock, FlaskConical, HelpCircle, Sparkles, Trophy, Zap } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { Markdown } from "../components/Markdown";
import { Empty } from "../components/ui";
import { useStore } from "../lib/store";
import { BADGES, LESSONS } from "./curriculum";
import { LabCard } from "./Lab";
import { levelOf, useProgress, XP } from "./progress";
import { QuizCard, type QuizResult } from "./Quiz";
import { Widget } from "./widgets";

interface StepState {
  /** Quiz: answered correctly; first try or not. Lab: solved; with help or not. */
  done: boolean;
  clean: boolean;
  attempts: number;
}

export function LessonPlayer({ lessonId, tabId }: { lessonId: string; tabId: string }) {
  const entry = LESSONS.get(lessonId);
  const openTab = useStore((s) => s.openTab);
  const closeTab = useStore((s) => s.closeTab);
  const progress = useProgress((s) => s.progress);
  const loaded = useProgress((s) => s.loaded);
  const load = useProgress((s) => s.load);
  const complete = useProgress((s) => s.complete);
  const [index, setIndex] = useState(0);
  const [states, setStates] = useState<Record<number, StepState>>({});
  const [finished, setFinished] = useState<{ xp: number; perfect: boolean; badges: string[]; levelUp: boolean } | null>(null);
  const [gain, setGain] = useState<{ n: number; key: number } | null>(null);
  const body = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!loaded) void load();
  }, [loaded, load]);
  useEffect(() => {
    body.current?.scrollTo({ top: 0 });
  }, [index]);

  const xpSoFar = useMemo(() => {
    if (!entry) return 0;
    return entry.lesson.steps.reduce((sum, step, i) => {
      const st = states[i];
      if (!st?.done) return sum;
      if (step.kind === "quiz") return sum + (st.clean ? XP.quizFirstTry : XP.quizRetry);
      // A skipped lab (no attempt) earns nothing.
      if (step.kind === "lab") return sum + (st.attempts === 0 ? 0 : st.clean ? XP.lab : XP.labWithHint);
      return sum;
    }, 0);
  }, [entry, states]);

  if (!entry) return <Empty title="Lesson not found" />;
  const { lesson, chapter } = entry;
  const step = lesson.steps[index];
  const st = states[index];
  const isLast = index === lesson.steps.length - 1;
  const blocked = (step.kind === "quiz" || step.kind === "lab") && !st?.done;

  const bump = (n: number) => setGain({ n, key: Date.now() });

  const onQuiz = (r: QuizResult) => {
    setStates((s) => {
      const prev = s[index] ?? { done: false, clean: true, attempts: 0 };
      if (prev.done) return s;
      const attempts = prev.attempts + 1;
      if (r === "correct") bump(attempts === 1 ? XP.quizFirstTry : XP.quizRetry);
      return { ...s, [index]: { done: r === "correct", clean: attempts === 1 && r === "correct", attempts } };
    });
  };
  const onLab = (usedHelp: boolean) => {
    setStates((s) => {
      if (s[index]?.done) return s;
      bump(usedHelp ? XP.labWithHint : XP.lab);
      return { ...s, [index]: { done: true, clean: !usedHelp, attempts: 1 } };
    });
  };

  const finish = () => {
    const graded = lesson.steps.map((s, i) => ({ s, st: states[i] })).filter(({ s }) => s.kind !== "read");
    const perfect = graded.length > 0 && graded.every(({ st }) => st?.done && st.clean);
    const labs = graded.filter(({ s, st }) => s.kind === "lab" && st?.done && st.attempts > 0).length;
    const total = xpSoFar + XP.lessonDone + (perfect ? XP.perfectBonus : 0);
    const before = levelOf(progress.xp).level;
    const badges = complete(lesson.id, total, perfect, labs);
    const after = levelOf(useProgress.getState().progress.xp).level;
    setFinished({ xp: total, perfect, badges, levelUp: after > before });
  };

  const nextId = (() => {
    const ids = [...LESSONS.keys()];
    return ids[ids.indexOf(lesson.id) + 1];
  })();

  if (finished) {
    const lvl = levelOf(progress.xp);
    return (
      <div className="pane bc-finish">
        <div className="bc-finish-card">
          <div className="bc-burst" aria-hidden>
            {Array.from({ length: 18 }, (_, i) => (
              <span key={i} style={{ ["--i" as string]: i }} />
            ))}
          </div>
          <Trophy size={40} className="bc-trophy" />
          <h1>Lesson complete!</h1>
          <p className="muted">{lesson.title}</p>
          <div className="bc-finish-stats">
            <div>
              <b>+{finished.xp}</b>
              <span>XP earned</span>
            </div>
            <div>
              <b>{finished.perfect ? "100%" : "✓"}</b>
              <span>{finished.perfect ? "perfect lesson" : "completed"}</span>
            </div>
            <div>
              <b>Lv {lvl.level}</b>
              <span>{lvl.title}</span>
            </div>
          </div>
          {finished.levelUp && (
            <div className="bc-levelup">
              <Sparkles size={16} /> Level up! You are now a <strong>{lvl.title}</strong>.
            </div>
          )}
          {finished.badges.length > 0 && (
            <div className="bc-new-badges">
              {finished.badges.map((id) => {
                const b = BADGES.find((x) => x.id === id)!;
                return (
                  <div key={id} className="bc-new-badge">
                    <img src={b.image} alt="" />
                    <div>
                      <div className="faint xs">Badge unlocked</div>
                      <strong>{b.name}</strong>
                      <div className="faint small">{b.description}</div>
                    </div>
                  </div>
                );
              })}
            </div>
          )}
          <div className="row" style={{ justifyContent: "center", marginTop: 18 }}>
            <button className="btn" onClick={() => (closeTab(tabId), openTab({ kind: "bootcamp" }))}>
              Back to the Bootcamp
            </button>
            {nextId && (
              <button className="btn primary" onClick={() => (closeTab(tabId), openTab({ kind: "lesson", lessonId: nextId }))}>
                Next lesson <ArrowRight size={14} />
              </button>
            )}
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="bc-player">
      <div className="bc-player-top">
        <button className="icon-btn" onClick={() => openTab({ kind: "bootcamp" })} title="Back to the Bootcamp" aria-label="Back to the Bootcamp">
          <ArrowLeft size={16} />
        </button>
        <div className="bc-steps" role="progressbar" aria-valuenow={index + 1} aria-valuemax={lesson.steps.length}>
          {lesson.steps.map((s, i) => (
            <button
              key={i}
              className={`bc-step-dot k-${s.kind}${i === index ? " now" : ""}${states[i]?.done || (s.kind === "read" && i < index) ? " done" : ""}`}
              onClick={() => i <= index && setIndex(i)}
              title={s.kind === "read" ? s.title : s.kind === "quiz" ? "Quiz" : s.lab.title}
              aria-label={`Step ${i + 1}`}
            />
          ))}
        </div>
        <span className="bc-xp-pill">
          <Zap size={13} /> {xpSoFar} XP
          {gain && (
            <span key={gain.key} className="bc-xp-gain">
              +{gain.n}
            </span>
          )}
        </span>
      </div>
      <div className="bc-player-body" ref={body}>
        <div className="bc-card" key={index}>
          <div className="bc-kicker" style={{ color: chapter.color }}>
            {step.kind === "read" ? <BookOpen size={13} /> : step.kind === "quiz" ? <HelpCircle size={13} /> : <FlaskConical size={13} />}
            {chapter.title} · {lesson.title}
            <span className="faint">
              {" "}
              · <Clock size={11} style={{ verticalAlign: -1 }} /> {lesson.minutes} min
            </span>
          </div>
          {step.kind === "read" && (
            <>
              <h2 className="bc-title">{step.title}</h2>
              <Markdown source={step.body} className="bc-read" />
              {step.widget && (
                <div className="bc-widget">
                  <Widget id={step.widget} />
                </div>
              )}
            </>
          )}
          {step.kind === "quiz" && (
            <>
              <h2 className="bc-title">Quick check</h2>
              <QuizCard key={index} q={step.q} onResult={onQuiz} answered={!!st?.done} />
            </>
          )}
          {step.kind === "lab" && <LabCard key={index} lab={step.lab} solved={!!st?.done} onSolved={onLab} />}
        </div>
      </div>
      <div className="bc-player-foot">
        <button className="btn ghost" disabled={index === 0} onClick={() => setIndex(index - 1)}>
          <ArrowLeft size={14} /> Back
        </button>
        <span className="grow" />
        {step.kind === "lab" && !st?.done && (
          <button className="btn ghost" onClick={() => (setStates((s) => ({ ...s, [index]: { done: true, clean: false, attempts: 0 } })), isLast ? finish() : setIndex(index + 1))} title="Continue without the lab's XP">
            Skip lab
          </button>
        )}
        {isLast ? (
          <button className="btn primary lg" disabled={blocked} onClick={finish}>
            <CheckCircle2 size={15} /> Finish lesson
          </button>
        ) : (
          <button className="btn primary lg" disabled={blocked} onClick={() => setIndex(index + 1)}>
            Continue <ArrowRight size={15} />
          </button>
        )}
      </div>
    </div>
  );
}
