// Markdown for docs everywhere in the app: GitHub-flavoured Markdown rendered by `marked`, then
// sanitized with DOMPurify. ```mermaid blocks become placeholders that <Markdown> turns into
// diagrams after mounting (see components/Markdown.tsx).

import DOMPurify from "dompurify";
import { Marked, type Tokens } from "marked";

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

const md = new Marked({
  gfm: true,
  breaks: true,
  renderer: {
    // Raw HTML in a doc is shown as text, never rendered.
    html({ text }: Tokens.HTML | Tokens.Tag) {
      return esc(text);
    },
    code({ text, lang }: Tokens.Code) {
      const language = (lang ?? "").trim().split(/\s+/)[0].toLowerCase();
      if (language === "mermaid") return `<div class="mermaid-block" data-src="${encodeURIComponent(text)}"><pre><code>${esc(text)}</code></pre></div>`;
      return `<pre><code${language ? ` class="language-${esc(language)}"` : ""}>${esc(text)}</code></pre>`;
    },
  },
});

const purify = typeof window !== "undefined" ? DOMPurify(window) : null;
if (purify) {
  purify.addHook("afterSanitizeAttributes", (node) => {
    if (node.tagName === "A") {
      const href = node.getAttribute("href") ?? "";
      if (!/^(https?:|mailto:|#)/i.test(href)) node.removeAttribute("href");
      node.setAttribute("target", "_blank");
      node.setAttribute("rel", "noreferrer noopener");
    }
  });
}

/** Markdown to sanitized HTML. Raw HTML in the source is escaped, not rendered. */
export function renderMarkdown(src: string): string {
  if (!src.trim()) return "";
  const html = md.parse(src.replace(/\r\n/g, "\n"), { async: false }) as string;
  if (!purify) return html;
  return purify.sanitize(html, { ADD_ATTR: ["target", "data-src"], FORBID_TAGS: ["style", "form", "input", "iframe", "img"], ALLOW_DATA_ATTR: true });
}
