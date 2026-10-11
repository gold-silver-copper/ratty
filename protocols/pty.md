# PTY backend and lifecycle

Linux, Android and macOS use fuxix 0.2.0. Its typed `Master` remains nonblocking
and close-on-exec. `Slave::spawn` consumes the slave, launches a new session
leader with a controlling terminal, and waits for the child's exec handshake.
The launcher closes inherited descriptors above standard input/output/error.
Environment overrides and working directory apply before exec. Windows and
platforms unsupported by fuxix retain portable-pty, including Windows ConPTY.

fuxix's published `Size` and `set_window_size` API only carry character
dimensions; the setter writes zero for the pixel dimensions. Ratty therefore
uses `rustix::termios::tcsetwinsize` at this boundary to preserve rows, columns,
pixel width and pixel height in one kernel update. The master remains owned by
fuxix; Ratty adds no raw descriptor ownership or unsafe ioctl implementation.
Parser geometry and successful PTY geometry remain independently tracked so a
failed kernel resize can be retried without claiming it succeeded.

## Executable entry point

Executables using `TerminalRuntime::spawn` must call
`ratty::runtime::run_pty_launcher()` before argument parsing, logging or starting
threads. Ratty, the headless capture example and the composition benchmark do
so. fuxix's private `__launch` invocation enters this path; normal application
arguments continue through the usual CLI. On the portable backend this is a
no-op. Embedders whose executable cannot supply the hook may use
`TerminalRuntime::spawn_with_launcher` with a compatible Ratty executable.

## Ownership and shutdown

A dedicated supervisor owns each child and polls `try_wait`, which reaps it
without depending on rendering, output consumption or MCP polling. The worker
is created before spawning the child. Parser and handle setup happen before
spawn; after successful spawn the supervisor owns cleanup even if subsequent
I/O worker setup fails.

The child states are `running`, `hung_up`, `stopping` and `exited`. A closed PTY
stream does not prove the child exited: a process can close its terminal and
continue running. Conversely, an exited child can leave unread output. MCP
reports stream connectivity independently from `child.state`, `child.running`
and the recorded exit code. Unix signal exits use `128 + signal`.

Shutdown cancels queued input/output work and asks the supervisor to kill the
owned child. On fuxix platforms it also signals the terminal's current
foreground group and the session leader's group. The caller gives the worker
up to 100 ms per shutdown call to finish reaping, joining it only after it has
finished. If the OS keeps a killed process uninterruptible, the worker retains
ownership and continues reaping while the host remains alive. This bounds the
UI wait; it cannot guarantee when an uninterruptible kernel operation ends.
Shutdown does not recursively discover deliberately detached/disowned processes.

The native reader polls for at most 20 ms at a time; output queue backpressure
also checks cancellation. The input worker is the sole writer and uses the
master's nonblocking writes. The portable reader/writer retain the OS backend's
blocking I/O behavior on their worker threads. A prepared cleanup worker owns
the final portable master/slave drop, because Windows `ClosePseudoConsole` can
block. During Windows and macOS shutdown the reader discards output until EOF:
ConPTY closure and macOS slave closure can wait for the output to drain before
the child finishes exiting. On macOS the reader also stops once the owned child
is reaped, so a detached process holding the slave cannot keep it alive.
Shutting down never joins an unfinished I/O worker on the UI thread.

## Validation

Linux integration tests exercise the actual Ratty launcher and PTY: environment,
working directory, controlling terminal, session ownership, terminal interrupts,
resize signals, natural exit and reaping, inherited descriptor closure, launch
failure cleanup, hangup while the child is alive, full output queues and input
backpressure. A kernel readback test covers all four resize dimensions. Native
lifecycle tests also run on macOS, with Linux `/proc` checks gated separately.
The macOS hangup fixture revokes its own PTY while the child ignores SIGHUP;
closing standard streams alone leaves the session's controlling terminal alive.
Windows tests exercise real ConPTY input, exit status and bounded shutdown.
CI runs the workspace tests on Linux, macOS and Windows in addition to its
existing platform compilation matrix.
