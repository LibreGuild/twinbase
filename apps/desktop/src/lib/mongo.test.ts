import { describe, expect, it } from "vitest";
import { columnsOf, shellText, shortText, typeOf, type Json } from "./mongo";

describe("mongo values", () => {
  const doc: Json = {
    _id: { $oid: "65a1b2c3d4e5f60718293a4b" },
    name: "Ann",
    "order-count": 3,
    at: { $date: "2024-05-01T10:00:00Z" },
    big: { $numberLong: "9007199254740993" },
    re: { $regularExpression: { pattern: "^a/b", options: "i" } },
    nested: { city: "Lisbon", tags: ["a", "b"] },
  };

  it("writes editable mongosh text", () => {
    const text = shellText(doc);
    expect(text).toContain('_id: ObjectId("65a1b2c3d4e5f60718293a4b")');
    expect(text).toContain('"order-count": 3');
    expect(text).toContain('at: ISODate("2024-05-01T10:00:00Z")');
    expect(text).toContain('big: NumberLong("9007199254740993")');
    expect(text).toContain("re: /^a\\/b/i");
    expect(text).toContain('nested: { city: "Lisbon", tags: ["a", "b"] }');
    expect(text.startsWith("{\n  _id")).toBe(true);
  });

  it("types, short text and table columns", () => {
    expect(typeOf({ $oid: "x" })).toBe("objectId");
    expect(typeOf(1.5)).toBe("double");
    expect(typeOf([1])).toBe("array");
    expect(shortText({ $date: "2024-05-01T10:00:00Z" })).toBe("2024-05-01T10:00:00Z");
    expect(shortText("x".repeat(200), 10)).toHaveLength(10);
    expect(columnsOf([{ b: 1 }, { _id: 1, a: 2 }])).toEqual(["_id", "b", "a"]);
    expect(columnsOf([{ b: 1 }])).toEqual(["b"]);
  });
});

import { formatItem, typed, unmarshal } from "./dynamo";

describe("dynamo values", () => {
  it("formats, types and unmarshals", () => {
    expect(formatItem({ pk: { S: "a" }, n: { N: "1.5" } })).toBe('{\n  "pk": { "S": "a" },\n  "n": { "N": "1.5" }\n}');
    expect(typed("N", " 42 ")).toEqual({ N: "42" });
    expect(() => typed("N", "abc")).toThrow();
    expect(typed("BOOL", "true")).toEqual({ BOOL: true });
    expect(unmarshal({ M: { a: { NS: ["1", "2.5"] }, b: { N: "12345678901234567890" } } })).toEqual({ a: [1, 2.5], b: "12345678901234567890" });
  });
});
