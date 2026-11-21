// The client: one class, one gRPC request per method.

import { spawn } from "node:child_process";
import * as fs from "node:fs";
import * as path from "node:path";

import * as grpc from "@grpc/grpc-js";
import * as protoLoader from "@grpc/proto-loader";

const DEFAULT_DIR = ".tether";

/** How long to wait for a daemon this client started to answer on its socket. */
const DAEMON_START_MS = 10_000;

/** A call the daemon refused. `code` is the gRPC status code. */
export class TetherError extends Error {
  readonly code: grpc.status;

  constructor(code: grpc.status, message: string) {
    super(message);
    this.name = "TetherError";
    this.code = code;
  }
}

/**
 * The run is halted or finished and takes no more calls.
 *
 * Tether halts a run when a budget is spent, a policy ask goes unanswered,
 * the kill switch is tripped or the model fails. The message says which.
 */
export class Halted extends TetherError {
  constructor(code: grpc.status, message: string) {
    super(code, message);
    this.name = "Halted";
  }
}

/** A piece of a message: text, a tool call, a tool result, or a block only its provider understands. */
export type Block = { type: string; [field: string]: unknown };

/** A message in Tether's form. */
export interface Message {
  role: "system" | "user" | "assistant" | "tool";
  content: Block[];
}

/** A tool the run offers, as listed when the run started. */
export interface Tool {
  name: string;
  server: string;
  description: string;
  inputSchema: unknown;
}

/** A tool the model asked for. */
export interface ToolCall {
  id: string;
  name: string;
  arguments: unknown;
}

/**
 * What a tool call came to.
 *
 * A call that policy denied comes back here with `isError` set, like any
 * other failed call. It does not throw.
 */
export interface ToolResult {
  id: string;
  content: unknown;
  isError: boolean;
  durationMs: number;
}

/**
 * One model turn.
 *
 * Append `message` to your history exactly as it is. It can hold blocks only
 * the provider understands, which must go back unchanged. Look at
 * `stopReason` before acting: `refusal` and `max_tokens` are replies that
 * are not finished answers.
 */
export interface Reply {
  message: Message;
  toolCalls: ToolCall[];
  /** The provider's word for why it stopped; `null` if it gave none. */
  stopReason: string | null;
  /** The reply's text blocks, joined. */
  text: string;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
}

/** One event of the trace. */
export interface TraceEvent {
  seq: number;
  type: string;
  /** The whole event as the trace has it: the envelope, with the payload under `data`. */
  data: { data: Record<string, any>; [field: string]: any };
}

/** A user message holding `text`. */
export function user(text: string): Message {
  return { role: "user", content: [{ type: "text", text }] };
}

/** A system message holding `text`. Put it first to make it the system prompt. */
export function system(text: string): Message {
  return { role: "system", content: [{ type: "text", text }] };
}

/** The message that hands a turn's tool results back to the model. */
export function toolResults(results: readonly ToolResult[]): Message {
  return {
    role: "tool",
    content: results.map((result) => ({
      type: "tool_result",
      id: result.id,
      content: result.content,
      is_error: result.isError,
    })),
  };
}

export interface RunOptions {
  /** Path of a `tether.toml`. Without one there are no rules and every tool call asks. */
  policy?: string;
  /**
   * `"anthropic"` (the default), `"openai_compat"` for a server with an
   * OpenAI-style chat completions endpoint (`OPENAI_BASE_URL`,
   * `OPENAI_API_KEY`), or `"fake"` with `script` for tests and demos.
   */
  provider?: string;
  /** For provider `"fake"`: a file of scripted replies. */
  script?: string;
  /** Where the trace goes. Default `.tether`. */
  dir?: string;
  /** The sandbox's working directory. Default: one beside the trace. */
  workspace?: string;
  /** Default `"main"`. */
  agent?: string;
  /** Provider beta headers that a field in `params` needs. */
  betas?: readonly string[];
  /** The daemon's socket. Default `<dir>/tetherd.sock`. */
  socket?: string;
}

export interface CompleteOptions {
  model: string;
  /** The whole history. */
  messages: readonly Message[];
  /** Names of the tools to offer. By default all of the run's. */
  tools?: readonly string[];
  /** Passed to the provider as the request's other fields (`max_tokens` and so on). */
  params?: Record<string, unknown>;
}

// The wire messages, with field names as the .proto spells them.
interface WireRun {
  run_id: string;
  trace_path: string;
  tools: { name: string; server: string; description: string; input_schema_json: string }[];
}
interface WireModelResponse {
  message_json: string;
  tool_calls: { id: string; name: string; arguments_json: string }[];
  input_tokens: number;
  output_tokens: number;
  stop_reason: string;
  cache_read_tokens: number;
  cache_write_tokens: number;
}
interface WireToolResult {
  id: string;
  content_json: string;
  is_error: boolean;
  duration_ms: number;
}
interface WireEvent {
  seq: number;
  type: string;
  json: string;
}

type Stub = grpc.Client & Record<string, (...args: unknown[]) => unknown>;

