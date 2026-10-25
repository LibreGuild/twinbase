import { Check, Copy, Database, Loader2, X } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { errorMessage } from "../lib/api";
import { copyText, kindClass } from "../lib/format";
import { useStore } from "../lib/store";
import type { DbKind } from "../lib/types";

export function Spinner({ size = 14 }: { size?: number }) {
  return <Loader2 size={size} className="spin" aria-label="Loading" />;
}

export function KindIcon({ kind, size = 14 }: { kind: DbKind; size?: number }) {
  return <Database size={size} className={kindClass(kind)} aria-hidden />;
}

/** Layers that close on Escape (dialogs, menus), innermost last: one Escape closes only the top one. */
const escapeLayers: Array<() => void> = [];
window.addEventListener("keydown", (e) => {
  // Editors and inputs that handle Escape themselves (autocomplete, cell editing) come first.
  if (e.key !== "Escape" || e.defaultPrevented || escapeLayers.length === 0) return;
  e.preventDefault();
  escapeLayers[escapeLayers.length - 1]();
});

/** Close on Escape while this component is the topmost layer. */
export function useEscape(onEscape: () => void) {
  const latest = useRef(onEscape);
  latest.current = onEscape;
  useEffect(() => {
    const layer = () => latest.current();
    escapeLayers.push(layer);
    return () => {
      const i = escapeLayers.lastIndexOf(layer);
      if (i >= 0) escapeLayers.splice(i, 1);
    };
  }, []);
}

export function Dialog(props: { title: ReactNode; onClose: () => void; children: ReactNode; footer?: ReactNode; wide?: boolean; icon?: ReactNode }) {
  const { onClose } = props;
  useEscape(onClose);
  return createPortal(
    <div className="overlay" onMouseDown={(e) => e.target === e.currentTarget && onClose()}>
      <div className={`dialog${props.wide ? " wide" : ""}`} role="dialog" aria-modal="true" aria-label={typeof props.title === "string" ? props.title : undefined}>
        <div className="dialog-head">
          {props.icon}
          <h3>{props.title}</h3>
          <button className="icon-btn" onClick={onClose} aria-label="Close" title="Close (Esc)">
            <X size={16} />
          </button>
        </div>
        <div className="dialog-body">{props.children}</div>
        {props.footer && <div className="dialog-foot">{props.footer}</div>}
      </div>
    </div>,
    document.body,
  );
}

export interface MenuItem {
  label: string;
  /** Second line in smaller text (e.g. a path). */
  sub?: string;
  icon?: ReactNode;
  danger?: boolean;
  disabled?: boolean;
  onClick: () => void;
}

export type MenuEntry = MenuItem | "sep";

export function ContextMenu({ x, y, items, onClose, header }: { x: number; y: number; items: MenuEntry[]; onClose: () => void; header?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ x, y });
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({ x: Math.max(4, Math.min(x, window.innerWidth - r.width - 6)), y: Math.max(4, Math.min(y, window.innerHeight - r.height - 6)) });
  }, [x, y]);
  useEscape(onClose);
  useEffect(() => {
    const close = (e: MouseEvent) => !ref.current?.contains(e.target as Node) && onClose();
    window.addEventListener("mousedown", close);
    window.addEventListener("blur", onClose);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("blur", onClose);
    };
  }, [onClose]);
  return createPortal(
    <div className="menu" ref={ref} style={{ left: pos.x, top: pos.y }} role="menu">
      {header && <div className="menu-head">{header}</div>}
      {items.map((it, i) =>
        it === "sep" ? (
          <div key={i} className="menu-sep" />
        ) : (
          <button
            key={i}
            role="menuitem"
            className={it.danger ? "danger" : ""}
            disabled={it.disabled}
            onClick={() => {
              onClose();
              it.onClick();
            }}
          >
            {it.icon}
            {it.sub ? (
              <span style={{ minWidth: 0, flex: 1 }}>
                <span className="ellipsis" style={{ display: "block" }}>
                  {it.label}
                </span>
                <span className="menu-sub">{it.sub}</span>
              </span>
            ) : (
              it.label
            )}
          </button>
        ),
      )}
    </div>,
    document.body,
  );
}

/** State + opener for a context menu. */
export function useContextMenu() {
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[]; header?: string } | null>(null);
  const open = useCallback(
    (
      e: { clientX: number; clientY: number; preventDefault(): void; stopPropagation?(): void },
      items: MenuEntry[],
      opts?: { anchor?: HTMLElement; header?: string },
    ) => {
      e.preventDefault();
      e.stopPropagation?.();
      // Menus opened from buttons drop down below them; context menus open at the pointer.
      const r = opts?.anchor?.getBoundingClientRect();
      setMenu({ x: r ? r.left : e.clientX, y: r ? r.bottom + 6 : e.clientY, items, header: opts?.header });
    },
    [],
  );
  const node = menu ? <ContextMenu {...menu} onClose={() => setMenu(null)} /> : null;
  return { open, node };
}

