// Bootcamp home: level and XP, streak, the chapter map, and the badge shelf.

import { ArrowRight, Award, BookOpen, Check, Flame, FlaskConical, GraduationCap, Lock, LogOut, Play, RotateCcw, Zap } from "lucide-react";
import { useEffect, useState } from "react";
import { ConfirmDialog, Spinner } from "../components/ui";
import { attempt, switchWorkspace, useStore } from "../lib/store";
import { BADGES, CHAPTERS, LESSONS } from "./curriculum";
import { chapterDone, isUnlocked, levelOf, nextLesson, useProgress } from "./progress";

function Ring({ value, size = 92, stroke = 9, color = "var(--accent)" }: { value: number; size?: number; stroke?: number; color?: string }) {
  const r = (size - stroke) / 2;
  const c = 2 * Math.PI * r;
  return (
    <svg width={size} height={size} viewBox={`0 0 ${size} ${size}`} className="bc-ring" aria-hidden>
      <circle cx={size / 2} cy={size / 2} r={r} strokeWidth={stroke} className="track" />
      <circle cx={size / 2} cy={size / 2} r={r} strokeWidth={stroke} stroke={color} strokeDasharray={c} strokeDashoffset={c * (1 - value)} strokeLinecap="round" className="fill" transform={`rotate(-90 ${size / 2} ${size / 2})`} />
    </svg>
  );
}

