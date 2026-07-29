/** React binding for {@link Session}. */

import { useCallback, useEffect, useRef, useState } from "react";

import { Session, type SessionStatus } from "./acp/session";
import { emptyThread, type AgentInfo, type InspectorEntry, type Thread } from "./acp/types";

/** Everything a connected view needs. */
export interface SessionState {
  thread: Thread;
  status: SessionStatus;
  agentInfo?: AgentInfo;
  frames: InspectorEntry[];
  prompt(text: string): void;
  cancel(): void;
  setMode(modeId: string): void;
  answerPermission(toolCallId: string, optionId: string | null): void;
  error?: string;
}

/** The inspector keeps a bounded log; Zed's ACP tools ring-buffer at 2000. */
const MAX_FRAMES = 2000;

/** Opens a session against `agentId` in `cwd`, and keeps React in step. */
export function useSession(agentId: string | null, cwd: string | null): SessionState {
  const [thread, setThread] = useState<Thread>(emptyThread);
  const [status, setStatus] = useState<SessionStatus>({ state: "connecting" });
  const [agentInfo, setAgentInfo] = useState<AgentInfo>();
  const [frames, setFrames] = useState<InspectorEntry[]>([]);
  const [error, setError] = useState<string>();

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
      agentInfo: setAgentInfo,
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

    active.connect({ agentId, cwd: cwd ?? undefined }).catch((cause: unknown) => {
      setError(cause instanceof Error ? cause.message : String(cause));
    });

    return () => {
      active.close();
      session.current = null;
    };
  }, [agentId, cwd]);

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

  const answerPermission = useCallback((toolCallId: string, optionId: string | null) => {
    session.current?.answerPermission(toolCallId, optionId);
  }, []);

  return { thread, status, agentInfo, frames, prompt, cancel, setMode, answerPermission, error };
}
