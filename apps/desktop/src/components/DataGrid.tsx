// Virtualized data grid (rows and columns): only visible cells are rendered, so large results stay
// light. Editing is optional and reported to the owner, which stages the changes.

import { ArrowDown, ArrowUp, KeyRound, Link2 } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import { cellClass, cellPreview, cellText } from "../lib/format";
import type { Cell, SortKey } from "../lib/types";

export interface GridColumn {
  name: string;
  type: string;
  pk?: boolean;
  fk?: string;
}

export type RowState = "added" | "deleted" | undefined;

export interface GridProps {
  columns: GridColumn[];
  rows: Cell[][];
  rowStates?: RowState[];
  /** `${row}:${col}` of cells with staged edits. */
  edited?: Set<string>;
  /** `${row}:${col}` of new-row cells left to the column default. */
  defaults?: Set<string>;
  editable?: boolean;
  rowNumberOffset?: number;
  sort?: SortKey[];
  onSort?: (column: string, additive: boolean) => void;
  onEdit?: (row: number, col: number, value: Cell) => void;
  onSelect?: (cell: { row: number; col: number } | null) => void;
  onContextMenu?: (e: ReactMouseEvent, row: number, col: number) => void;
  /** Persist column widths under this key. */
  widthKey?: string;
}

const ROW_H = 28;
const HEAD_H = 30;
const GUTTER = 56;
const OVERSCAN = 6;

function initialWidths(columns: GridColumn[], rows: Cell[][]): number[] {
  return columns.map((c, i) => {
    // Header: icon + bold name + type label; values: monospace text.
    const header = (c.pk || c.fk ? 16 : 0) + c.name.length * 7.6 + Math.min(c.type.length, 14) * 6.2 + 30;
    let longest = 0;
    for (let r = 0; r < Math.min(rows.length, 60); r++) {
      const v = rows[r][i];
      const l = v === null ? 4 : Math.min(String(v).length, 60);
      if (l > longest) longest = l;
    }
    return Math.round(Math.max(70, Math.min(360, Math.max(header, longest * 7.3 + 20))));
  });
}

function parseInput(text: string, type: string, original: Cell): Cell {
  const t = type.toLowerCase();
  if (typeof original === "number" || /^(int|bigint|smallint|tinyint|mediumint|serial|bigserial|float|double|real|numeric|decimal)|int[248]$|float[48]$/.test(t)) {
    if (/^-?\d+(\.\d+)?(e[+-]?\d+)?$/i.test(text.trim()) && Number.isFinite(Number(text)) && Math.abs(Number(text)) <= Number.MAX_SAFE_INTEGER) return Number(text);
    return text;
  }
  if (typeof original === "boolean" || t === "bool" || t === "boolean") {
    if (/^(true|t|1|yes)$/i.test(text.trim())) return true;
    if (/^(false|f|0|no)$/i.test(text.trim())) return false;
  }
  return text;
}

