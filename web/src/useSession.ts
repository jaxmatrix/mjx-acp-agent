/** React binding for {@link Session}. */

import { useCallback, useEffect, useRef, useState } from "react";

import { resumeStore } from "./acp/resume";
import { Session, type SessionStatus } from "./acp/session";
import {
  emptyThread,
  type AgentInfo,
  type ElicitationAnswer,
  type InspectorEntry,
  type Thread,
} from "./acp/types";

/** Everything a connected view needs. */
export interface SessionState {
  thread: Thread;
  status: SessionStatus;
  agentInfo?: AgentInfo;
  frames: InspectorEntry[];
  prompt(text: string): void;
  cancel(): void;
  setMode(modeId: string): void;
  setConfigOption(configId: string, value: string | boolean): void;
  answerPermission(toolCallId: string, optionId: string | null): void;
  answerElicitation(requestId: string | number, answer: ElicitationAnswer): void;
  /** Opens a new socket to the same agent, taking it back from another tab. */
  reconnect(): void;
  error?: string;
}

/** The inspector keeps a bounded log; Zed's ACP tools ring-buffer at 2000. */
const MAX_FRAMES = 2000;

const resume = resumeStore();

/** Opens a session against `agentId` in `cwd`, and keeps React in step. */
export function useSession(agentId: string | null, cwd: string | null): SessionState {
  const [thread, setThread] = useState<Thread>(emptyThread);
  const [status, setStatus] = useState<SessionStatus>({ state: "connecting" });
  const [agentInfo, setAgentInfo] = useState<AgentInfo>();
  const [frames, setFrames] = useState<InspectorEntry[]>([]);
  const [error, setError] = useState<string>();
  /** Bumped to reopen the socket; see `reconnect`. */
  const [attempt, setAttempt] = useState(0);

  const session = useRef<Session>(null);
  const nextSeq = useRef(0);

  useEffect(() => {
    if (!agentId) return;

    // Reset rather than accumulate: switching agents starts a new conversation.
    setThread(emptyThread());
    setFrames([]);
    setError(undefined);
    setAgentInfo(undefined);
    nextSeq.current = 0;

    const active = new Session(emptyThread(), {
      thread: setThread,
      agentInfo: (info) => {
        // Keep the handle to come back with. The server mints a new id when it
        // could not honour the one we sent, so writing back whatever it says
        // is also how a stale id gets replaced.
        resume.set(agentId, cwd ?? "", info.connectionId);
        setAgentInfo(info);
      },
      status: setStatus,
      frame: (entry) => {
        setFrames((current) => {
          const next = [
            ...current,
            { ...entry, seq: (nextSeq.current += 1), at: Date.now() },
          ];
          return next.length > MAX_FRAMES ? next.slice(-MAX_FRAMES) : next;
        });
      },
    });
    session.current = active;

    active
      .connect({
        agentId,
        cwd: cwd ?? undefined,
        // If this tab was here before, rejoin the agent it left rather than
        // start another. The reset above is not wasted: the replay replaces the
        // thread a moment later, and until it does an empty page is honest.
        resume: resume.get(agentId, cwd ?? ""),
      })
      .catch((cause: unknown) => {
        setError(cause instanceof Error ? cause.message : String(cause));
      });

    return () => {
      active.close();
      session.current = null;
    };
  }, [agentId, cwd, attempt]);

  const prompt = useCallback((text: string) => {
    session.current?.prompt(text).catch((cause: unknown) => {
      setError(cause instanceof Error ? cause.message : String(cause));
    });
  }, []);

  const cancel = useCallback(() => {
    void session.current?.cancel();
  }, []);

  const setMode = useCallback((modeId: string) => {
    void session.current?.setMode(modeId);
  }, []);

  const setConfigOption = useCallback((configId: string, value: string | boolean) => {
    void session.current?.setConfigOption(configId, value);
  }, []);

  const answerPermission = useCallback((toolCallId: string, optionId: string | null) => {
    session.current?.answerPermission(toolCallId, optionId);
  }, []);

  const answerElicitation = useCallback(
    (requestId: string | number, answer: ElicitationAnswer) => {
      session.current?.answerElicitation(requestId, answer);
    },
    [],
  );

  const reconnect = useCallback(() => setAttempt((n) => n + 1), []);

  return {
    thread,
    status,
    agentInfo,
    frames,
    prompt,
    cancel,
    setMode,
    setConfigOption,
    answerPermission,
    answerElicitation,
    reconnect,
    error,
  };
}
