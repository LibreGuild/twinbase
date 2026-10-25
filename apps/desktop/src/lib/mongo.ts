// MongoDB values as the UI shows and edits them: relaxed Extended JSON from the core, written
// back in mongosh syntax (ObjectId("…"), ISODate("…"), unquoted keys), which the core parses.

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

const IDENT = /^[A-Za-z_$][\w$]*$/;

function str(s: string): string {
  return JSON.stringify(s);
}

/** The special constructor text for an Extended JSON wrapper, or undefined for plain objects. */
export function special(v: Json): string | undefined {
  if (!v || typeof v !== "object" || Array.isArray(v)) return undefined;
  const keys = Object.keys(v);
  const o = v as Record<string, Json>;
  if (keys.length === 1) {
    const [k] = keys;
    const x = o[k];
    switch (k) {
      case "$oid":
        return `ObjectId(${str(String(x))})`;
      case "$date":
        if (typeof x === "string") return `ISODate(${str(x)})`;
        if (x && typeof x === "object" && "$numberLong" in x) return `new Date(${Number((x as Record<string, Json>).$numberLong)})`;
        return `new Date(${Number(x)})`;
      case "$numberLong":
        return `NumberLong(${str(String(x))})`;
      case "$numberDecimal":
        return `NumberDecimal(${str(String(x))})`;
      case "$numberInt":
        return `NumberInt(${String(x)})`;
      case "$numberDouble":
        return String(x);
      case "$uuid":
        return `UUID(${str(String(x))})`;
      case "$minKey":
        return "MinKey()";
      case "$maxKey":
        return "MaxKey()";
      case "$undefined":
        return "undefined";
      case "$regularExpression": {
        const r = x as { pattern: string; options: string };
        return `/${regexLiteralBody(r.pattern)}/${r.options}`;
      }
      case "$timestamp": {
        const t = x as { t: number; i: number };
        return `Timestamp(${t.t}, ${t.i})`;
      }
      case "$binary": {
        const b = x as { base64: string; subType: string };
        return `BinData(${parseInt(b.subType, 16)}, ${str(b.base64)})`;
      }
      // Firestore markers (plain JSON form used by TwinBase).
      case "__timestamp":
        return `Timestamp(${str(String(x))})`;
      case "__ref":
        return `Ref(${str(String(x))})`;
      case "__geo": {
        const [lat, lng] = x as number[];
        return `GeoPoint(${lat}, ${lng})`;
      }
      case "__bytes":
        return `Bytes(${str(String(x))})`;
      case "__double":
        return String(x);
    }
  }
  return undefined;
}

/** Pretty mongosh-style text for a value (editable and re-parsable by the core). */
export function shellText(v: Json, indent = "", step = "  "): string {
  const sp = special(v);
  if (sp !== undefined) return sp;
  if (v === null) return "null";
  if (typeof v === "string") return str(v);
  if (typeof v !== "object") return String(v);
  const inner = indent + step;
  if (Array.isArray(v)) {
    if (v.length === 0) return "[]";
    const items = v.map((x) => shellText(x, inner, step));
    const flat = `[${items.join(", ")}]`;
    if (flat.length <= 72 && !flat.includes("\n")) return flat;
    return `[\n${items.map((x) => inner + x).join(",\n")}\n${indent}]`;
  }
  const entries = Object.entries(v);
  if (entries.length === 0) return "{}";
  const parts = entries.map(([k, x]) => `${IDENT.test(k) ? k : str(k)}: ${shellText(x, inner, step)}`);
  const flat = `{ ${parts.join(", ")} }`;
  if (flat.length <= 72 && !flat.includes("\n") && indent.length > 0) return flat;
  return `{\n${parts.map((x) => inner + x).join(",\n")}\n${indent}}`;
}

/** One-line text for grids and previews. */
export function shortText(v: Json, max = 120): string {
  const sp = special(v);
  let s: string;
  if (sp !== undefined) s = sp.replace(/^(?:ObjectId|ISODate|Timestamp)\("(.*)"\)$/, "$1").replace(/^Ref\("(.*)"\)$/, "→ $1");
  else if (v === null) s = "null";
  else if (typeof v === "string") s = v;
  else if (typeof v !== "object") s = String(v);
  else if (Array.isArray(v)) s = `[${v.length}] ${shellText(v, "", "").replace(/\n/g, " ")}`;
  else s = shellText(v, "", "").replace(/\n/g, " ");
  return s.length > max ? `${s.slice(0, max - 1)}…` : s;
}

/** Display type of a value, for badges and the table view. */
export function typeOf(v: Json): string {
  const sp = special(v);
  if (sp !== undefined) {
    const k = Object.keys(v as object)[0];
    return (
      { $oid: "objectId", $date: "date", $numberLong: "long", $numberDecimal: "decimal", $numberInt: "int", $numberDouble: "double", $uuid: "uuid", $regularExpression: "regex", $timestamp: "timestamp", $binary: "binData", __timestamp: "timestamp", __ref: "reference", __geo: "geopoint", __bytes: "bytes", __double: "double" }[k] ?? k.replace(/^\$|^__/, "")
    );
  }
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  if (typeof v === "object") return "object";
  if (typeof v === "number") return Number.isInteger(v) ? "int" : "double";
  if (typeof v === "boolean") return "bool";
  return "string";
}

/** Columns for a table view: top-level keys in order of first appearance, `_id` first. */
export function columnsOf(docs: Record<string, Json>[]): string[] {
  const seen = new Set<string>(["_id"]);
  for (const d of docs) for (const k of Object.keys(d)) seen.add(k);
  if (!docs.some((d) => "_id" in d)) seen.delete("_id");
  return [...seen];
}

export const PIPELINE_STAGES: Record<string, string> = {
  $match: "{ $match: { status: \"active\" } }",
  $group: "{ $group: { _id: \"$field\", count: { $sum: 1 } } }",
  $sort: "{ $sort: { count: -1 } }",
  $project: "{ $project: { _id: 0, name: 1 } }",
  $lookup: "{ $lookup: { from: \"other\", localField: \"otherId\", foreignField: \"_id\", as: \"other\" } }",
  $unwind: "{ $unwind: \"$items\" }",
  $limit: "{ $limit: 20 }",
  $addFields: "{ $addFields: { total: { $sum: \"$items.price\" } } }",
};

/** A regex pattern as the body of a `/…/` literal: unescaped slashes get a backslash, existing
 * escapes (including `\/`) stay as they are. */
export function regexLiteralBody(pattern: string): string {
  let out = "";
  let escaped = false;
  for (const ch of pattern) {
    if (escaped) {
      out += ch;
      escaped = false;
    } else if (ch === "\\") {
      out += ch;
      escaped = true;
    } else {
      out += ch === "/" ? "\\/" : ch;
    }
  }
  return out;
}
