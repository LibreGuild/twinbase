// CodeMirror 6 editor for SQL (dialect-aware highlighting and schema completion) and for
// mongosh-style MongoDB statements.

import { autocompletion, closeBrackets, closeBracketsKeymap, completionKeymap } from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { javascript } from "@codemirror/lang-javascript";
import { Cassandra, MSSQL, MySQL, PLSQL, PostgreSQL, SQLite, sql, type SQLConfig } from "@codemirror/lang-sql";
import { bracketMatching, HighlightStyle, indentOnInput, syntaxHighlighting } from "@codemirror/language";
import { highlightSelectionMatches, searchKeymap } from "@codemirror/search";
import { Compartment, EditorState, Prec } from "@codemirror/state";
import { drawSelection, EditorView, highlightActiveLine, highlightActiveLineGutter, keymap, lineNumbers, placeholder as placeholderExt } from "@codemirror/view";
import { tags as t } from "@lezer/highlight";
import { forwardRef, useEffect, useImperativeHandle, useRef } from "react";
import { CONSOLE_KINDS } from "../lib/format";
import type { DbKind } from "../lib/types";

const highlight = HighlightStyle.define([
  { tag: [t.keyword, t.operatorKeyword, t.modifier], color: "var(--kw)", fontWeight: "600" },
  { tag: [t.string, t.special(t.string)], color: "var(--str)" },
  { tag: [t.number, t.bool, t.null], color: "var(--num)" },
  { tag: [t.comment, t.lineComment, t.blockComment], color: "var(--cmt)", fontStyle: "italic" },
  { tag: [t.function(t.variableName), t.standard(t.name), t.typeName], color: "var(--fn)" },
  { tag: t.special(t.name), color: "var(--text)" },
  { tag: t.punctuation, color: "var(--text-muted)" },
]);

export interface SqlEditorHandle {
  /** Selected text, or the whole document when nothing is selected. */
  selectionOrAll(): string;
  /** The statement around the cursor (between semicolons). */
  currentStatement(): string;
  getValue(): string;
  setValue(v: string): void;
  focus(): void;
}

export interface SqlEditorProps {
  value: string;
  kind?: DbKind;
  schema?: SQLConfig["schema"];
  defaultSchema?: string;
  onChange?: (v: string) => void;
  onRun?: () => void;
  onRunCurrent?: () => void;
  onSave?: () => void;
  readOnly?: boolean;
  placeholder?: string;
  minHeight?: number;
}

function dialectFor(kind?: DbKind) {
  return kind === "mysql" || kind === "clickhouse" || kind === "spanner" || kind === "bigquery" ? MySQL : kind === "sqlite" ? SQLite : kind === "mssql" ? MSSQL : kind === "cassandra" ? Cassandra : kind === "oracle" ? PLSQL : PostgreSQL;
}

function languageFor(p: Pick<SqlEditorProps, "kind" | "schema" | "defaultSchema">) {
  if (p.kind && CONSOLE_KINDS.includes(p.kind)) return javascript();
  return sql({ dialect: dialectFor(p.kind), schema: p.schema, defaultSchema: p.defaultSchema, upperCaseKeywords: true });
}

/** Start offsets of console statements: mongosh lines beginning with db., show or use; DynamoDB
 * requests beginning with an operation name. */
function statementStarts(doc: string, kind: DbKind): number[] {
  const re =
    kind === "dynamodb"
      ? /^\s*[A-Z][A-Za-z]{3,}\b/
      : kind === "firestore"
        ? /^\s*(get|list|query|count|collections|set|update|add|delete)\b/i
        : kind === "elasticsearch"
          ? /^\s*(GET|POST|PUT|DELETE|HEAD|PATCH)\s+\S/
        : /^\s*(db\b|show\s|use\s)/;
  const starts = [0];
  let offset = 0;
  for (const line of doc.split("\n")) {
    if (offset > 0 && re.test(line)) starts.push(offset);
    offset += line.length + 1;
  }
  return starts;
}