function toError(error: unknown): TetherError {
  const status = error as Partial<grpc.ServiceError>;
  const code = status.code ?? grpc.status.UNKNOWN;
  const message = status.details || (error instanceof Error ? error.message : String(error));
  return code === grpc.status.FAILED_PRECONDITION
    ? new Halted(code, message)
    : new TetherError(code, message);
}

let cachedService: grpc.ServiceClientConstructor | undefined;

/** The client class for the service, from the .proto shipped with this package. */
function service(): grpc.ServiceClientConstructor {
  if (!cachedService) {
    // From dist/src or src, the package root is two levels up.
    const proto = path.join(__dirname, "..", "..", "proto", "tether.proto");
    const definition = protoLoader.loadSync(proto, {
      keepCase: true,
      // 64-bit counts here are token counts and milliseconds: they fit a number.
      longs: Number,
      defaults: true,
    });
    const loaded = grpc.loadPackageDefinition(definition) as unknown as {
      tether: { v1: { Tether: grpc.ServiceClientConstructor } };
    };
    cachedService = loaded.tether.v1.Tether;
  }
  return cachedService;
}

function onPath(program: string): boolean {
  const dirs = (process.env.PATH ?? "").split(path.delimiter).filter(Boolean);
  return dirs.some((dir) => {
    try {
      fs.accessSync(path.join(dir, program), fs.constants.X_OK);
      return true;
    } catch {
      return false;
    }
  });
}

/** The command that starts the daemon: `tetherd`, or `tether daemon`. */
function daemonCommand(): string[] {
  const override = process.env.TETHERD;
  if (override) return [override];
  if (onPath("tetherd")) return ["tetherd"];
  if (onPath("tether")) return ["tether", "daemon"];
  throw new TetherError(
    grpc.status.UNAVAILABLE,
    "no daemon is running and neither tetherd nor tether is on PATH; " +
      "set TETHERD to the tetherd binary",
  );
}

function open(socket: string): Stub {
  const Tether = service();
  return new Tether(`unix:${socket}`, grpc.credentials.createInsecure(), {
    // Over a Unix socket gRPC would send the socket's path as the HTTP/2
    // authority, which the daemon's HTTP stack rejects. Name one instead.
    "grpc.default_authority": "localhost",
  }) as unknown as Stub;
}

function ready(stub: Stub, ms: number): Promise<boolean> {
  return new Promise((resolve) => {
    stub.waitForReady(Date.now() + ms, (error) => resolve(!error));
  });
}

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

/** A client for the daemon on `socket`, starting the daemon if needed. */
async function connect(socket: string): Promise<Stub> {
  if (fs.existsSync(socket)) {
    const stub = open(socket);
    if (await ready(stub, 1000)) return stub;
    stub.close();
  }

  const command = daemonCommand();
  fs.mkdirSync(path.dirname(socket), { recursive: true });
  const logPath = path.join(path.dirname(socket), "tetherd.log");
  const log = fs.openSync(logPath, "a");
  try {
    const [program, ...args] = command;
    const daemon = spawn(program as string, [...args, "--socket", socket], {
      // Its own session, so it outlives this process and its Ctrl-C.
      detached: true,
      stdio: ["ignore", log, log],
    });
    // A failure to start shows up below as the socket never appearing.
    daemon.on("error", () => {});
    daemon.unref();
  } finally {
    fs.closeSync(log);
  }

  const deadline = Date.now() + DAEMON_START_MS;
  while (Date.now() < deadline) {
    if (fs.existsSync(socket)) {
      const stub = open(socket);
      if (await ready(stub, 1000)) return stub;
      stub.close();
    }
    await sleep(50);
  }
  throw new TetherError(
    grpc.status.UNAVAILABLE,
    `the daemon did not come up on ${socket}; see ${logPath}`,
  );
}

/**
 * A run under Tether.
 *
 * `Run.start` starts the run. Call `finish` when the agent is done, or use
 * `withRun`, which finishes or halts the run for you.
 *
 * Options left out fall back to `TETHER_POLICY`, `TETHER_DIR` and
 * `TETHER_SOCKET`, which `tether run -- <command>` sets.
 */
export class Run {
  /** The run's id. */
  readonly id: string;
  /** Where the trace is being written. */
  readonly tracePath: string;
  /** The tools the run offers. */
  readonly tools: readonly Tool[];

  private readonly stub: Stub;
  private over = false;

  private constructor(stub: Stub, created: WireRun) {
    this.stub = stub;
    this.id = created.run_id;
    this.tracePath = created.trace_path;
    this.tools = created.tools.map((tool) => ({
      name: tool.name,
      server: tool.server,
      description: tool.description,
      inputSchema: JSON.parse(tool.input_schema_json),
    }));
  }

