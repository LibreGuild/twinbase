// The Bootcamp syllabus, in order.

import type { BadgeDef, Chapter, Lesson } from "../types";
import { caching } from "./caching";
import { foundations } from "./foundations";
import { indexes } from "./indexes";
import { internals } from "./internals";
import { modeling } from "./modeling";
import { nosql } from "./nosql";
import { scale } from "./scale";
import { sql } from "./sql";
import { transactions } from "./transactions";
import { vectors } from "./vectors";

export const CHAPTERS: Chapter[] = [foundations, sql, modeling, indexes, transactions, internals, caching, nosql, scale, vectors];

export const LESSONS: Map<string, { lesson: Lesson; chapter: Chapter; index: number }> = new Map(
  CHAPTERS.flatMap((chapter) => chapter.lessons.map((lesson, index) => [lesson.id, { lesson, chapter, index }] as const)),
);

const img = (id: string) => new URL(`../../assets/badges/${id}.webp`, import.meta.url).href;

export const BADGES: BadgeDef[] = [
  { id: "foundations", name: "Foundations", description: "Finished Database Foundations.", image: img("foundations") },
  { id: "sql", name: "Query Crafter", description: "Finished SQL Essentials.", image: img("sql") },
  { id: "architect", name: "Data Architect", description: "Finished Data Modeling.", image: img("architect") },
  { id: "speed", name: "Speed Demon", description: "Finished Indexes & Performance.", image: img("speed") },
  { id: "acid", name: "ACID Keeper", description: "Finished Transactions & ACID.", image: img("acid") },
  { id: "internals", name: "Under the Hood", description: "Finished Storage Internals.", image: img("internals") },
  { id: "cache", name: "Cache Master", description: "Finished Caching with Redis.", image: img("cache") },
  { id: "nosql", name: "Document Ranger", description: "Finished Documents & Key-Value.", image: img("nosql") },
  { id: "scale", name: "Scale Architect", description: "Finished Scaling & System Design.", image: img("scale") },
  { id: "vectors", name: "Vector Voyager", description: "Finished Vectors & Embeddings.", image: img("vectors") },
  { id: "first-steps", name: "First Steps", description: "Completed your first lesson.", image: img("first-steps") },
  { id: "lab-rat", name: "Lab Rat", description: "Solved your first hands-on lab.", image: img("lab-rat") },
  { id: "perfectionist", name: "Perfectionist", description: "Finished a lesson with every answer right first time.", image: img("perfectionist") },
  { id: "streak-3", name: "On Fire", description: "Learned three days in a row.", image: img("streak-3") },
  { id: "streak-7", name: "Week Warrior", description: "Learned seven days in a row.", image: img("streak-7") },
  { id: "graduate", name: "Bootcamp Graduate", description: "Finished every chapter.", image: img("graduate") },
];
