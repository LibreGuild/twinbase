// DynamoDB attribute values: typed JSON ({"S": "x"}) as the API uses it, plain values for
// display, and typed values built from what the user types.

import type { Json } from "./mongo";

export type AttrValue = Record<string, unknown>;
export type DynItem = Record<string, AttrValue>;
export type ScalarType = "S" | "N" | "B" | "BOOL" | "NULL";

/** Plain JSON for display: sets become arrays, numbers stay exact (strings when too big). */
export function unmarshal(v: AttrValue): Json {
  const [t, x] = Object.entries(v)[0] ?? ["NULL", true];
  switch (t) {
    case "S":
    case "B":
      return x as string;
    case "N": {
      const n = Number(x);
      return Number.isSafeInteger(n) || (Number.isFinite(n) && String(n) === x) ? n : (x as string);
    }
    case "BOOL":
      return x as boolean;
    case "NULL":
      return null;
    case "SS":
    case "BS":
      return x as string[];
    case "NS":
      return (x as string[]).map((n) => unmarshal({ N: n }));
    case "L":
      return (x as AttrValue[]).map(unmarshal);
    case "M":
      return unmarshalItem(x as DynItem);
    default:
      return JSON.stringify(v);
  }
}

export function unmarshalItem(item: DynItem): Record<string, Json> {
  return Object.fromEntries(Object.entries(item).map(([k, v]) => [k, unmarshal(v)]));
}

/** The type tag of a typed value (S, N, SS, M, …). */
export function typeTag(v: AttrValue | undefined): string {
  return v ? (Object.keys(v)[0] ?? "") : "";
}

/** A typed scalar from text the user typed. */
export function typed(type: ScalarType, text: string): AttrValue {
  switch (type) {
    case "N": {
      const t = text.trim();
      if (!/^[-+]?(\d+\.?\d*|\.\d+)([eE][-+]?\d+)?$/.test(t)) throw new Error(`"${text}" is not a number`);
      return { N: t };
    }
    case "BOOL":
      return { BOOL: /^(true|1|yes)$/i.test(text.trim()) };
    case "NULL":
      return { NULL: true };
    case "B":
      return { B: text.trim() };
    default:
      return { S: text };
  }
}

/** The key attributes of an item, for edits and deletes. */
export function keyOf(item: DynItem, keyNames: string[]): DynItem {
  return Object.fromEntries(keyNames.filter((k) => k in item).map((k) => [k, item[k]]));
}

/** Table columns: key attributes first, then others in order of first appearance. */
export function itemColumns(items: DynItem[], keyNames: string[]): string[] {
  const seen = new Set<string>(keyNames);
  for (const it of items) for (const k of Object.keys(it)) seen.add(k);
  return [...seen];
}

export interface KeySchemaElement {
  AttributeName: string;
  KeyType: "HASH" | "RANGE";
}

export interface TableDescription {
  TableName: string;
  KeySchema: KeySchemaElement[];
  AttributeDefinitions: { AttributeName: string; AttributeType: "S" | "N" | "B" }[];
  ItemCount?: number;
  TableSizeBytes?: number;
  TableStatus?: string;
  BillingModeSummary?: { BillingMode: string };
  GlobalSecondaryIndexes?: { IndexName: string; KeySchema: KeySchemaElement[]; Projection: { ProjectionType: string; NonKeyAttributes?: string[] }; ItemCount?: number }[];
  LocalSecondaryIndexes?: { IndexName: string; KeySchema: KeySchemaElement[]; Projection: { ProjectionType: string; NonKeyAttributes?: string[] }; ItemCount?: number }[];
  TimeToLiveDescription?: { TimeToLiveStatus: string; AttributeName?: string };
  TableArn?: string;
}

export function keyNames(schema: KeySchemaElement[] | undefined): { hash?: string; range?: string } {
  return { hash: schema?.find((k) => k.KeyType === "HASH")?.AttributeName, range: schema?.find((k) => k.KeyType === "RANGE")?.AttributeName };
}

export function attrType(desc: TableDescription | undefined, name: string | undefined): "S" | "N" | "B" {
  return desc?.AttributeDefinitions.find((a) => a.AttributeName === name)?.AttributeType ?? "S";
}

export const TABLE_TEMPLATE = `CreateTable {
  "TableName": "orders",
  "BillingMode": "PAY_PER_REQUEST",
  "KeySchema": [
    { "AttributeName": "pk", "KeyType": "HASH" },
    { "AttributeName": "sk", "KeyType": "RANGE" }
  ],
  "AttributeDefinitions": [
    { "AttributeName": "pk", "AttributeType": "S" },
    { "AttributeName": "sk", "AttributeType": "S" }
  ]
}`;

/** Typed JSON with one attribute per line; small values stay on their line. */
export function formatItem(item: DynItem): string {
  const lines = Object.entries(item).map(([k, v]) => {
    const flat = JSON.stringify(v, null, 1).replace(/\n\s*/g, " ");
    const text = flat.length <= 100 ? flat : JSON.stringify(v, null, 2).replace(/\n/g, "\n  ");
    return `  ${JSON.stringify(k)}: ${text}`;
  });
  return lines.length ? `{\n${lines.join(",\n")}\n}` : "{}";
}