export const SqlEditor = forwardRef<SqlEditorHandle, SqlEditorProps>(function SqlEditor(props, ref) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const language = useRef(new Compartment());
  const editable = useRef(new Compartment());
  const cb = useRef(props);
  cb.current = props;

  useEffect(() => {
    const run = (fn?: () => void) => () => {
      fn?.();
      return true;
    };
    const state = EditorState.create({
      doc: props.value,
      extensions: [
        lineNumbers(),
        highlightActiveLineGutter(),
        history(),
        drawSelection(),
        indentOnInput(),
        bracketMatching(),
        closeBrackets(),
        autocompletion({ activateOnTyping: true, maxRenderedOptions: 60 }),
        highlightActiveLine(),
        highlightSelectionMatches(),
        syntaxHighlighting(highlight),
        EditorView.lineWrapping,
        placeholderExt(props.placeholder ?? ""),
        language.current.of(languageFor(props)),
        editable.current.of(EditorState.readOnly.of(!!props.readOnly)),
        Prec.highest(
          keymap.of([
            { key: "Mod-Enter", run: run(() => cb.current.onRun?.()) },
            { key: "Shift-Enter", run: run(() => cb.current.onRunCurrent?.()) },
            { key: "Mod-s", run: run(() => cb.current.onSave?.()), preventDefault: true },
          ]),
        ),
        keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...completionKeymap, indentWithTab]),
        EditorView.updateListener.of((u) => {
          if (u.docChanged) cb.current.onChange?.(u.state.doc.toString());
        }),
      ],
    });
    const v = new EditorView({ state, parent: host.current! });
    view.current = v;
    return () => v.destroy();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    view.current?.dispatch({
      effects: language.current.reconfigure(languageFor(props)),
    });
  }, [props.kind, props.schema, props.defaultSchema]);

  useEffect(() => {
    view.current?.dispatch({ effects: editable.current.reconfigure(EditorState.readOnly.of(!!props.readOnly)) });
  }, [props.readOnly]);

  // External value changes (e.g. loading a saved query).
  useEffect(() => {
    const v = view.current;
    if (v && v.state.doc.toString() !== props.value) v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: props.value } });
  }, [props.value]);

  useImperativeHandle(ref, () => ({
    selectionOrAll() {
      const v = view.current!;
      const sel = v.state.selection.main;
      return sel.empty ? v.state.doc.toString() : v.state.sliceDoc(sel.from, sel.to);
    },
    currentStatement() {
      const v = view.current!;
      const doc = v.state.doc.toString();
      const pos = v.state.selection.main.head;
      if (cb.current.kind && CONSOLE_KINDS.includes(cb.current.kind)) {
        const starts = statementStarts(doc, cb.current.kind);
        const i = Math.max(0, starts.findLastIndex((s) => s <= pos));
        return doc.slice(starts[i], starts[i + 1] ?? doc.length).trim();
      }
      // SQL Server: the GO batch under the cursor when it declares variables or holds a module or
      // block (those statements only work together); otherwise the statement inside the batch.
      let base = 0;
      let text = doc;
      if (cb.current.kind === "mssql") {
        const go = /^[ \t]*go[ \t]*(\d+)?[ \t]*$/gim;
        let from = 0;
        let m: RegExpExecArray | null;
        let to = doc.length;
        while ((m = go.exec(doc))) {
          if (m.index + m[0].length < pos) from = m.index + m[0].length;
          else {
            to = m.index;
            break;
          }
        }
        text = doc.slice(from, to);
        base = from;
        if (/\b(declare|begin|create\s+(or\s+alter\s+)?(proc|procedure|function|trigger|view)|alter\s+(proc|procedure|function|trigger|view))\b/i.test(text)) return text.trim();
      }
      // Naive split that respects quotes is enough to find the statement under the cursor.
      let start = 0;
      let inQuote: string | null = null;
      const bounds: [number, number][] = [];
      const at = pos - base;
      for (let i = 0; i < text.length; i++) {
        const ch = text[i];
        if (inQuote) {
          if (ch === inQuote) inQuote = null;
        } else if (ch === "'" || ch === '"' || ch === "`") inQuote = ch;
        else if (ch === ";") {
          bounds.push([start, i + 1]);
          start = i + 1;
        }
      }
      bounds.push([start, text.length]);
      const hit = bounds.find(([a, b]) => at >= a && at <= b && text.slice(a, b).trim()) ?? bounds.reverse().find(([a, b]) => a <= at && text.slice(a, b).trim());
      return hit ? text.slice(hit[0], hit[1]).trim() : "";
    },
    getValue: () => view.current!.state.doc.toString(),
    setValue(value: string) {
      const v = view.current!;
      v.dispatch({ changes: { from: 0, to: v.state.doc.length, insert: value } });
    },
    focus: () => view.current?.focus(),
  }));

  return <div ref={host} style={{ height: "100%", minHeight: props.minHeight ?? 0, display: "flex", flexDirection: "column" }} className="selectable" />;
});