export function BootcampHome() {
  const progress = useProgress((s) => s.progress);
  const loaded = useProgress((s) => s.loaded);
  const load = useProgress((s) => s.load);
  const reset = useProgress((s) => s.reset);
  const openTab = useStore((s) => s.openTab);
  const settings = useStore((s) => s.settings);
  const bootcamp = useStore((s) => s.bootcamp);
  const [confirmReset, setConfirmReset] = useState(false);
  useEffect(() => {
    void load();
  }, [load]);
  if (!loaded) return <div className="pane"><div className="page"><Spinner /></div></div>;

  const lvl = levelOf(progress.xp);
  const next = nextLesson(progress);
  const nextEntry = next ? LESSONS.get(next) : undefined;
  const totalLessons = CHAPTERS.reduce((n, c) => n + c.lessons.length, 0);
  const doneLessons = Object.keys(progress.lessons).filter((id) => LESSONS.has(id)).length;
  const earned = BADGES.filter((b) => progress.badges[b.id]).length;
  const previous = settings?.recent_workspaces.find((p) => p !== bootcamp?.path);
  const streakAlive = progress.streak.last && (Date.now() - Date.parse(`${progress.streak.last}T00:00:00`)) / 86_400_000 < 2;

  return (
    <div className="pane bc-home">
      <div className="page" style={{ maxWidth: 1100 }}>
        <section className="bc-hero">
          <div className="bc-hero-main">
            <span className="eyebrow">
              <GraduationCap size={13} /> TwinBase Bootcamp
            </span>
            <h1>Learn how databases really work.</h1>
            <p>Short lessons in plain words, quick quizzes, and hands-on labs on real mock servers — from your first SELECT to sharding and vector search.</p>
            <div className="row wrap" style={{ marginTop: 14 }}>
              {nextEntry ? (
                <button className="btn primary lg" onClick={() => openTab({ kind: "lesson", lessonId: next! })}>
                  <Play size={15} /> {doneLessons ? "Continue" : "Start"}: {nextEntry.lesson.title}
                </button>
              ) : (
                <span className="badge success">
                  <Check size={12} /> Every lesson completed — replay any for practice.
                </span>
              )}
              {previous && (
                <button className="btn ghost" onClick={() => attempt("Could not open workspace", () => switchWorkspace(previous))} title={previous}>
                  <LogOut size={14} /> Back to my workspace
                </button>
              )}
            </div>
          </div>
          <div className="bc-level">
            <div className="bc-level-ring">
              <Ring value={lvl.fraction} size={116} stroke={10} />
              <div className="bc-level-n">
                <span>Level</span>
                <b>{lvl.level}</b>
              </div>
            </div>
            <div className="bc-level-title">{lvl.title}</div>
            <div className="faint xs">
              {progress.xp - lvl.floor} / {lvl.next - lvl.floor} XP to level {lvl.level + 1}
            </div>
          </div>
        </section>

        <div className="bc-stats">
          <div className="bc-stat">
            <Zap size={18} className="c-xp" />
            <b>{progress.xp.toLocaleString()}</b>
            <span>XP</span>
          </div>
          <div className="bc-stat">
            <Flame size={18} className={streakAlive ? "c-fire" : "faint"} />
            <b>{streakAlive ? progress.streak.count : 0}</b>
            <span>day streak</span>
          </div>
          <div className="bc-stat">
            <BookOpen size={18} className="c-book" />
            <b>
              {doneLessons}/{totalLessons}
            </b>
            <span>lessons</span>
          </div>
          <div className="bc-stat">
            <FlaskConical size={18} className="c-lab" />
            <b>{progress.labs}</b>
            <span>labs solved</span>
          </div>
          <div className="bc-stat">
            <Award size={18} className="c-badge" />
            <b>
              {earned}/{BADGES.length}
            </b>
            <span>badges</span>
          </div>
        </div>

        <div className="section-head">
          <h2>Chapters</h2>
          <span className="sub">Lessons unlock in order inside a chapter; jump to any chapter you like.</span>
        </div>
        <div className="bc-chapters">
          {CHAPTERS.map((ch, ci) => {
            const done = chapterDone(progress, ch.id);
            const badge = BADGES.find((b) => b.id === ch.badge);
            const complete = done === ch.lessons.length;
            return (
              <div key={ch.id} className={`bc-chapter${complete ? " complete" : ""}`} style={{ ["--ch" as string]: ch.color }}>
                <div className="bc-chapter-head">
                  <span className="bc-chapter-n">{String(ci + 1).padStart(2, "0")}</span>
                  <div className="grow">
                    <div className="bc-chapter-title">{ch.title}</div>
                    <div className="bc-chapter-sub">{ch.subtitle}</div>
                  </div>
                  {badge && <img className={`bc-chapter-badge${complete ? "" : " locked"}`} src={badge.image} alt={badge.name} title={complete ? badge.name : `Finish the chapter to earn “${badge.name}”`} />}
                </div>
                <div className="bc-progress">
                  <span style={{ width: `${(done / ch.lessons.length) * 100}%` }} />
                </div>
                <div className="bc-lessons">
                  {ch.lessons.map((l) => {
                    const rec = progress.lessons[l.id];
                    const open = isUnlocked(progress, l.id);
                    const hasLab = l.steps.some((s) => s.kind === "lab");
                    return (
                      <button key={l.id} className={`bc-lesson${rec ? " done" : ""}${!open ? " locked" : ""}${l.id === next ? " next" : ""}`} disabled={!open} onClick={() => openTab({ kind: "lesson", lessonId: l.id })} title={l.summary}>
                        <span className="bc-lesson-icon">{rec ? <Check size={13} /> : open ? <Play size={11} /> : <Lock size={11} />}</span>
                        <span className="grow ellipsis">{l.title}</span>
                        {hasLab && <FlaskConical size={12} className="faint" aria-label="has a lab" />}
                        {rec?.perfect && <span className="bc-perfect" title="Perfect">★</span>}
                        <span className="faint xs">{l.minutes}m</span>
                      </button>
                    );
                  })}
                </div>
              </div>
            );
          })}
        </div>

        <div className="section-head">
          <h2>Badges</h2>
          <span className="sub">
            {earned} of {BADGES.length} earned
          </span>
        </div>
        <div className="bc-badges">
          {BADGES.map((b) => {
            const at = progress.badges[b.id];
            return (
              <div key={b.id} className={`bc-badge${at ? "" : " locked"}`} title={at ? `Earned ${new Date(at).toLocaleDateString()}` : "Not earned yet"}>
                <img src={b.image} alt="" />
                <div className="bc-badge-name">{b.name}</div>
                <div className="bc-badge-desc">{b.description}</div>
              </div>
            );
          })}
        </div>

        <div className="row" style={{ marginTop: 28 }}>
          <span className="faint xs">Progress is saved on this computer. Lab servers live in the Bootcamp workspace, separate from your projects.</span>
          <span className="grow" />
          <button className="btn sm ghost" onClick={() => setConfirmReset(true)}>
            <RotateCcw size={12} /> Reset progress
          </button>
          {next && (
            <button className="btn sm" onClick={() => openTab({ kind: "lesson", lessonId: next })}>
              Next lesson <ArrowRight size={13} />
            </button>
          )}
        </div>
      </div>
      {confirmReset && <ConfirmDialog title="Reset Bootcamp progress?" body="XP, streak, badges and completed lessons are cleared. This cannot be undone." confirmLabel="Reset" danger onClose={() => setConfirmReset(false)} onConfirm={() => reset()} />}
    </div>
  );
}
