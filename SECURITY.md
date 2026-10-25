# Security policy

## Supported versions
Security fixes go into the latest release. Please update to the
[latest release](https://github.com/LibreGuild/twinbase/releases/latest) before reporting.

## Reporting a vulnerability
**Please don't open a public issue, discussion or pull request for a security problem.**

Report it privately on GitHub: [**Security → Report a vulnerability**](https://github.com/LibreGuild/twinbase/security/advisories/new).
Only the maintainers can see it.

Please include:
- what an attacker can do, and what they need first (for example a shared bundle, a client that can
  reach a mock server, a malicious database server, or an AI agent with MCP access)
- the steps or a proof of concept to reproduce it
- the TwinBase version (Settings shows it, or `twinbase --version`) and your operating system

## What happens next
- We acknowledge the report within 7 days and keep you updated as we investigate.
- Once it is fixed, we publish a release and a security advisory. We credit you unless you prefer
  not to be named.
- Please give us a reasonable time to ship the fix before you disclose it publicly.

## What counts
TwinBase handles untrusted input on purpose, so these are especially relevant:
- **AI agents (MCP):** an agent writing through a connection that is read-only for agents, reaching
  the computer (files, programs) through TwinBase, reading settings, tokens or passwords, or another
  local program using the MCP endpoint without its token.
- **Bundles and workspaces** (they are shared and kept in Git): reading or writing files outside the
  workspace, running code, or leaking secrets when one is opened, imported or started.
- **Mock servers:** a client that crashes TwinBase, exhausts memory without bound, gets past a mock's
  password, or reads data it should not.
- **Database servers:** a malicious server or response that crashes the app or makes it run
  something it should not.
- **Secrets:** passwords, keys or tokens ending up in workspace files, logs, history, bundles or
  agent responses.

Not in scope: problems that need an attacker who already controls your user account, and mocks you
deliberately expose on your network without a password (the mock dialog warns about this).
