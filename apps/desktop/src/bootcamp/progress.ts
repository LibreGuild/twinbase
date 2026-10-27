// Bootcamp progress: XP, levels, streaks and badges. Saved by the core (bootcamp-progress.json in
// the app directory), so it survives reinstalls of the Bootcamp workspace.

import { create } from "zustand";
import { call } from "../lib/api";
import { BADGES, CHAPTERS, LESSONS } from "./curriculum";

export interface LessonRecord {
  completedAt: string;
  xp: number;
  perfect: boolean;
}

export interface Progress {
  version: 1;
  xp: number;
  lessons: Record<string, LessonRecord>;
  /** Badge id → date earned. */
  badges: Record<string, string>;
  labs: number;
  streak: { count: number; last: string };
}

export const XP = { quizFirstTry: 10, quizRetry: 4, lab: 25, labWithHint: 15, lessonDone: 15, perfectBonus: 10 };

const EMPTY: Progress = { version: 1, xp: 0, lessons: {}, badges: {}, labs: 0, streak: { count: 0, last: "" } };

const LEVELS = [0, 100, 250, 450, 700, 1000, 1350, 1750, 2200, 2700, 3300];
const TITLES = ["Curious Intern", "Junior Querier", "Data Apprentice", "Index Wrangler", "Schema Smith", "Query Engineer", "Senior Engineer", "Staff Engineer", "Principal Engineer", "Database Wizard", "Legend of the WAL"];

export function levelOf(xp: number) {
  let level = 0;
  while (level + 1 < LEVELS.length && xp >= LEVELS[level + 1]) level++;
  const floor = LEVELS[level];
  const next = LEVELS[level + 1] ?? floor + 600;
  return { level: level + 1, title: TITLES[level], floor, next, fraction: Math.min(1, (xp - floor) / (next - floor)) };
}

function today(): string {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

function daysBetween(a: string, b: string): number {
  return Math.round((Date.parse(`${b}T00:00:00`) - Date.parse(`${a}T00:00:00`)) / 86_400_000);
}

export function chapterDone(p: Progress, chapterId: string): number {
  const ch = CHAPTERS.find((c) => c.id === chapterId);
  return ch ? ch.lessons.filter((l) => p.lessons[l.id]).length : 0;
}

/** The first lesson not yet completed, in syllabus order. */
export function nextLesson(p: Progress): string | undefined {
  for (const ch of CHAPTERS) for (const l of ch.lessons) if (!p.lessons[l.id]) return l.id;
  return undefined;
}

/** Lessons unlock in order within a chapter; chapters are all open so learners can skip ahead. */
export function isUnlocked(p: Progress, lessonId: string): boolean {
  const entry = LESSONS.get(lessonId);
  if (!entry || entry.index === 0) return true;
  return !!p.lessons[entry.chapter.lessons[entry.index - 1].id] || !!p.lessons[lessonId];
}

interface Store {
  progress: Progress;
  loaded: boolean;
  load(): Promise<void>;
  /** Record a finished lesson; returns newly earned badge ids. */
  complete(lessonId: string, xp: number, perfect: boolean, labs: number): string[];
  reset(): void;
}

let saveTimer: ReturnType<typeof setTimeout> | undefined;
function persist(p: Progress) {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => void call("bootcamp.progress.save", { progress: p }).catch(() => undefined), 300);
}

export const useProgress = create<Store>((set, get) => ({
  progress: EMPTY,
  loaded: false,
  async load() {
    try {
      const p = await call<Partial<Progress>>("bootcamp.progress.get");
      set({ progress: { ...EMPTY, ...p, streak: { ...EMPTY.streak, ...(p.streak ?? {}) } }, loaded: true });
    } catch {
      set({ loaded: true });
    }
  },
  complete(lessonId, xp, perfect, labs) {
    const before = get().progress;
    const first = !before.lessons[lessonId];
    const day = today();
    const gap = before.streak.last ? daysBetween(before.streak.last, day) : 99;
    const streak = gap === 0 ? before.streak : { count: gap === 1 ? before.streak.count + 1 : 1, last: day };
    const p: Progress = {
      ...before,
      // Replaying a lesson earns a quarter of its XP, so practice still counts a little.
      xp: before.xp + (first ? xp : Math.round(xp / 4)),
      labs: before.labs + labs,
      lessons: { ...before.lessons, [lessonId]: { completedAt: new Date().toISOString(), xp: Math.max(xp, before.lessons[lessonId]?.xp ?? 0), perfect: perfect || !!before.lessons[lessonId]?.perfect } },
      streak,
      badges: { ...before.badges },
    };
    const earned: string[] = [];
    const award = (id: string, cond: boolean) => {
      if (cond && !p.badges[id] && BADGES.some((b) => b.id === id)) {
        p.badges[id] = new Date().toISOString();
        earned.push(id);
      }
    };
    award("first-steps", true);
    award("lab-rat", p.labs > 0);
    award("perfectionist", perfect);
    award("streak-3", streak.count >= 3);
    award("streak-7", streak.count >= 7);
    for (const ch of CHAPTERS) award(ch.badge, ch.lessons.every((l) => p.lessons[l.id]));
    award("graduate", CHAPTERS.every((ch) => ch.lessons.every((l) => p.lessons[l.id])));
    set({ progress: p });
    persist(p);
    return earned;
  },
  reset() {
    set({ progress: EMPTY });
    persist(EMPTY);
  },
}));
