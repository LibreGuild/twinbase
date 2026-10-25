import { describe, expect, it } from "vitest";
import { renderMarkdown } from "./markdown";
import { isReadOnlySql, riskyStatement } from "./format";

describe("markdown", () => {
  it("escapes HTML", () => {
    expect(renderMarkdown("<script>alert(1)</script>")).toBe("<p>&lt;script&gt;alert(1)&lt;/script&gt;</p>");
  });
  it("renders headings, lists, code and links", () => {
    const html = renderMarkdown("## Why\n- one `x`\n- **two**\n\n```\nselect 1;\n```\n[docs](https://example.com)");
    expect(html).toContain("<h2>Why</h2>");
    expect(html).toContain("<li>one <code>x</code></li>");
    expect(html).toContain("<strong>two</strong>");
    expect(html).toContain("<pre><code>select 1;</code></pre>");
    expect(html).toContain('href="https://example.com"');
  });
  it("does not turn javascript: links into anchors", () => {
    expect(renderMarkdown("[x](javascript:alert(1))")).not.toContain("<a");
  });
});

describe("sql classification", () => {
  it("flags unfiltered writes and drops", () => {
    expect(riskyStatement("DELETE FROM users")).toBeTruthy();
    expect(riskyStatement("update users set a = 1")).toBeTruthy();
    expect(riskyStatement("DROP TABLE users")).toBeTruthy();
    expect(riskyStatement("DELETE FROM users WHERE id = 1")).toBeUndefined();
    expect(riskyStatement("SELECT 'DELETE FROM x'")).toBeUndefined();
  });
  it("detects read-only SQL", () => {
    expect(isReadOnlySql("select 1; with x as (select 1) select * from x")).toBe(true);
    expect(isReadOnlySql("insert into t values (1)")).toBe(false);
    expect(isReadOnlySql("SELECT * INTO copy FROM t")).toBe(false);
    expect(isReadOnlySql("SELECT 1\nGO\nDELETE FROM t")).toBe(false);
    expect(isReadOnlySql("SELECT TOP 5 * FROM [t]\ngo\nSELECT 2")).toBe(true);
    expect(isReadOnlySql("with x as (delete from t returning *) select * from x")).toBe(false);
  });
});
