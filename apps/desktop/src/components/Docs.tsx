// Documentation editors: the workspace README, a connection or mock server, and the objects inside
// them (tables, collections, indexes, key patterns) with per-column notes. Everything is Markdown
// stored in the workspace (see crates/core/src/docs.rs).

import { BookOpen, Columns3, Plus, Save, Trash2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { call } from "../lib/api";
import { attempt, targetInfo, useStore } from "../lib/store";
import { DocsEditor, Markdown } from "./Markdown";
import { ErrorBox, Spinner, useAsync } from "./ui";

export interface ObjectDocs {
  docs: string;
  columns: Record<string, string>;
}

/** Save-able copy of a value: tracks the saved state so Save is only offered for real changes. */
function useDraft<T>(saved: T | undefined) {
  const [draft, setDraft] = useState<T | undefined>(saved);
  const [base, setBase] = useState<string>(JSON.stringify(saved));
  useEffect(() => {
    setDraft(saved);
    setBase(JSON.stringify(saved));
  }, [saved]);
  return { draft, setDraft, dirty: draft !== undefined && JSON.stringify(draft) !== base, markSaved: () => setBase(JSON.stringify(draft)) };
}

/** README of the workspace (target omitted) or docs of a connection / mock server. */
export function TargetDocs({ target, compact }: { target?: string; compact?: boolean }) {
  const toast = useStore((s) => s.toast);
  const res = useAsync(() => call<{ docs: string }>("docs.get", target ? { target } : {}).then((r) => r.docs), [target]);
  const { draft, setDraft, dirty, markSaved } = useDraft(res.data);
  const [busy, setBusy] = useState(false);
  if (res.loading && draft === undefined) return <Spinner />;
  if (res.error) return <ErrorBox error={res.error} />;
  const save = async () => {
    setBusy(true);
    const ok = await attempt("Could not save the docs", () => call("docs.set", { ...(target ? { target } : {}), docs: draft ?? "" }));
    setBusy(false);
    if (ok) {
      markSaved();
      toast("success", "Docs saved");
    }
  };
  return (
    <div className="docs-block">
      <DocsEditor
        value={draft ?? ""}
        onChange={setDraft}
        rows={compact ? 6 : 12}
        placeholder={target ? "What this database is for, who owns it, conventions, gotchas… Markdown and ```mermaid diagrams work." : "# Workspace\nWhat this workspace is for, how the mocks map to real systems, onboarding notes…"}
      />
      <div className="row" style={{ marginTop: 8 }}>
        <span className="faint xs">{target ? "Stored with the connection in the workspace." : "Saved as README.md in the workspace folder."}</span>
        <span className="grow" />
        {dirty && (
          <button className="btn sm ghost" onClick={() => setDraft(res.data)}>
            Discard
          </button>
        )}
        <button className="btn sm primary" disabled={!dirty || busy} onClick={save}>
          {busy ? <Spinner size={12} /> : <Save size={13} />} Save
        </button>
      </div>
    </div>
  );
}

/** Docs of one object and its columns or fields. `columns` lists the known ones (with types). */
export function ObjectDocsEditor({ target, object, columns, columnLabel = "Columns" }: { target: string; object: string; columns?: { name: string; type?: string }[]; columnLabel?: string }) {
  const toast = useStore((s) => s.toast);
  const res = useAsync(() => call<Partial<ObjectDocs>>("docs.get", { target, object }).then((d) => ({ docs: d.docs ?? "", columns: d.columns ?? {} })), [target, object]);
  const { draft, setDraft, dirty, markSaved } = useDraft<ObjectDocs>(res.data);
  const [busy, setBusy] = useState(false);
  const [extra, setExtra] = useState("");
  const rows = useMemo(() => {
    const known = columns ?? [];
    const names = new Set(known.map((c) => c.name));
    const documented = Object.keys(draft?.columns ?? {}).filter((n) => !names.has(n)).map((name) => ({ name, type: undefined as string | undefined, missing: !!columns }));
    return [...known.map((c) => ({ ...c, missing: false })), ...documented];
  }, [columns, draft?.columns]);
  if (res.loading && !draft) return <Spinner />;
  if (res.error) return <ErrorBox error={res.error} />;
  if (!draft) return null;
  const setColumn = (name: string, text: string) => setDraft({ ...draft, columns: { ...draft.columns, [name]: text } });
  const removeColumn = (name: string) => {
    const c = { ...draft.columns };
    delete c[name];
    setDraft({ ...draft, columns: c });
  };
  const save = async () => {
    setBusy(true);
    const ok = await attempt("Could not save the docs", () => call("docs.set", { target, object, docs: draft.docs, columns: draft.columns }));
    setBusy(false);
    if (ok) {
      markSaved();
      toast("success", "Docs saved");
    }
  };
  return (
    <div className="docs-block">
      <h3 className="docs-h">
        <BookOpen size={14} /> About <span className="mono">{object}</span>
      </h3>
      <DocsEditor value={draft.docs} onChange={(docs) => setDraft({ ...draft, docs })} rows={8} placeholder="What one row means, who writes it, retention, gotchas… Markdown and ```mermaid diagrams work." />
      <h3 className="docs-h" style={{ marginTop: 18 }}>
        <Columns3 size={14} /> {columnLabel}
      </h3>
      <div className="col-docs">
        {rows.length === 0 && <div className="faint small">No {columnLabel.toLowerCase()} yet. Add one below.</div>}
        {rows.map((c) => (
          <div key={c.name} className="col-doc">
            <div className="col-doc-name">
              <span className="mono">{c.name}</span>
              {c.type && <span className="faint xs">{c.type}</span>}
              {c.missing && <span className="badge warning" title="Documented, but not in the current schema">gone</span>}
            </div>
            <textarea className="textarea" rows={1} value={draft.columns[c.name] ?? ""} placeholder="Meaning, units, allowed values…" onChange={(e) => setColumn(c.name, e.target.value)} />
            {c.missing || !columns ? (
              <button className="icon-btn" title="Remove" aria-label={`Remove docs of ${c.name}`} onClick={() => removeColumn(c.name)}>
                <Trash2 size={13} />
              </button>
            ) : (
              <span />
            )}
          </div>
        ))}
      </div>
      {!columns?.length && (
        <div className="row" style={{ marginTop: 8 }}>
          <input className="input sm mono" placeholder="field name" value={extra} onChange={(e) => setExtra(e.target.value)} onKeyDown={(e) => e.key === "Enter" && extra.trim() && (setColumn(extra.trim(), ""), setExtra(""))} />
          <button className="btn sm" disabled={!extra.trim()} onClick={() => (setColumn(extra.trim(), ""), setExtra(""))}>
            <Plus size={13} /> Add field
          </button>
        </div>
      )}
      <div className="row docs-save">
        <span className="faint xs">Saved in the workspace next to the {targetInfo(target)?.isMock ? "mock server" : "connection"}, so it travels with Git and bundles.</span>
        <span className="grow" />
        {dirty && (
          <button className="btn sm ghost" onClick={() => setDraft(res.data)}>
            Discard
          </button>
        )}
        <button className="btn sm primary" disabled={!dirty || busy} onClick={save}>
          {busy ? <Spinner size={12} /> : <Save size={13} />} Save docs
        </button>
      </div>
    </div>
  );
}

/** Docs tab: a connection or mock server, its documented objects, or one object. */
export function DocsView({ target, object }: { target?: string; object?: string }) {
  const openTab = useStore((s) => s.openTab);
  useStore((s) => s.workspace);
  const info = targetInfo(target);
  const list = useAsync(() => (target && !object ? call<{ objects: Record<string, ObjectDocs> }>("docs.list", { target }) : Promise.resolve(undefined)), [target, object]);
  const title = !target ? "Workspace README" : object ? object : `${info?.name ?? target}`;
  return (
    <div className="pane">
      <div className="page">
        <div className="docs-crumbs faint small">
          <button className="link" onClick={() => openTab({ kind: "docs" })}>Workspace</button>
          {target && (
            <>
              {" / "}
              <button className="link" onClick={() => openTab({ kind: "docs", target })}>{info?.name ?? target}</button>
            </>
          )}
          {object && <> / {object}</>}
        </div>
        <h1>{title}</h1>
        <p>{!target ? "The front page of this workspace, kept as README.md so it also shows on GitHub." : object ? "Notes on this object and its fields." : "Notes on this database, plus every object you have documented in it."}</p>
        {object && target ? <ObjectDocsEditor target={target} object={object} /> : <TargetDocs target={target} />}
        {target && !object && (
          <>
            <h2>Documented objects</h2>
            {list.loading && <Spinner />}
            {list.data && Object.keys(list.data.objects).length === 0 && <p className="faint">Nothing yet. Open a table or collection and use its Docs section, or right-click it in the sidebar → Docs.</p>}
            <div className="cards">
              {Object.entries(list.data?.objects ?? {}).map(([name, d]) => (
                <div key={name} className="card action docs-card" onClick={() => openTab({ kind: "docs", target, object: name })}>
                  <div style={{ minWidth: 0 }}>
                    <div className="card-title mono ellipsis">{name}</div>
                    <Markdown source={d.docs.split("\n").slice(0, 3).join("\n")} className="card-desc docs-excerpt" empty="No description" />
                    {Object.keys(d.columns ?? {}).length > 0 && <div className="faint xs" style={{ marginTop: 4 }}>{Object.keys(d.columns).length} documented fields</div>}
                  </div>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
