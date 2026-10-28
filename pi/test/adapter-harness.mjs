import defaultFactory from "../index.ts";

// Registered-handler test runtime. This deliberately models replacement (fresh
// factory/context/history + stale old APIs), not Pi internals. Real SDK coverage
// lives in pi-reset.test.mjs and the independent production-runtime regression.
export function harness(directory, options = {}) {
  const root = { states: [], errors: [], staleCalls: [], dispatches: [], replacements: 0, attempts: 0,
    flagValues: new Map(options.flagValues instanceof Map ? options.flagValues : Object.entries(options.flagValues ?? {})),
    flagReads: [], registeredFlags: new Map(),
    factory: options.factory ?? defaultFactory, cancel: false, cancelFirst: false, depth: 0 };
  function renderPromptSections(promptOptions) {
    if (promptOptions.forceSystemPrompt !== undefined) return promptOptions.forceSystemPrompt;
    return Object.entries(promptOptions.sections ?? {}).map(([name, content]) =>
      name === "preamble" ? content : `<${name}>\n${content}\n</${name}>`).join("\n\n");
  }
  function create(id) {
    const events = new Map(), commands = new Map(), tools = new Map();
    const entries = [], messages = [], notifications = [], statuses = [];
    let active = ["read", "bash", "policy-tool"], idle = true, editor = "", pending = false, deferInsertion = false, alive = true;
    function live(action) { if (!alive) { root.staleCalls.push(action); throw new Error("stale fixture context/API"); } }
    const context = {
      cwd: directory, mode: "tui", isIdle: () => { live("isIdle"); return idle; }, hasPendingMessages: () => { live("pending"); return pending; },
      sessionManager: { getSessionId: () => { live("session ID"); return id; }, getBranch: () => { live("branch"); return entries; } },
      ui: { notify: (message, level) => { live("notify"); notifications.push({ message, level }); }, setStatus: (_key, value) => { live("status"); statuses.push(value); }, getEditorText: () => { live("editor"); return editor; } },
      async sendUserMessage(text, opts) { live("fresh command dispatch"); return dispatch(text, opts); },
      async newSession(opts) {
        live("newSession"); root.attempts++;
        if (root.cancel && root.cancelFirst) return { cancelled: true };
        const before = await emit("session_before_switch", { reason: "new" });
        if (before.some(value => value?.cancel) || root.cancel) return { cancelled: true };
        const next = await root.replace("new", { beforeDone: true });
        await root.beforeFreshRestore?.(next);
        await opts.withSession?.(next.context);
        return { cancelled: false };
      },
    };
    async function emit(event, payload = {}) {
      if (event === "before_agent_start") {
        payload.systemPromptOptions ??= { sections: { preamble: payload.systemPrompt ?? "" } };
        payload.systemPromptOptions.sections ??= {};
        Object.defineProperty(payload, "systemPrompt", {
          configurable: true, enumerable: true,
          get: () => renderPromptSections(payload.systemPromptOptions),
        });
      }
      const results = []; root.depth++;
      try {
        for (const fn of events.get(event) ?? []) {
          const value = await fn(payload, context); results.push(value);
          if (event === "session_before_switch" && value?.cancel) break;
        }
        return results;
      } finally { root.depth--; }
    }
    async function dispatch(text, opts) {
      live("command dispatch");
      root.dispatches.push({ text, opts, id, depth: root.depth });
      if (!opts?.expandPromptTemplates) throw new Error("Command expansion required by fixture");
      const [name, ...args] = text.slice(1).split(" ");
      const command = commands.get(name);
      if (!command) throw new Error("Unknown fixture command; no model fallback is permitted");
      return command.handler(args.join(" "), context);
    }
    async function insert(index) {
      const { message, options } = messages[index];
      entries.push({ type: "custom_message", ...message });
      if (options.triggerTurn) await emit("message_end", { message: { ...message, role: "custom" } });
    }
    const api = {
      getCommands: () => [...commands].map(([name]) => ({ name })),
      getActiveTools: () => { live("active tools"); return active; },
      setActiveTools: names => { live("set tools"); active = names; },
      on: (event, handler) => { const handlers = events.get(event) ?? []; handlers.push(handler); events.set(event, handlers); },
      registerFlag: (name, definition) => root.registeredFlags.set(name, definition),
      getFlag: name => { root.flagReads.push(name); return root.flagValues.get(name); },
      registerCommand: (name, value) => commands.set(name, value), registerTool: definition => tools.set(definition.name, definition),
      appendEntry: (customType, data) => { live("append"); entries.push({ type: "custom", customType, data }); },
      sendUserMessage(text, opts) { live("old command dispatch"); void Promise.resolve().then(() => dispatch(text, opts)).catch(error => root.errors.push(error)); },
      sendMessage: (message, opts) => {
        live("send message"); messages.push({ message, options: opts });
        if (opts.triggerTurn && idle) { idle = false; void emit("agent_start"); }
        if (!deferInsertion) void insert(messages.length - 1);
      },
    };
    const state = { api, context, emit, commands, tools, entries, messages, notifications, statuses, root,
      prompt: async base => { const event = { systemPrompt: base ?? "" }; await emit("before_agent_start", event); return { text: event.systemPrompt, options: event.systemPromptOptions }; },
      active: () => active, editor: text => { editor = text; }, pending: value => { pending = value; }, idle: value => { idle = value; },
      deferInsertion: value => { deferInsertion = value; }, insert, invalidate: () => { alive = false; },
      settle: async (aborted = false) => { idle = true; await emit("agent_settled", { aborted }); },
    };
    root.states.push(state); return state;
  }
  root.current = create(options.sessionId ?? "pi-session");
  root.replace = async (reason, { beforeDone = false } = {}) => {
    const current = root.current;
    if (!beforeDone) {
      const event = reason === "fork" ? "session_before_fork" : "session_before_switch";
      const payload = reason === "fork" ? { entryId: "fixture-entry", position: "before" } : { reason };
      const before = await current.emit(event, payload);
      if (before.some(value => value?.cancel)) return undefined;
    }
    await current.emit("session_shutdown", { reason });
    current.invalidate();
    const next = create(`${options.sessionId ?? "pi-session"}-fresh-${++root.replacements}`);
    root.current = next;
    await root.factory(next.api);
    await next.emit("session_start", { reason });
    return next;
  };
  return root.current;
}
