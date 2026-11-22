# Security

Tether exists to run agents you do not trust. That makes it important to be
exact about what it stops and what it does not.

## Reporting a vulnerability

Report it privately through GitHub: open the repository's
[Security tab](https://github.com/chizy7/tether/security) and choose
"Report a vulnerability". Please do not open a public issue.

Include the Tether version (`tether --version`), the platform, the sandbox
backend, and the smallest policy and command that show the problem.

Tether has one maintainer. Expect an acknowledgement within a week. A fix is
released as a new version and the report is credited in `CHANGELOG.md` unless
you ask otherwise.

Only the latest release receives fixes.

## What Tether enforces

Two separate things stand between an agent and the machine.

**Policy** decides whether a model call or tool call happens at all. Every
call an agent makes through Tether is checked against the rules and budgets in
`tether.toml` before it runs, and the decision is written to the trace. Policy
matches on what the agent asked for: the tool name and its arguments. It is as
good as its rules, and it only sees calls that go through Tether.

Where a rule could be wrong in two directions, a `deny` or an `ask` errs
toward matching and an `allow` toward not matching:

- A `deny` on a `path` or a command name ignores letter case, since many
  filesystems do: `SECRETS/` and `RM` are caught. An `allow` matches as
  written.
- A path that climbs out with `..` matches every `deny` and `ask` on a path,
  and no `allow`.
- A rule matches a command by its name. It does not see through `sh -c`,
  `env`, `xargs` or a renamed copy of the program. A policy that has to hold
  allows narrowly and denies by default; it does not list what to refuse.
- A dollar budget needs prices. A model with no price counts as $0, with a
  warning; use `max_tokens` for those.

**The sandbox** limits what a tool call can do once policy has allowed it.
How much it limits depends on the backend.

### `process` (the default): not a security boundary

Commands run as ordinary child processes, as the user running Tether. This
backend limits the damage of an ordinary mistake. It does not contain a
program that is trying to get out.

| | Linux | macOS |
| --- | --- | --- |
| Commands start in the workspace with only `PATH` in the environment | yes | yes |
| The `read_file`, `write_file` and `list_dir` tools refuse a path that leaves the workspace | yes | yes |
| Time limit, covering input and output too; the command's process group is killed when it returns or runs out of time | yes | yes |
| Output kept: the first 4 MiB each of what a command prints and of its errors; `read_file` reads at most 16 MiB | yes | yes |
| CPU time and open file limits | yes | yes |
| Memory limit | yes | no (refused at start) |
| Deny list of system calls, no privilege gain through setuid programs | yes | no |
| No internet sockets when the run has no network | yes | **no, and not reported** |

What it does not stop, on either platform:

- **A command can read and write any file the user can,** by absolute path.
  The workspace check covers the file tools, not what a command does.
- **The file tools follow symlinks.** A link inside the workspace that points
  outside it is read or written through.
- **A process that leaves its group is not killed.** One that calls `setsid`
  outlives the command and the run.
- **A command sees the same kernel, processes and devices** as the user.
- **On macOS a command can use the network** even when the run is configured
  with none.

If the agent can run commands and you would not run those commands yourself,
do not use this backend.

### `docker`: filesystem and network isolation, shared kernel

One container per run. The workspace is the only host path mounted. Commands
run as the host user with every capability dropped and no privilege gain.
With no network configured the container has loopback only. File reads and
writes go through the container, so a symlink planted in the workspace cannot
point at a host file.

What it does not stop:

- **The kernel is shared with the host.** A kernel or container runtime
  vulnerability is an escape.
- **The workspace is a real host directory.** Anything written there is on
  the host afterwards, with the user's ownership.
- **A network allowlist is not supported.** The choice is all or none, and a
  run that asks for an allowlist is refused at start.

### External MCP servers: outside the sandbox

A server named in `[[mcp_server]]` is not sandboxed by Tether. A stdio
server is a child process running as you, started with `PATH` and the
environment variables the policy's `env` list names; an HTTP server is
wherever its URL points. What its tools do, they do with that server's own
access.

Policy is the only thing between the agent and such a server: every call is
decided by a rule before it is sent, and recorded. Nothing limits what the
server does once a call is allowed. Only name servers you would run
yourself, and prefer a rule that allows the tools you need over one that
allows the whole server.

Calls to these servers are never made twice: replay gives back the recorded
result, and `resume` does not repeat them.

A trace records the policy it ran under, servers included, and that policy
could name any command. Tether never starts a server because a trace names
it. `tether replay` on its own starts nothing. `resume`, and a replay or
eval that goes live, start a server only from a `--policy` file you pass or
with `--trust-recorded-servers`, and print the command or URL first. Use
that flag for traces you made.

### `firecracker`: planned for v1.0

A separate kernel per run. It does not exist yet.

## Other things worth knowing

- **Going live on someone else's trace runs its commands.** `tether replay
  --live-after-divergence`, `tether eval --system` and `tether resume` bring
  a fresh workspace up to date by running the trace's earlier sandbox calls
  again, `exec` included, and under the `process` backend those are ordinary
  commands. Reading a trace (`inspect`, `verify`, `diff`, `ui`, a plain
  `replay`) runs nothing. Treat a trace from a stranger like a script from
  one.
- **A trace has one writer.** A run locks its trace while it writes. `tether
  resume` on a run that is still alive is refused; it used to interleave two
  runs in one file, which then never verified again.
- **A blob is checked every time it is read.** Its content must hash to its
  name and it must be a plain file, so a trace directory from someone else
  cannot point a blob at a file of yours and have a replay copy it out.
- **Credentials do not follow redirects.** Requests to a model provider, an
  HTTP MCP server and an OpenTelemetry endpoint go to the address given and
  no further.
- **The policy file is in every trace.** `run.created` records `tether.toml`
  whole, so anything written in it travels with the trace. Never put a token
  in an `[[mcp_server]]` header: write `${NAME}` and Tether fills it in from
  the environment when it connects.
- **Traces hold what the agent saw.** Model requests and responses and tool
  outputs are stored in the trace and its blobs, unencrypted. If a tool read a
  secret, the secret is in the trace. Treat traces like logs that may contain
  credentials, including when you pack and share one.
- **The hash chain detects accidents, not attackers.** `tether verify` shows
  that a trace has not been truncated or edited in place. Someone who can
  rewrite the whole file can rewrite the chain. It is not a signature.
- **The daemon trusts its socket.** `tetherd` listens on a Unix socket and
  does not authenticate clients. Anyone who can open the socket can start
  runs under their own policy. The socket file is created readable and
  writable by its owner only.
- **`tether ui` serves on 127.0.0.1 only** and checks the Host header. It has
  no login, so another local user who knows the port can read the trace while
  it is running.
- **API keys** are read from the environment by Tether itself. Commands in
  the sandbox start with only `PATH` set and do not inherit them.

## What counts as a vulnerability

Yes:

- A call that policy should have denied, or asked about, running anyway.
- A side effect that reaches the model or a tool without a policy decision in
  the trace.
- The `docker` backend exposing a host path other than the workspace, or
  network access when configured with none.
- A call reaching an external MCP server without a policy decision, or a
  stdio server started with an environment variable the policy did not name
  (other than `PATH`).
- A header value filled in from the environment appearing in a trace.
- An MCP server named only by a trace being started without
  `--trust-recorded-servers`.
- The file tools accepting a path that leaves the workspace, such as one with
  `..` in it.
- A budget that can be exceeded by more than the one call that crosses it.
- A call that names a denied path or command in different letter case, or
  with arguments in another shape, getting past the `deny`.
- A crafted trace or packed tar that makes `inspect`, `verify`, `unpack`,
  `replay` or `ui` write outside its output directory or run code.

No, because it is documented above:

- A command under the `process` backend reaching files by absolute path or
  through a symlink, or the network on macOS.
- A rule in your own `tether.toml` that matches less than you meant. Reports
  of confusing rule semantics are welcome as ordinary issues.
