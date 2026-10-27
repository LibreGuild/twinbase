// @vitest-environment jsdom
import { describe, expect, it } from "vitest";
import { renderMarkdown } from "./markdown";
import { isReadOnlySql, riskyStatement } from "./format";

describe("markdown", () => {
  it("escapes raw HTML", () => {
    const html = renderMarkdown("<script>alert(1)</script> and <b>bold</b>");
    expect(html).not.toContain("<script");
    expect(html).not.toContain("<b>");
    expect(html).toContain("&lt;b&gt;");
  });
  it("renders headings, lists, code, tables and links", () => {
    const html = renderMarkdown("## Why\n- one `x`\n- **two**\n\n```sql\nselect 1 < 2;\n```\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n[docs](https://example.com)");
    expect(html).toContain("<h2>Why</h2>");
    expect(html).toContain("<code>x</code>");
    expect(html).toContain("<strong>two</strong>");
    expect(html).toContain("select 1 &lt; 2;");
    expect(html).toContain("<table>");
    expect(html).toContain('href="https://example.com"');
  });
  it("drops javascript: links", () => {
    expect(renderMarkdown("[x](javascript:alert(1))")).not.toContain("javascript:");
  });
  it("turns mermaid blocks into diagram placeholders", () => {
    const html = renderMarkdown("```mermaid\ngraph TD; A-->B\n```");
    expect(html).toContain('class="mermaid-block"');
    expect(html).toContain("data-src=");
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