export function DataGrid(props: GridProps) {
  const { columns, rows } = props;
  const scrollRef = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ top: 0, left: 0, width: 800, height: 400 });
  const [widths, setWidths] = useState<number[]>(() => initialWidths(columns, rows));
  const [sel, setSel] = useState<{ row: number; col: number } | null>(null);
  const [editing, setEditing] = useState<{ row: number; col: number; text: string } | null>(null);
  const colSignature = columns.map((c) => c.name).join("\u0001");

  // Recompute widths when the column set changes (keep user-resized widths by name).
  useEffect(() => {
    let saved: Record<string, number> = {};
    try {
      if (props.widthKey) saved = JSON.parse(localStorage.getItem(`twinbase.widths.${props.widthKey}`) ?? "{}");
    } catch {
      /* ignore */
    }
    const auto = initialWidths(columns, rows);
    setWidths(columns.map((c, i) => saved[c.name] ?? auto[i]));
    setSel((s) => (s && s.col < columns.length && s.row < rows.length ? s : null));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [colSignature, props.widthKey]);

  useEffect(() => {
    if (sel && sel.row >= rows.length) setSel(rows.length ? { row: rows.length - 1, col: sel.col } : null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rows.length]);

  const offsets = useMemo(() => {
    const o = [0];
    for (const w of widths) o.push(o[o.length - 1] + w);
    return o;
  }, [widths]);
  const totalW = offsets[offsets.length - 1] ?? 0;

  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    const measure = () => setView((v) => ({ ...v, width: el.clientWidth, height: el.clientHeight }));
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const raf = useRef(0);
  const onScroll = () => {
    cancelAnimationFrame(raf.current);
    raf.current = requestAnimationFrame(() => {
      const el = scrollRef.current;
      if (el) setView((v) => ({ ...v, top: el.scrollTop, left: el.scrollLeft }));
    });
  };

  const firstRow = Math.max(0, Math.floor(view.top / ROW_H) - OVERSCAN);
  const lastRow = Math.min(rows.length, Math.ceil((view.top + view.height) / ROW_H) + OVERSCAN);
  let firstCol = 0;
  while (firstCol < columns.length && offsets[firstCol + 1] < view.left - GUTTER) firstCol++;
  let lastCol = firstCol;
  while (lastCol < columns.length && offsets[lastCol] < view.left + view.width) lastCol++;
  firstCol = Math.max(0, firstCol - 1);
  lastCol = Math.min(columns.length, lastCol + 1);

  const select = useCallback(
    (cell: { row: number; col: number } | null) => {
      setSel(cell);
      props.onSelect?.(cell);
    },
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [props.onSelect],
  );

  const ensureVisible = (row: number, col: number) => {
    const el = scrollRef.current;
    if (!el) return;
    const top = row * ROW_H;
    if (top < el.scrollTop) el.scrollTop = top;
    else if (top + ROW_H > el.scrollTop + el.clientHeight - HEAD_H) el.scrollTop = top + ROW_H - el.clientHeight + HEAD_H;
    const left = offsets[col];
    const right = offsets[col + 1];
    if (left < el.scrollLeft) el.scrollLeft = left;
    else if (right + GUTTER > el.scrollLeft + el.clientWidth) el.scrollLeft = right + GUTTER - el.clientWidth;
  };

  const canEdit = (row: number) => !!props.editable && props.rowStates?.[row] !== "deleted";

  const startEdit = (row: number, col: number, initial?: string) => {
    if (!canEdit(row)) return;
    const v = rows[row][col];
    setEditing({ row, col, text: initial ?? (v === null ? "" : cellText(v)) });
  };

  const commitEdit = (moveDown?: boolean) => {
    if (!editing) return;
    const { row, col, text } = editing;
    const original = rows[row][col];
    const value = parseInput(text, columns[col].type, original);
    if (value !== original && !(original === null && text === "")) props.onEdit?.(row, col, value);
    setEditing(null);
    scrollRef.current?.focus();
    if (moveDown && row + 1 < rows.length) {
      select({ row: row + 1, col });
      ensureVisible(row + 1, col);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (editing || !rows.length || !columns.length) return;
    const cur = sel ?? { row: 0, col: 0 };
    let { row, col } = cur;
    const page = Math.max(1, Math.floor((view.height - HEAD_H) / ROW_H) - 1);
    switch (e.key) {
      case "ArrowDown":
        row = Math.min(rows.length - 1, row + (e.metaKey || e.ctrlKey ? rows.length : 1));
        break;
      case "ArrowUp":
        row = Math.max(0, row - (e.metaKey || e.ctrlKey ? rows.length : 1));
        break;
      case "ArrowRight":
      case "Tab":
        if (e.key === "Tab" && e.shiftKey) col = Math.max(0, col - 1);
        else col = Math.min(columns.length - 1, col + (e.metaKey || e.ctrlKey ? columns.length : 1));
        break;
      case "ArrowLeft":
        col = Math.max(0, col - (e.metaKey || e.ctrlKey ? columns.length : 1));
        break;
      case "PageDown":
        row = Math.min(rows.length - 1, row + page);
        break;
      case "PageUp":
        row = Math.max(0, row - page);
        break;
      case "Home":
        col = 0;
        break;
      case "End":
        col = columns.length - 1;
        break;
      case "Enter":
      case "F2":
        if (sel) startEdit(sel.row, sel.col);
        e.preventDefault();
        return;
      case "c":
        if ((e.metaKey || e.ctrlKey) && sel) {
          navigator.clipboard.writeText(cellText(rows[sel.row][sel.col]));
          e.preventDefault();
        }
        return;
      default:
        if (sel && e.key.length === 1 && !e.metaKey && !e.ctrlKey && !e.altKey && canEdit(sel.row)) {
          startEdit(sel.row, sel.col, e.key);
          e.preventDefault();
        }
        return;
    }
    e.preventDefault();
    select({ row, col });
    ensureVisible(row, col);
  };

  const sortOf = (name: string) => props.sort?.find((s) => s.column === name);

  const startResize = (e: ReactMouseEvent, col: number) => {
    e.preventDefault();
    e.stopPropagation();
    const start = e.clientX;
    const base = widths[col];
    const move = (ev: MouseEvent) => {
      const w = Math.max(50, Math.min(1200, base + ev.clientX - start));
      setWidths((ws) => ws.map((x, i) => (i === col ? w : x)));
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      if (props.widthKey) {
        setWidths((ws) => {
          try {
            const saved = JSON.parse(localStorage.getItem(`twinbase.widths.${props.widthKey}`) ?? "{}");
            saved[columns[col].name] = ws[col];
            localStorage.setItem(`twinbase.widths.${props.widthKey}`, JSON.stringify(saved));
          } catch {
            /* ignore */
          }
          return ws;
        });
      }
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  const visibleCols: number[] = [];
  for (let c = firstCol; c < lastCol; c++) visibleCols.push(c);
  const gutterDigits = String((props.rowNumberOffset ?? 0) + rows.length).length;
  const gutterW = Math.max(GUTTER, gutterDigits * 8 + 20);

  return (
    <div className="grid-scroll" ref={scrollRef} tabIndex={0} onScroll={onScroll} onKeyDown={onKeyDown} role="grid" aria-rowcount={rows.length} aria-colcount={columns.length}>
      <div style={{ width: totalW + gutterW, height: HEAD_H + rows.length * ROW_H, position: "relative" }}>
        <div className="grid-header" style={{ width: totalW + gutterW }}>
          <div className="grid-gutter" style={{ width: gutterW, height: HEAD_H, position: "sticky", left: 0, zIndex: 4 }} />
          {visibleCols.map((c) => {
            const col = columns[c];
            const s = sortOf(col.name);
            return (
              <div
                key={c}
                className="grid-hcell"
                role="columnheader"
                style={{ left: gutterW + offsets[c], width: widths[c] }}
                title={`${col.name} · ${col.type}${col.fk ? ` → ${col.fk}` : ""}${props.onSort ? "\nClick to sort, Shift+click to add" : ""}`}
                onClick={(e) => props.onSort?.(col.name, e.shiftKey)}
              >
                {col.pk && <KeyRound size={12} style={{ color: "var(--warning)", flex: "none" }} />}
                {col.fk && <Link2 size={12} style={{ color: "var(--info)", flex: "none" }} />}
                <span className="hname">{col.name}</span>
                <span className="htype">{col.type}</span>
                {s && (s.descending ? <ArrowDown size={12} style={{ flex: "none" }} /> : <ArrowUp size={12} style={{ flex: "none" }} />)}
                <span className="resize" onMouseDown={(e) => startResize(e, c)} onClick={(e) => e.stopPropagation()} />
              </div>
            );
          })}
        </div>
        {rows.slice(firstRow, lastRow).map((row, i) => {
          const r = firstRow + i;
          const state = props.rowStates?.[r];
          return (
            <div key={r} className={`grid-row${state ? ` ${state}` : ""}`} role="row" style={{ top: HEAD_H + r * ROW_H, height: ROW_H, width: totalW + gutterW }}>
              <div className="grid-gutter cell" style={{ width: gutterW, height: ROW_H }}>
                {state === "added" ? "new" : (props.rowNumberOffset ?? 0) + r + 1}
              </div>
              {visibleCols.map((c) => {
                const v = row[c];
                const isDefault = props.defaults?.has(`${r}:${c}`);
                const cls = isDefault ? "null" : cellClass(v, columns[c].type);
                const text = isDefault ? "DEFAULT" : cellPreview(v);
                const isSel = sel?.row === r && sel.col === c;
                return (
                  <div
                    key={c}
                    role="gridcell"
                    className={`grid-cell ${cls}${isSel ? " selected" : ""}${props.edited?.has(`${r}:${c}`) ? " edited" : ""}`}
                    style={{ left: gutterW + offsets[c], width: widths[c] }}
                    title={text.length > 24 ? text.slice(0, 600) : undefined}
                    onMouseDown={() => select({ row: r, col: c })}
                    onDoubleClick={() => startEdit(r, c)}
                    onContextMenu={(e) => {
                      select({ row: r, col: c });
                      props.onContextMenu?.(e, r, c);
                    }}
                  >
                    <span>{text}</span>
                  </div>
                );
              })}
            </div>
          );
        })}
        {editing && (
          <input
            className="grid-edit"
            autoFocus
            aria-label={`Edit ${columns[editing.col].name}`}
            style={{ top: HEAD_H + editing.row * ROW_H, left: gutterW + offsets[editing.col], width: Math.max(widths[editing.col], 160), height: ROW_H }}
            value={editing.text}
            onChange={(e) => setEditing({ ...editing, text: e.target.value })}
            onBlur={() => commitEdit()}
            onKeyDown={(e) => {
              if (e.key === "Enter") commitEdit(true);
              else if (e.key === "Escape") {
                setEditing(null);
                scrollRef.current?.focus();
              }
              e.stopPropagation();
            }}
          />
        )}
      </div>
    </div>
  );
}
