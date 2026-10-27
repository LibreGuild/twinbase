# Desktop app guide

## Start page and workspaces

On first launch TwinBase asks how to start: open the **default workspace**, **create a new
workspace** (name + location — a `.twinbase` folder inside your code repository is a good choice, so
the team can commit it to GitHub), or **open an existing workspace** folder. The workspace name in the top bar
opens a menu to switch between recent workspaces, create or open one, import a bundle, or
bring the start page back.

## Layout

- **Top bar:** it replaces the system title bar. On macOS the window buttons sit at its left end,
  centred in the bar; on Windows and Linux TwinBase draws its own minimise, maximise and close
  buttons at the right end. Next come the logo and the workspace switcher; on the right, search
  (⌘/Ctrl+K: tables, mocks, connections, saved queries and actions), the **mock servers** pill
  (start/stop any mock from its menu), the **AI agents** status, the Activity toggle, the light/dark
  switch and **Settings**. Empty space in the bar drags the window; double-click maximises it.
- **Icon rail** (left): Home, **Databases** (mock servers and connections, expandable to schemas
  and tables), **Saved queries** (collections), **Schema snapshots and diff**, and at the bottom the
  **Bootcamp**.
- **Tabs** (the first row of the rounded work panel): Home, tables, SQL editors, diagrams, Redis
  browsers and consoles, mock servers, collections, docs, lessons and schema comparisons. The strip
  scrolls sideways with a trackpad, the mouse wheel or its scrollbar; **+** at the right end opens a
  new query. A dot marks
  unsaved changes; closing such a tab asks first.
- **Activity** (bottom of the panel, ⌘/Ctrl+J): every change made by you, AI agents and the CLI,
  with the SQL and errors. Filter by *AI agents* or *Errors*; click a statement to open it in an
  editor. The top-bar icon shows a badge for unseen agent actions.

## Settings

Settings open in a pop-up (gear icon or ⌘/Ctrl+,) with sections for Appearance, Data & safety, AI
agents (MCP), Workspace and About.

## Tables

Data, Structure (columns, foreign keys, "referenced by", indexes) and DDL (in any dialect). Edit
cells in place (double-click, Enter or just type), stage new/duplicated/deleted rows, then Review
(SQL + optional preview on a local twin) or Save. The inspector (panel icon) shows full values and
the whole row. Toolbar: open a query, the diagram, generate sample rows, or edit the table design.

## SQL editor

Pick a connection, write SQL with schema-aware completion, run all/selection (⌘/Ctrl+Enter) or the
current statement (Shift+Enter). **Preview** runs writes on a local twin and shows the diff;
**Commit** applies them in one transaction guarded by the previewed row count. **Save as…** stores
the query with Markdown documentation, tags and an optional expectation.

## Diagram and designer

The ER diagram lays tables out automatically (positions you drag are remembered). Search dims
non-matching tables; *Keys only* hides other columns. Drag from a key's right dot onto another
table's column to create a foreign key. **+ Table** opens the designer: columns, types (with
suggestions per dialect), primary key, auto-increment, nullability, unique, defaults and references.
For existing tables the designer generates `ALTER` statements and explains changes it cannot make.
**DDL** converts the schema to PostgreSQL, MySQL or SQLite.

## Templates

Home → *Prototype from a template* creates a PostgreSQL or MySQL mock with related tables (blog,
e-commerce, multi-tenant SaaS, task tracker), fills them with realistic sample data, saves the seed
and opens the diagram.

## Redis

Expand a Redis connection or mock in the sidebar to see its numbered databases (with key counts) and
a **Console**. The key browser has a database picker, a **Tree** view that groups keys by `:`
namespaces (or a flat **List**), pattern and type filters, typed editors for strings, hashes, lists,
sets, sorted sets and RedisJSON documents, a stream viewer, TTL controls, a **Server** panel with
every `INFO` section, and an inline console with history (↑/↓). The Console tab runs a script of
commands line by line (⌘/Ctrl+Enter runs the current line, ⇧ runs all) with timings. Commands that
would break the shared connection (`SELECT`, `MULTI`, `SUBSCRIBE`, `MONITOR`) are refused with an
explanation.

## Mock servers

Connect (snippets for many languages and frameworks), Traffic (live statements per client), Faults,
Seed (edit, capture current data, reset) and Docs.

## Documentation

Everything can carry Markdown docs, rendered with GitHub-flavoured Markdown (tables, task lists) and
**Mermaid** diagrams in ```` ```mermaid ```` blocks. Every editor has Write, Preview and Split modes.

- **Workspace:** Home → *About this workspace* (saved as `README.md` in the workspace folder, so it
  also shows on GitHub).
- **Connections and mock servers:** right-click → **Docs**, or a mock's **Docs** tab.
- **Tables, collections, indexes:** a table's **Docs** tab documents the table and each column;
  other objects have **Docs** in their right-click menu.
- **Saved queries and collections:** in the save dialog and the collection page.

Docs are plain files in the workspace (see [workspace-format.md](workspace-format.md)), travel with
bundles, and are readable and writable by AI agents (`get_docs` / `set_docs`).

## Bootcamp

The graduation cap at the bottom of the rail opens the **Bootcamp**: ten chapters of short lessons
on databases and system design — foundations, SQL, data modeling, indexes, transactions, storage
internals, Redis caching, documents and key-value stores, scaling, and vectors/embeddings — with
interactive illustrations, quizzes (multiple choice, select-all, true/false, fill in the blank,
match the pairs, put in order) and hands-on labs on real mock servers. Lessons earn XP, levels,
streaks and badges. The Bootcamp runs in a workspace of its own, so its lab servers never touch your
projects; **Back to my workspace** returns. See [bootcamp.md](bootcamp.md).

## Keyboard shortcuts

| Keys | Action |
| --- | --- |
| ⌘/Ctrl+K | Search: tables, mocks, connections, saved queries, actions |
| ⌘/Ctrl+T | New SQL query |
| ⌘/Ctrl+W | Close tab (desktop app) |
| ⌘/Ctrl+1…9 | Switch to tab |
| ⌘/Ctrl+J | Toggle Activity |
| ⌘/Ctrl+, | Settings |
| ⌘/Ctrl+Enter / Shift+Enter | Run all or selection / run current statement |
| ⌘/Ctrl+S | Save query |
| ⌘/Ctrl+Enter (Redis console, labs) | Run the current line / Run |

## Themes and fonts

Light, dark (a warm, dark grey-brown — never pure black) or follow the system, with a teal
(`#55A9A0`) accent. Settings → Appearance offers bundled interface fonts — **Inter** (default),
**Atkinson Hyperlegible** (designed for readability), **IBM Plex Sans** or the system font — and code
fonts **JetBrains Mono** (default), **Fira Code** or the system monospace, plus text size. Fonts are
bundled with the app; nothing is loaded from the internet.
