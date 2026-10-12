import { describe, expect, test } from "bun:test";

// Run against the bridge emitted by tty7, never a second hand-written template.
// TTY7_PI_BRIDGE names an isolated generated Pi/OMP/Prime Agent index.ts.
const bridgePath = process.env.TTY7_PI_BRIDGE;
if (!bridgePath) throw new Error("Set TTY7_PI_BRIDGE to the generated tty7 extension path");
const source = await Bun.file(bridgePath).text();
const transpiler = new Bun.Transpiler({ loader: "ts" });
const code = transpiler.transformSync(source.replace('import { spawnSync } from "node:child_process";', ""));

type Report = { event: string; session: string | undefined };
type Context = {
  sessionManager: { getSessionId(): string };
  agent?: { kind: string; depth: number };
  hasPendingMessages(): boolean;
};
type Handler = (event: Record<string, unknown>, ctx: Context) => void;

function rig(reports: Report[]) {
  const handlers = new Map<string, Handler>();
  const factory = new Function("spawnSync", "process", code.replace("export default", "return"))(
    (_exe: string, args: string[], options: { input?: string }) => {
      const payload = options.input ? JSON.parse(options.input) : {};
      reports.push({ event: args[2], session: payload.session_id });
    },
    { env: { TTY7: "1" } },
  );
  factory({ on: (event: string, handler: Handler) => handlers.set(event, handler) });
  return (event: string, ctx: Context, fields: Record<string, unknown> = {}) => {
    const handler = handlers.get(event);
    if (!handler) return;
    handler(fields, ctx);
  };
}

function context(session: string, kind?: string): Context {
  return {
    sessionManager: { getSessionId: () => session },
    agent: kind ? { kind, depth: 0 } : undefined,
    hasPendingMessages: () => false,
  };
}

describe("generated terminal agent ownership", () => {
  test("loading a factory without a session context cannot reset a pane", () => {
    const reports: Report[] = [];
    rig(reports);
    expect(reports).toEqual([]);
  });

  test("a rebound depth-zero subagent cannot reset or finish the main terminal session", () => {
    const reports: Report[] = [];
    const main = rig(reports);
    // Test child ownership independently of the old context-free load report.
    reports.length = 0;
    const root = context("root", "main");
    main("session_start", root);
    main("agent_start", root);
    const child = rig(reports);
    const sub = context("child", "sub");
    for (const event of ["session_start", "agent_start", "agent_end", "session_shutdown",
      "tool_approval_requested", "tool_approval_resolved", "auto_retry_start", "agent_settled"]) {
      child(event, sub);
      // A provider forwarding child contexts through the main handler must
      // not mutate its remembered turn or learn child-only settled support.
      main(event, sub);
    }
    expect(reports).toEqual([
      { event: "session-start", session: "root" },
      { event: "prompt-submit", session: "root" },
    ]);
    main("tool_approval_requested", root);
    main("tool_approval_resolved", root);
    main("agent_end", root, { willContinue: false });
    expect(reports.slice(-3)).toEqual([
      { event: "permission-request", session: "root" },
      { event: "prompt-submit", session: "root" },
      { event: "stop", session: "root" },
    ]);
  });

  test("continuation and queued messages stay working; genuine completion stays Done", () => {
    const reports: Report[] = [];
    const emit = rig(reports);
    const root = context("root", "main");
    emit("agent_start", root);
    emit("agent_end", root, { willContinue: true });
    root.hasPendingMessages = () => true;
    emit("agent_end", root, { willContinue: false });
    expect(reports).toEqual([{ event: "prompt-submit", session: "root" }]);
    root.hasPendingMessages = () => false;
    emit("auto_retry_start", root);
    emit("agent_end", root, { willContinue: false });
    expect(reports.slice(-2)).toEqual([
      { event: "prompt-submit", session: "root" },
      { event: "stop", session: "root" },
    ]);
    // Pi versions with the final settled event keep using that final boundary.
    emit("agent_settled", root);
    emit("agent_start", root);
    emit("agent_end", root);
    expect(reports.at(-1)).toEqual({ event: "prompt-submit", session: "root" });
    emit("agent_settled", root);
    expect(reports.at(-1)).toEqual({ event: "stop", session: "root" });
  });

  test("Pi contexts without OMP identity retain ordinary Waiting/Done semantics", () => {
    const reports: Report[] = [];
    const emit = rig(reports);
    const root = context("pi-root");
    emit("session_start", root);
    emit("agent_start", root);
    emit("ui_prompt_start", root, { kind: "confirm" });
    emit("ui_prompt_end", root);
    emit("agent_end", root);
    expect(reports.map(report => report.event)).toEqual([
      "session-start", "prompt-submit", "permission-request", "prompt-submit", "stop",
    ]);
    emit("ui_prompt_start", root, { kind: "select" });
    emit("ui_prompt_end", root);
    expect(reports.slice(-2).map(report => report.event)).toEqual(["question-asked", "stop"]);
  });
});
