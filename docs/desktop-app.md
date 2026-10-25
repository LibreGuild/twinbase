# Desktop app guide

## Start page and workspaces

On first launch TwinBase asks how to start: open the **default workspace**, **create a new
workspace** (name + location — a `.twinbase` folder inside your code repository is a good choice, so
the team can commit it to GitHub), or **open an existing workspace** folder. The workspace name in the
top bar opens a menu to switch between recent workspaces, create or open one, import a bundle, or
bring the start page back.

## Layout

- **Top bar:** logo and workspace switcher, a search field (⌘/Ctrl+K) that jumps to tables, mocks,
  connections, saved queries and actions, a **mock servers** pill (start/stop any mock from its
  menu), the **AI agents** status (connected agents, or a warning when MCP is unavailable), the
  Activity toggle, the light/dark switch and **Settings**. On macOS the bar replaces the window title
  bar; drag it to move the window.
- **Icon rail** (left): Home, **Databases** (mock servers and connections, expandable to schemas
  and tables), **Saved queries** (collections), and **Schema snapshots and diff**.
- **Tabs** (centre, in a rounded panel): Home, tables, SQL editors, diagrams, Redis browsers, mock
  servers, collections and schema comparisons. A dot marks unsaved changes; closing such a tab asks
  first.
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

Key list with pattern and type filters, typed editors for strings, hashes, lists, sets and sorted
sets, TTL controls, and a console with history (↑/↓).

## Mock servers

Connect (snippets for many languages and frameworks), Traffic (live statements per client) and Seed
(edit, capture current data, reset).

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

## Themes and fonts

Light, dark (a warm, dark grey-brown — never pure black) or follow the system, with a teal
(`#55A9A0`) accent. Settings → Appearance offers bundled interface fonts — **Inter** (default),
**Atkinson Hyperlegible** (designed for readability), **IBM Plex Sans** or the system font — and code
fonts **JetBrains Mono** (default), **Fira Code** or the system monospace, plus text size. Fonts are
bundled with the app; nothing is loaded from the internet.
