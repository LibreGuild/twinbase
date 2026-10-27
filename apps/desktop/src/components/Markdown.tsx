// Rendered Markdown with Mermaid diagrams, and the Write / Preview / Split editor used for docs
// across the app (workspace, connections, mocks, tables, columns, saved queries, collections).

import { Columns2, Eye, PenLine } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import DOMPurify from "dompurify";
import { openExternal } from "../lib/api";
import { renderMarkdown } from "../lib/markdown";
import { useAppliedTheme } from "../lib/theme";

let mermaidSeq = 0;
type MermaidApi = typeof import("mermaid").default;
let mermaidLoad: Promise<MermaidApi> | null = null;
function loadMermaid(): Promise<MermaidApi> {
  mermaidLoad ??= import("mermaid").then((m) => m.default);
  return mermaidLoad;
}

/** Replace `.mermaid-block` placeholders inside `root` with rendered diagrams. */
async function renderDiagrams(root: HTMLElement, theme: "light" | "dark") {
  const blocks = [...root.querySelectorAll<HTMLElement>(".mermaid-block")];
  if (!blocks.length) return;
  const mermaid = await loadMermaid();
  mermaid.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    theme: "base",
    // The app's palette: teal accents on warm neutrals.
    themeVariables:
      theme === "dark"
        ? { darkMode: true, background: "#23201c", primaryColor: "#2c3a37", primaryBorderColor: "#55a9a0", primaryTextColor: "#ece6db", secondaryColor: "#34302a", tertiaryColor: "#23201c", lineColor: "#8f877a", textColor: "#ece6db", noteBkgColor: "#34302a", noteTextColor: "#ece6db", actorBkg: "#2c3a37", actorBorder: "#55a9a0", signalColor: "#aba295", fontSize: "13px" }
        : { background: "#ffffff", primaryColor: "#e6f2f0", primaryBorderColor: "#55a9a0", primaryTextColor: "#1d1c1a", secondaryColor: "#f4f2ec", tertiaryColor: "#ffffff", lineColor: "#8d887d", textColor: "#1d1c1a", noteBkgColor: "#fbf3df", actorBkg: "#e6f2f0", actorBorder: "#55a9a0", fontSize: "13px" },
    fontFamily: "Inter Variable, -apple-system, system-ui, sans-serif",
  });
  for (const block of blocks) {
    const src = decodeURIComponent(block.dataset.src ?? "");
    try {
      const { svg } = await mermaid.render(`mmd-${++mermaidSeq}`, src);
      if (!block.isConnected) return;
      // Mermaid draws labels as HTML inside <foreignObject>; keep them, still sanitized.
      block.innerHTML = DOMPurify.sanitize(svg, {
        USE_PROFILES: { svg: true, svgFilters: true, html: true },
        ADD_TAGS: ["foreignObject"],
        HTML_INTEGRATION_POINTS: { foreignobject: true },
      });
      block.classList.add("rendered");
    } catch (e) {
      block.classList.add("failed");
      const note = document.createElement("div");
      note.className = "mermaid-error";
      note.textContent = `Diagram error: ${e instanceof Error ? e.message.split("\n")[0] : String(e)}`;
      block.appendChild(note);
    }
  }
}

export function Markdown({ source, className, empty, style }: { source: string; className?: string; empty?: string; style?: React.CSSProperties }) {
  const html = useMemo(() => renderMarkdown(source), [source]);
  const ref = useRef<HTMLDivElement>(null);
  const theme = useAppliedTheme();
  useEffect(() => {
    if (ref.current) void renderDiagrams(ref.current, theme);
  }, [html, theme]);
  if (!html) return empty ? <div className={`markdown ${className ?? ""}`} style={style}><p className="faint">{empty}</p></div> : null;
  return (
    <div
      ref={ref}
      className={`markdown ${className ?? ""}`}
      style={style}
      dangerouslySetInnerHTML={{ __html: html }}
      onClick={(e) => {
        // Links open in the system browser, not inside the app window.
        const a = (e.target as HTMLElement).closest("a");
        const href = a?.getAttribute("href");
        if (a && href && !href.startsWith("#")) {
          e.preventDefault();
          void openExternal(href).catch(() => undefined);
        }
      }}
    />
  );
}

type Mode = "write" | "preview" | "split";

export function DocsEditor({
  value,
  onChange,
  rows = 8,
  placeholder = "Markdown: what this is for, how it is used, caveats… Mermaid diagrams go in ```mermaid blocks.",
  initialMode,
}: {
  value: string;
  onChange: (v: string) => void;
  rows?: number;
  placeholder?: string;
  initialMode?: Mode;
}) {
  const [mode, setMode] = useState<Mode>(initialMode ?? (value.trim() ? "preview" : "write"));
  const editor = (
    <textarea className="textarea mono docs-textarea" rows={rows} value={value} onChange={(e) => onChange(e.target.value)} placeholder={placeholder} spellCheck />
  );
  const preview = <Markdown source={value} className="note-box docs-preview" empty="Nothing to preview yet." style={{ minHeight: rows * 20 }} />;
  return (
    <div className="docs-editor">
      <div className="seg" style={{ marginBottom: 6 }}>
        <button className={mode === "write" ? "on" : ""} onClick={() => setMode("write")}>
          <PenLine size={12} /> Write
        </button>
        <button className={mode === "preview" ? "on" : ""} onClick={() => setMode("preview")}>
          <Eye size={12} /> Preview
        </button>
        <button className={mode === "split" ? "on" : ""} onClick={() => setMode("split")}>
          <Columns2 size={12} /> Split
        </button>
      </div>
      {mode === "write" ? editor : mode === "preview" ? <div onDoubleClick={() => setMode("write")} title="Double-click to edit">{preview}</div> : (
        <div className="docs-split">
          {editor}
          {preview}
        </div>
      )}
    </div>
  );
}
