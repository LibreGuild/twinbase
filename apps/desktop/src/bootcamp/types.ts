// Bootcamp content model: chapters of short lessons; each lesson is a sequence of steps (reading
// cards, quick quizzes and hands-on labs on the Bootcamp's mock servers).

export type WidgetId =
  | "table-anatomy"
  | "join-explorer"
  | "index-race"
  | "btree"
  | "tx-isolation"
  | "cache-sim"
  | "hash-ring"
  | "replication"
  | "vector-space"
  | "lsm-vs-btree"
  | "normalize";

export interface ReadStep {
  kind: "read";
  title: string;
  /** Markdown; ```mermaid blocks render as diagrams. */
  body: string;
  /** An interactive illustration shown under the text. */
  widget?: WidgetId;
}

interface QuestionBase {
  prompt: string;
  /** Shown after answering (right or wrong): why. */
  explain: string;
}

export type Question =
  | (QuestionBase & { type: "choice"; options: string[]; answer: number })
  | (QuestionBase & { type: "multi"; options: string[]; answers: number[] })
  | (QuestionBase & { type: "truefalse"; answer: boolean })
  /** `prompt` contains one `___`; any of `answers` is accepted (case and spacing ignored). */
  | (QuestionBase & { type: "blank"; answers: string[] })
  | (QuestionBase & { type: "match"; pairs: [string, string][] })
  /** `items` in the correct order; shown shuffled. */
  | (QuestionBase & { type: "order"; items: string[] });

export interface QuizStep {
  kind: "quiz";
  q: Question;
}

export type LabEngine = "postgres" | "redis" | "mongodb";

export interface Lab {
  engine: LabEngine;
  title: string;
  /** Markdown task description. */
  task: string;
  /** Runs before the lab starts (SQL script, Redis commands one per line, or mongosh statements). */
  setup: string;
  starter: string;
  hint: string;
  /** A correct answer; the user's result must match what this returns. */
  solution: string;
  /**
   * How the attempt is judged:
   * - `result`: the user's last statement returns the same rows as `solution` (order matters only
   *   when the solution sorts).
   * - `state`: after the user's statements, `query` returns `expect` (rows as text).
   */
  check: { kind: "result" } | { kind: "state"; query: string; expect: string };
}

export interface LabStep {
  kind: "lab";
  lab: Lab;
}

export type Step = ReadStep | QuizStep | LabStep;

export interface Lesson {
  id: string;
  title: string;
  /** One-line summary for the chapter map. */
  summary: string;
  minutes: number;
  steps: Step[];
}

export interface Chapter {
  id: string;
  title: string;
  subtitle: string;
  /** Accent for the chapter card (CSS colour). */
  color: string;
  /** Badge earned by finishing every lesson. */
  badge: string;
  lessons: Lesson[];
}

export interface BadgeDef {
  id: string;
  name: string;
  description: string;
  image: string;
}