  static async start(options: RunOptions = {}): Promise<Run> {
    // The daemon may have another working directory: send absolute paths.
    const absolute = (given: string | undefined) => (given ? path.resolve(given) : "");
    const dir = path.resolve(options.dir || process.env.TETHER_DIR || DEFAULT_DIR);
    const socket = path.resolve(
      options.socket || process.env.TETHER_SOCKET || path.join(dir, "tetherd.sock"),
    );
    const stub = await connect(socket);
    try {
      const created = await unary<WireRun>(stub, "CreateRun", {
        agent: options.agent ?? "main",
        policy_path: absolute(options.policy || process.env.TETHER_POLICY),
        workspace: absolute(options.workspace),
        provider: options.provider ?? "anthropic",
        script_path: absolute(options.script),
        dir,
        betas: [...(options.betas ?? [])],
      });
      return new Run(stub, created);
    } catch (error) {
      stub.close();
      throw toError(error);
    }
  }

  /** One request, with a halt remembered so that `withRun` does not finish a halted run. */
  private async call<T>(method: string, request: object): Promise<T> {
    try {
      return await unary<T>(this.stub, method, request);
    } catch (error) {
      const refused = toError(error);
      if (refused instanceof Halted) this.over = true;
      throw refused;
    }
  }

  /** Ask the model for its next turn. */
  async complete(options: CompleteOptions): Promise<Reply> {
    const response = await this.call<WireModelResponse>("Complete", {
      run_id: this.id,
      model: options.model,
      messages_json: JSON.stringify(options.messages),
      tools: options.tools ? [...options.tools] : this.tools.map((tool) => tool.name),
      params_json: options.params ? JSON.stringify(options.params) : "",
    });
    const message = JSON.parse(response.message_json) as Message;
    return {
      message,
      toolCalls: response.tool_calls.map((call) => ({
        id: call.id,
        name: call.name,
        arguments: JSON.parse(call.arguments_json),
      })),
      stopReason: response.stop_reason || null,
      text: (message.content ?? [])
        .filter((block) => block.type === "text" && typeof block.text === "string")
        .map((block) => block.text)
        .join(""),
      inputTokens: response.input_tokens,
      outputTokens: response.output_tokens,
      cacheReadTokens: response.cache_read_tokens,
      cacheWriteTokens: response.cache_write_tokens,
    };
  }

  /** Run a tool the model asked for, through policy and the sandbox. */
  async callTool(call: ToolCall): Promise<ToolResult> {
    const result = await this.call<WireToolResult>("CallTool", {
      run_id: this.id,
      call: { id: call.id, name: call.name, arguments_json: JSON.stringify(call.arguments) },
    });
    return {
      id: result.id,
      content: JSON.parse(result.content_json),
      isError: result.is_error,
      durationMs: result.duration_ms,
    };
  }

  /** Say the agent is done. `outcome` is recorded in the trace. */
  async finish(outcome: unknown = null): Promise<void> {
    try {
      await this.call("Finish", { run_id: this.id, outcome_json: JSON.stringify(outcome) });
    } finally {
      this.over = true;
    }
  }

  /** Trip the run's kill switch. */
  async halt(): Promise<void> {
    try {
      await this.call("Halt", { run_id: this.id });
    } finally {
      this.over = true;
    }
  }

  /** Answer a policy ask. The call that asked is still waiting on its own promise. */
  async answer(subjectSeq: number, allow: boolean, answeredBy = "sdk"): Promise<void> {
    await this.call("Answer", {
      run_id: this.id,
      subject_seq: subjectSeq,
      allow,
      answered_by: answeredBy,
    });
  }

  /** The trace so far, then each new event until the run ends. */
  async *events(): AsyncGenerator<TraceEvent> {
    const start = this.stub.Events as (request: object) => grpc.ClientReadableStream<WireEvent>;
    const stream = start.call(this.stub, { run_id: this.id });
    try {
      for await (const event of stream) {
        const wire = event as WireEvent;
        yield { seq: wire.seq, type: wire.type, data: JSON.parse(wire.json) };
      }
    } catch (error) {
      throw toError(error);
    } finally {
      stream.cancel();
    }
  }

  /** Whether the run has finished or halted, as far as this client has seen. */
  get isOver(): boolean {
    return this.over;
  }

  /** Close the connection. The run itself is not affected. */
  close(): void {
    this.stub.close();
  }
}

function unary<T>(stub: Stub, method: string, request: object): Promise<T> {
  return new Promise((resolve, reject) => {
    const send = stub[method] as (
      request: object,
      done: (error: grpc.ServiceError | null, response: T) => void,
    ) => void;
    send.call(stub, request, (error, response) => (error ? reject(error) : resolve(response)));
  });
}

/**
 * Start a run, hand it to `body`, and end it: finished if `body` returns
 * without having finished or halted it, halted if `body` throws. The
 * connection is closed either way.
 */
export async function withRun<T>(
  options: RunOptions,
  body: (run: Run) => Promise<T>,
): Promise<T> {
  const run = await Run.start(options);
  try {
    const result = await body(run);
    if (!run.isOver) await run.finish();
    return result;
  } catch (error) {
    if (!run.isOver) {
      // The body's error is the one to report, not a failure to halt.
      await run.halt().catch(() => {});
    }
    throw error;
  } finally {
    run.close();
  }
}
