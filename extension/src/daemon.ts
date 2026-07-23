import * as fs from "fs";
import * as os from "os";
import * as path from "path";

export interface DaemonInfo {
  port: number;
  token: string;
}

function stateDir(): string {
  return (
    process.env.FORGE_COMPOSER_STATE_DIR ??
    path.join(os.homedir(), ".local/share/forge-composer")
  );
}

export function discover(): DaemonInfo | undefined {
  const dir = stateDir();

  let port: number;
  try {
    const raw = fs.readFileSync(path.join(dir, "daemon.json"), "utf8");
    const parsed = JSON.parse(raw) as { port?: unknown };
    if (typeof parsed.port !== "number" || !Number.isFinite(parsed.port)) {
      return undefined;
    }
    port = parsed.port;
  } catch {
    return undefined;
  }

  let token: string;
  try {
    token = fs.readFileSync(path.join(dir, "auth.token"), "utf8").trim();
    if (!token) {
      return undefined;
    }
  } catch {
    return undefined;
  }

  return { port, token };
}

export class DaemonClient {
  private readonly baseUrl: string;
  private readonly token: string;

  constructor(info: DaemonInfo) {
    this.baseUrl = `http://127.0.0.1:${info.port}`;
    this.token = info.token;
  }

  private headers(extra?: Record<string, string>): Record<string, string> {
    return {
      Authorization: `Bearer ${this.token}`,
      ...extra,
    };
  }

  async createSession(workspace?: string): Promise<string> {
    const body =
      workspace !== undefined ? JSON.stringify({ workspace }) : "{}";
    const res = await fetch(`${this.baseUrl}/sessions`, {
      method: "POST",
      headers: this.headers({ "Content-Type": "application/json" }),
      body,
    });
    if (!res.ok) {
      throw new Error(`createSession failed: ${res.status}`);
    }
    const data = (await res.json()) as { id?: string };
    if (typeof data.id !== "string") {
      throw new Error("createSession: missing id");
    }
    return data.id;
  }

  async listSessions(): Promise<string[]> {
    const res = await fetch(`${this.baseUrl}/sessions`, {
      headers: this.headers(),
    });
    if (!res.ok) {
      throw new Error(`listSessions failed: ${res.status}`);
    }
    const data = (await res.json()) as { sessions?: string[] };
    return Array.isArray(data.sessions) ? data.sessions : [];
  }

  async events(session: string, since: number): Promise<any[]> {
    const res = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(session)}/events?since=${since}`,
      { headers: this.headers() }
    );
    if (!res.ok) {
      throw new Error(`events failed: ${res.status}`);
    }
    const data = (await res.json()) as { events?: any[] };
    return Array.isArray(data.events) ? data.events : [];
  }

  async sendMessage(
    session: string,
    text: string,
    attachments?: { path: string }[]
  ): Promise<void> {
    const payload: { text: string; attachments?: { path: string }[] } = {
      text,
    };
    if (attachments && attachments.length > 0) {
      payload.attachments = attachments;
    }
    const res = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(session)}/message`,
      {
        method: "POST",
        headers: this.headers({ "Content-Type": "application/json" }),
        body: JSON.stringify(payload),
      }
    );
    if (!res.ok) {
      throw new Error(`sendMessage failed: ${res.status}`);
    }
  }

  async approve(
    session: string,
    id: string,
    approved: boolean
  ): Promise<void> {
    const res = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(session)}/approve`,
      {
        method: "POST",
        headers: this.headers({ "Content-Type": "application/json" }),
        body: JSON.stringify({ id, approved }),
      }
    );
    if (!res.ok) {
      throw new Error(`approve failed: ${res.status}`);
    }
  }

  async checkpoints(
    session: string
  ): Promise<{ hash: string; label: string }[]> {
    const res = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(session)}/checkpoints`,
      { headers: this.headers() }
    );
    if (!res.ok) {
      throw new Error(`checkpoints failed: ${res.status}`);
    }
    const data = (await res.json()) as {
      checkpoints?: { hash: string; label: string }[];
    };
    return Array.isArray(data.checkpoints) ? data.checkpoints : [];
  }

  async restore(session: string, hash: string): Promise<void> {
    const res = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(session)}/restore`,
      {
        method: "POST",
        headers: this.headers({ "Content-Type": "application/json" }),
        body: JSON.stringify({ hash }),
      }
    );
    if (!res.ok) {
      throw new Error(`restore failed: ${res.status}`);
    }
  }

  async fileAt(session: string, hash: string, path: string): Promise<string> {
    const params = new URLSearchParams({ hash, path });
    const res = await fetch(
      `${this.baseUrl}/sessions/${encodeURIComponent(session)}/file_at?${params}`,
      { headers: this.headers() }
    );
    if (!res.ok) {
      throw new Error(`fileAt failed: ${res.status}`);
    }
    return res.text();
  }

  stream(
    session: string,
    onLedger: (e: any) => void,
    onDelta: (t: string) => void
  ): () => void {
    const controller = new AbortController();
    const url = `${this.baseUrl}/sessions/${encodeURIComponent(session)}/stream`;

    void (async () => {
      try {
        const res = await fetch(url, {
          headers: this.headers({ Accept: "text/event-stream" }),
          signal: controller.signal,
        });
        if (!res.ok || !res.body) {
          return;
        }

        const reader = res.body.getReader();
        const decoder = new TextDecoder();
        let buffer = "";

        while (true) {
          const { done, value } = await reader.read();
          if (done) {
            break;
          }
          buffer += decoder.decode(value, { stream: true });
          buffer = drainSseBuffer(buffer, onLedger, onDelta);
        }
      } catch (err) {
        if ((err as Error).name === "AbortError") {
          return;
        }
      }
    })();

    return () => controller.abort();
  }
}

function drainSseBuffer(
  buffer: string,
  onLedger: (e: any) => void,
  onDelta: (t: string) => void
): string {
  while (true) {
    const sep = buffer.indexOf("\n\n");
    if (sep === -1) {
      break;
    }

    const block = buffer.slice(0, sep);
    buffer = buffer.slice(sep + 2);

    let eventName = "";
    const dataLines: string[] = [];
    for (const line of block.split("\n")) {
      if (line.startsWith("event:")) {
        eventName = line.slice(6).trim();
      } else if (line.startsWith("data:")) {
        dataLines.push(line.slice(5).trimStart());
      }
    }

    const data = dataLines.join("\n");
    if (!data) {
      continue;
    }

    try {
      const parsed = JSON.parse(data) as { text?: string };
      if (eventName === "ledger") {
        onLedger(parsed);
      } else if (eventName === "delta") {
        onDelta(parsed.text ?? "");
      }
    } catch {
      // ignore malformed SSE payloads
    }
  }

  return buffer;
}
