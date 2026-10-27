// Transport to the Rust core: Tauri IPC inside the desktop app, or the local dev HTTP API
// (`twinbase serve --dev-api`) when the UI runs in a normal browser.

import type { ApiError, CoreEvent } from "./types";

const isTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

function devConfig() {
  const params = new URLSearchParams(window.location.search);
  const fromUrl = params.get("token");
  if (fromUrl) sessionStorage.setItem("twinbase.token", fromUrl);
  const port = params.get("core") ?? sessionStorage.getItem("twinbase.core") ?? "47800";
  sessionStorage.setItem("twinbase.core", port);
  return { base: `http://127.0.0.1:${port}`, token: fromUrl ?? sessionStorage.getItem("twinbase.token") ?? "" };
}

export class CoreCallError extends Error {
  readonly error: ApiError;
  constructor(error: ApiError) {
    super(error.message);
    this.error = error;
  }
}

/** Call a core API method. Throws `CoreCallError` with the core's error details. */
export async function call<T = unknown>(method: string, params: object = {}): Promise<T> {
  if (isTauri) {
    const { invoke } = await import("@tauri-apps/api/core");
    try {
      return await invoke<T>("api", { method, params });
    } catch (e) {
      throw new CoreCallError(typeof e === "object" && e && "message" in e ? (e as ApiError) : { kind: "internal", message: String(e) });
    }
  }
  const { base, token } = devConfig();
  let res: Response;
  try {
    res = await fetch(`${base}/api/${method}`, {
      method: "POST",
      headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}` },
      body: JSON.stringify(params),
    });
  } catch {
    throw new CoreCallError({ kind: "unavailable", message: `TwinBase core is not reachable at ${base}. Run \`twinbase serve --dev-api\`.` });
  }
  if (res.status === 403) throw new CoreCallError({ kind: "forbidden", message: "Dev API refused the request: open the UI with ?token=<token printed by twinbase serve --dev-api>." });
  const body = (await res.json()) as { ok: boolean; result?: T; error?: ApiError };
  if (!body.ok) throw new CoreCallError(body.error ?? { kind: "internal", message: "unknown error" });
  return body.result as T;
}

/** Subscribe to core events. Returns an unsubscribe function. */
export function subscribe(handler: (e: CoreEvent) => void): () => void {
  if (isTauri) {
    let unlisten: (() => void) | undefined;
    let cancelled = false;
    import("@tauri-apps/api/event").then(({ listen }) =>
      listen<CoreEvent>("twinbase://event", (e) => handler(e.payload)).then((u) => {
        if (cancelled) u();
        else unlisten = u;
      }),
    );
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }
  const { base, token } = devConfig();
  const source = new EventSource(`${base}/api/events?token=${encodeURIComponent(token)}`);
  source.onmessage = (m) => {
    try {
      handler(JSON.parse(m.data) as CoreEvent);
    } catch {
      /* ignore malformed frames */
    }
  };
  return () => source.close();
}

export const runtime = { isTauri };

/** Native file pickers in the desktop app; a plain path prompt in the browser. */
export async function pickPath(opts: { save?: boolean; directory?: boolean; title: string; defaultPath?: string; extensions?: string[] }): Promise<string | null> {
  if (isTauri) {
    const dialog = await import("@tauri-apps/plugin-dialog");
    const filters = opts.extensions ? [{ name: opts.extensions.join(", "), extensions: opts.extensions }] : undefined;
    if (opts.save) return (await dialog.save({ title: opts.title, defaultPath: opts.defaultPath, filters })) ?? null;
    const r = await dialog.open({ title: opts.title, directory: opts.directory, multiple: false, defaultPath: opts.defaultPath, filters });
    return typeof r === "string" ? r : null;
  }
  return window.prompt(`${opts.title} (absolute path)`, opts.defaultPath ?? "");
}

export function errorMessage(e: unknown): string {
  if (e instanceof CoreCallError) {
    const { message, code, detail } = e.error;
    return [message, detail, code ? `(code ${code})` : ""].filter(Boolean).join("\n");
  }
  return e instanceof Error ? e.message : String(e);
}

/** Open a web link or an editor deep link in the system's default handler. */
export async function openExternal(url: string): Promise<void> {
  if (isTauri) await call("app.open_url", { url });
  else window.open(url, "_blank", "noopener,noreferrer");
}
