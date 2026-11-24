/**
 * TypeScript client for Tether.
 *
 * Your agent keeps its own loop. It calls `run.complete` where it called its
 * provider SDK and `run.callTool` where it ran a tool, and Tether does the
 * rest: policy, sandbox, budgets and the trace.
 *
 * This module is a thin client for the daemon. Nothing here decides
 * anything: every call is one gRPC request.
 */

export type {
  Block,
  CompleteOptions,
  Message,
  Reply,
  RunOptions,
  Tool,
  ToolCall,
  ToolResult,
  TraceEvent,
} from "./run";
export {
  Halted,
  Run,
  system,
  TetherError,
  toolResults,
  user,
  withRun,
} from "./run";
