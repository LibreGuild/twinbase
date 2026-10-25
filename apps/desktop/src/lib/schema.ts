// Cached schema graphs per connection/schema, invalidated by schema-change events.

import { useEffect, useState } from "react";
import { call } from "./api";
import { useStore } from "./store";
import type { TableSchema } from "./types";

const cache = new Map<string, { version: number; promise: Promise<TableSchema[]> }>();

export function loadGraph(connection: string, schema: string, version: number): Promise<TableSchema[]> {
  const key = `${connection}|${schema}`;
  const hit = cache.get(key);
  if (hit && hit.version === version) return hit.promise;
  const promise = call<{ tables: TableSchema[] }>("db.graph", { connection, schema }).then((g) => g.tables);
  promise.catch(() => cache.delete(key));
  cache.set(key, { version, promise });
  return promise;
}

/** Tables (with columns and keys) of a schema, refreshed when the schema changes. */
export function useGraph(connection: string | undefined, schema: string | undefined) {
  const version = useStore((s) => (connection ? (s.schemaVersion[connection] ?? 0) : 0));
  const [tables, setTables] = useState<TableSchema[] | undefined>();
  const [error, setError] = useState<unknown>();
  useEffect(() => {
    if (!connection || schema === undefined) {
      setTables(undefined);
      return;
    }
    let live = true;
    loadGraph(connection, schema, version).then(
      (t) => live && (setTables(t), setError(undefined)),
      (e) => live && setError(e),
    );
    return () => {
      live = false;
    };
  }, [connection, schema, version]);
  return { tables, error };
}

/** CodeMirror completion schema: `{ table: [columns] }`. */
export function completionSchema(tables: TableSchema[] | undefined): Record<string, string[]> | undefined {
  if (!tables) return undefined;
  const out: Record<string, string[]> = {};
  for (const t of tables) out[t.name] = t.columns.map((c) => c.name);
  return out;
}