export function CopyButton({ text, label, small }: { text: string; label?: string; small?: boolean }) {
  const [done, setDone] = useState(false);
  return (
    <button
      className={label ? `btn${small ? " sm" : ""}` : "icon-btn"}
      title="Copy"
      aria-label={label ?? "Copy"}
      onClick={async () => {
        await copyText(text);
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}
    >
      {done ? <Check size={14} /> : <Copy size={14} />}
      {label}
    </button>
  );
}

export function CodeBlock({ code, maxHeight }: { code: string; maxHeight?: number }) {
  return (
    <div className="code-block" style={maxHeight ? { maxHeight } : undefined}>
      <span className="copy">
        <CopyButton text={code} />
      </span>
      {code}
    </div>
  );
}

export function Empty({ icon, title, children }: { icon?: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      {icon && <div className="icon">{icon}</div>}
      <div style={{ fontWeight: 600, color: "var(--text)" }}>{title}</div>
      {children}
    </div>
  );
}

export function ErrorBox({ error }: { error: unknown }) {
  if (!error) return null;
  return <div className="error-box">{typeof error === "string" ? error : errorMessage(error)}</div>;
}

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="field">
      <label>{label}</label>
      {children}
      {hint && <div className="hint">{hint}</div>}
    </div>
  );
}

/** Load data with an async function; reloads when `deps` change. */
export function useAsync<T>(fn: () => Promise<T>, deps: unknown[]) {
  const [state, setState] = useState<{ data?: T; error?: unknown; loading: boolean }>({ loading: true });
  const seq = useRef(0);
  const run = useCallback(async () => {
    const n = ++seq.current;
    setState((s) => ({ ...s, loading: true }));
    try {
      const data = await fn();
      if (n === seq.current) setState({ data, loading: false });
    } catch (error) {
      if (n === seq.current) setState({ error, loading: false });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
  useEffect(() => {
    run();
  }, [run]);
  return { ...state, reload: run, setData: (data: T) => setState({ data, loading: false }) };
}

/** Drag handle that resizes a CSS variable length (px) on an element. */
export function Resizer({ axis, target, variable, min, max, invert }: { axis: "x" | "y"; target: () => HTMLElement | null; variable: string; min: number; max: number; invert?: boolean }) {
  const [active, setActive] = useState(false);
  return (
    <div
      className={`resizer-${axis}${active ? " active" : ""}`}
      role="separator"
      aria-orientation={axis === "x" ? "vertical" : "horizontal"}
      onMouseDown={(e) => {
        const el = target();
        if (!el) return;
        e.preventDefault();
        const start = axis === "x" ? e.clientX : e.clientY;
        const base = axis === "x" ? el.getBoundingClientRect().width : el.getBoundingClientRect().height;
        setActive(true);
        const move = (ev: MouseEvent) => {
          const delta = (axis === "x" ? ev.clientX : ev.clientY) - start;
          const size = Math.max(min, Math.min(max, base + (invert ? -delta : delta)));
          el.style.setProperty(variable, `${size}px`);
        };
        const up = () => {
          setActive(false);
          window.removeEventListener("mousemove", move);
          window.removeEventListener("mouseup", up);
          try {
            localStorage.setItem(`twinbase.size.${variable}`, el.style.getPropertyValue(variable));
          } catch {
            /* storage unavailable */
          }
        };
        window.addEventListener("mousemove", move);
        window.addEventListener("mouseup", up);
      }}
    />
  );
}

export function restoredSize(variable: string): string | undefined {
  try {
    return localStorage.getItem(`twinbase.size.${variable}`) ?? undefined;
  } catch {
    return undefined;
  }
}

export function Toasts() {
  const toasts = useStore((s) => s.toasts);
  const dismiss = useStore((s) => s.dismissToast);
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`}>
          <div className="t-body">
            <div className="t-title">{t.title}</div>
            {t.body && <div className="muted">{t.body}</div>}
          </div>
          <button className="icon-btn sm" onClick={() => dismiss(t.id)} aria-label="Dismiss">
            <X size={13} />
          </button>
        </div>
      ))}
    </div>
  );
}

export function ConfirmDialog(props: { title: string; body: string; confirmLabel: string; danger?: boolean; onConfirm: () => void | Promise<void>; onClose: () => void }) {
  const [busy, setBusy] = useState(false);
  return (
    <Dialog
      title={props.title}
      onClose={props.onClose}
      footer={
        <>
          <button className="btn" onClick={props.onClose}>
            Cancel
          </button>
          <button
            className={`btn ${props.danger ? "danger" : "primary"}`}
            disabled={busy}
            autoFocus
            onClick={async () => {
              setBusy(true);
              try {
                await props.onConfirm();
              } finally {
                setBusy(false);
                props.onClose();
              }
            }}
          >
            {busy && <Spinner />}
            {props.confirmLabel}
          </button>
        </>
      }
    >
      <p className="muted" style={{ margin: 0, lineHeight: 1.55, whiteSpace: "pre-wrap" }}>
        {props.body}
      </p>
    </Dialog>
  );
}
