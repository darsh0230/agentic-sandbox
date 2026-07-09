const $ = (id) => document.getElementById(id);

const state = {
  running: false,
};

const defaults = {
  command: "pi --workspace /home/agent --input /home/agent/input/task.md",
  task: "Complete the requested task. Write structured final output to /home/agent/.pi/result.json and any artifacts under /home/agent/output.",
};

function init() {
  $("runId").value = `run-${new Date().toISOString().replace(/[-:.TZ]/g, "").slice(0, 14)}`;
  $("command").value = defaults.command;
  $("task").value = defaults.task;
  $("token").value = localStorage.getItem("piSandboxToken") || "";
  $("healthBtn").addEventListener("click", checkHealth);
  $("clearBtn").addEventListener("click", clearOutput);
  $("runForm").addEventListener("submit", runAgent);
  for (const input of document.querySelectorAll("input, textarea, select")) {
    input.addEventListener("input", updatePreview);
  }
  updatePreview();
}

function authHeaders() {
  const token = $("token").value.trim();
  localStorage.setItem("piSandboxToken", token);
  return token ? { Authorization: `Bearer ${token}` } : {};
}

async function checkHealth() {
  setHealth("checking", "Checking...", "Waiting for /api/health");
  try {
    const response = await fetch("/api/health", { headers: authHeaders() });
    if (!response.ok) throw new Error(`${response.status} ${response.statusText}`);
    const body = await response.json();
    setHealth("ok", "Healthy", `API says ${body.status}`);
  } catch (error) {
    setHealth("bad", "Unavailable", error.message);
  }
}

function setHealth(kind, text, detail) {
  $("statusLight").className = `status-light ${kind === "ok" ? "ok" : kind === "bad" ? "bad" : ""}`;
  $("healthText").textContent = text;
  $("healthDetail").textContent = detail;
}

function buildRequest() {
  const collectPaths = $("collectPaths").value.split("\n").map((line) => line.trim()).filter(Boolean);
  const resultFile = $("resultFile").value.trim();
  return {
    run_id: $("runId").value.trim(),
    command: shellWords($("command").value),
    injected_files: [{
      relative_path: "input/task.md",
      content_utf8: $("task").value,
      mode: 420,
    }],
    injected_readonly_dirs: [],
    env: [],
    timeout_ms: Number($("timeoutMs").value || 300000),
    limits: {
      stdout_max_bytes: 262144,
      stderr_max_bytes: 262144,
    },
    result_policy: {
      collect_paths: collectPaths,
      result_file: resultFile || null,
    },
    cleanup_policy: $("cleanupPolicy").value,
  };
}

function updatePreview() {
  try {
    $("requestPreview").textContent = JSON.stringify(buildRequest(), null, 2);
  } catch (error) {
    $("requestPreview").textContent = error.message;
  }
}

async function runAgent(event) {
  event.preventDefault();
  if (state.running) return;
  clearOutput();
  state.running = true;
  $("runBtn").disabled = true;
  $("runBtn").textContent = "Running...";
  appendEvent("client", "Submitting run request");

  try {
    const response = await fetch("/api/run", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        ...authHeaders(),
      },
      body: JSON.stringify(buildRequest()),
    });
    if (!response.ok) throw new Error(await response.text());
    await readNdjson(response.body);
  } catch (error) {
    appendEvent("error", error.message, "stderr");
  } finally {
    state.running = false;
    $("runBtn").disabled = false;
    $("runBtn").textContent = "Run Agent";
  }
}

async function readNdjson(body) {
  const reader = body.pipeThrough(new TextDecoderStream()).getReader();
  let buffer = "";
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buffer += value;
    let newline;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (line) handleLine(JSON.parse(line));
    }
  }
}

function handleLine(line) {
  if (line.type === "event") {
    renderRunEvent(line.event);
  } else if (line.type === "result") {
    appendEvent("result", `exit=${line.result.exit_code ?? "signal:" + line.result.signal}`);
    $("resultJson").textContent = JSON.stringify(line.result.result_json || line.result, null, 2);
    renderFiles(line.result.collected_files || []);
  } else if (line.type === "error") {
    appendEvent("error", line.message, "stderr");
  }
}

function renderRunEvent(event) {
  switch (event.type) {
    case "started":
      appendEvent("started", `${event.run_id} on ${event.slot_id}, pid ${event.pid}`);
      break;
    case "stdout":
      appendEvent("stdout", decodeBytes(event.data));
      break;
    case "stderr":
      appendEvent("stderr", decodeBytes(event.data), "stderr");
      break;
    case "result_file_found":
      appendEvent("artifact", `${event.relative_path} (${event.size} bytes)`);
      break;
    case "exited":
      appendEvent("exited", `code=${event.exit_code ?? "none"} signal=${event.signal ?? "none"}`);
      break;
    case "timed_out":
      appendEvent("timeout", "Run exceeded timeout", "stderr");
      break;
    default:
      appendEvent(event.type, JSON.stringify(event));
  }
}

function renderFiles(files) {
  if (!files.length) {
    $("files").textContent = "No files collected.";
    return;
  }
  $("files").textContent = files.map((file) => `# ${file.relative_path} (${file.size} bytes)\n${decodeBytes(file.content)}`).join("\n\n");
}

function appendEvent(tag, payload, className = "") {
  const row = document.createElement("div");
  row.className = "event";
  const label = document.createElement("div");
  label.className = "tag";
  label.textContent = tag;
  const content = document.createElement("div");
  content.className = `payload ${className}`;
  content.textContent = payload;
  row.append(label, content);
  $("timeline").append(row);
  $("timeline").scrollTop = $("timeline").scrollHeight;
}

function clearOutput() {
  $("timeline").textContent = "";
  $("resultJson").textContent = "No result yet.";
  $("files").textContent = "No files collected yet.";
}

function decodeBytes(value) {
  if (typeof value === "string") return value;
  return new TextDecoder().decode(Uint8Array.from(value || []));
}

function shellWords(input) {
  const words = [];
  let current = "";
  let quote = null;
  let escaping = false;
  for (const char of input.trim()) {
    if (escaping) {
      current += char;
      escaping = false;
    } else if (char === "\\") {
      escaping = true;
    } else if (quote) {
      if (char === quote) quote = null;
      else current += char;
    } else if (char === "'" || char === '"') {
      quote = char;
    } else if (/\s/.test(char)) {
      if (current) {
        words.push(current);
        current = "";
      }
    } else {
      current += char;
    }
  }
  if (quote) throw new Error(`Unclosed ${quote} quote in command`);
  if (escaping) current += "\\";
  if (current) words.push(current);
  if (!words.length) throw new Error("Command is required");
  return words;
}

init();
