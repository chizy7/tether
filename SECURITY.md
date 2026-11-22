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

**The sandbox** limits what a tool call can do once policy has allowed it.
How much it limits depends on the backend.

### `process` (the default): not a security boundary

Commands run as ordinary child processes, as the user running Tether. This
backend limits the damage of an ordinary mistake. It does not contain a
program that is trying to get out.

| | Linux | macOS |
| --- | --- | --- |
| Commands start in the workspace with only `PATH` in the environment | yes | yes |
| The `read_file` and `write_file` tools refuse a path that leaves the workspace | yes | yes |
| Time limit, killing the command and everything it started | yes | yes |
| CPU time and open file limits | yes | yes |
| Memory limit | yes | no (refused at start) |
| Deny list of system calls, no privilege gain through setuid programs | yes | no |
| No internet sockets when the run has no network | yes | **no, and not reported** |

What it does not stop, on either platform:

- **A command can read and write any file the user can,** by absolute path.
  The workspace check covers the file tools, not what a command does.
- **The file tools follow symlinks.** A link inside the workspace that points
  outside it is read or written through.
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

### `firecracker`: planned for v1.0

A separate kernel per run. It does not exist yet.

## Other things worth knowing

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
- The file tools accepting a path that leaves the workspace, such as one with
  `..` in it.
- A budget that can be exceeded by more than the one call that crosses it.
- A crafted trace or packed tar that makes `inspect`, `verify`, `unpack`,
  `replay` or `ui` write outside its output directory or run code.

No, because it is documented above:

- A command under the `process` backend reaching files by absolute path or
  through a symlink, or the network on macOS.
- A rule in your own `tether.toml` that matches less than you meant. Reports
  of confusing rule semantics are welcome as ordinary issues.
