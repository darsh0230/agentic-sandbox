const $ = (id) => document.getElementById(id);

const state = {
  running: false,
  uploads: [],
};

const maxUploadBytes = 24 * 1024 * 1024;
const maxRequestBytes = 32 * 1024 * 1024;

const defaults = {
  command: "bash -lc 'pi -p \"$(cat input/task.md)\"'",
  task: "Complete the requested task. Write structured final output to /home/agent/result.json and any artifacts under /home/agent/output.",
};

function init() {
  $("runId").value = `run-${new Date().toISOString().replace(/[-:.TZ]/g, "").slice(0, 14)}`;
  $("command").value = defaults.command;
  $("task").value = defaults.task;
  $("token").value = localStorage.getItem("agenticSandboxToken") || "";
  $("healthBtn").addEventListener("click", checkHealth);
  $("clearBtn").addEventListener("click", clearOutput);
  $("clearUploadsBtn").addEventListener("click", clearUploads);
  $("fileUpload").addEventListener("change", () => addUploads($("fileUpload").files, false));
  $("folderUpload").addEventListener("change", () => addUploads($("folderUpload").files, true));
  $("runForm").addEventListener("submit", runAgent);
  for (const input of document.querySelectorAll("input, textarea, select")) {
    input.addEventListener("input", updatePreview);
  }
  updatePreview();
}

function authHeaders() {
  const token = $("token").value.trim();
  localStorage.setItem("agenticSandboxToken", token);
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

function buildRequest(injectedFiles = [], includeSecrets = false) {
  const collectPaths = $("collectPaths").value.split("\n").map((line) => line.trim()).filter(Boolean);
  const resultFile = $("resultFile").value.trim();
  const agentKeyName = $("agentKeyName").value.trim();
  const agentKeyValue = $("agentKeyValue").value;
  if ((agentKeyName && !agentKeyValue) || (!agentKeyName && agentKeyValue)) {
    throw new Error("Provider credential variable and value must be supplied together");
  }
  if (agentKeyName && !/^[A-Za-z_][A-Za-z0-9_]*$/.test(agentKeyName)) {
    throw new Error("Provider credential variable is not a valid environment name");
  }
  return {
    run_id: $("runId").value.trim(),
    command: shellWords($("command").value),
    injected_files: [{
      relative_path: "input/task.md",
      content_utf8: $("task").value,
      mode: 420,
    }, ...injectedFiles],
    injected_readonly_dirs: [],
    env: agentKeyName ? [[agentKeyName, includeSecrets ? agentKeyValue : "<redacted>"]] : [],
    timeout_ms: Number($("timeoutMs").value || 300000),
    kill_on_disconnect: true,
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
    const previewFiles = state.uploads.map((upload) => ({
      relative_path: upload.relativePath,
      content: `<${upload.file.size} bytes from ${upload.displayName}>`,
      mode: 420,
    }));
    $("requestPreview").textContent = JSON.stringify(buildRequest(previewFiles), null, 2);
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
    const uploadedFiles = await readInjectedUploads();
    const body = JSON.stringify(buildRequest(uploadedFiles, true));
    const bodyBytes = new TextEncoder().encode(body).length;
    if (bodyBytes > maxRequestBytes) {
      throw new Error(`Request body is ${formatBytes(bodyBytes)}; the server limit is ${formatBytes(maxRequestBytes)}.`);
    }
    const response = await fetch("/api/run", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        ...authHeaders(),
      },
      body,
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

function addUploads(files, preserveFolderPath) {
  const prefix = normalizeRelativePath($("uploadPrefix").value.trim());
  for (const file of files || []) {
    if (!file.name) continue;
    const sourcePath = preserveFolderPath ? file.webkitRelativePath || file.name : file.name;
    const relativePath = joinRelativePath(prefix, sourcePath);
    state.uploads.push({ file, relativePath, displayName: sourcePath });
  }
  $("fileUpload").value = "";
  $("folderUpload").value = "";
  renderUploads();
  updatePreview();
}

function clearUploads() {
  state.uploads = [];
  $("fileUpload").value = "";
  $("folderUpload").value = "";
  renderUploads();
  updatePreview();
}

function renderUploads() {
  if (!state.uploads.length) {
    $("uploadList").textContent = "No extra files selected.";
    return;
  }
  const totalBytes = state.uploads.reduce((sum, upload) => sum + upload.file.size, 0);
  const lines = state.uploads.map((upload) => `${upload.relativePath} (${formatBytes(upload.file.size)})`);
  $("uploadList").textContent = `${state.uploads.length} file(s), ${formatBytes(totalBytes)} total\n${lines.join("\n")}`;
}

async function readInjectedUploads() {
  const seen = new Set(["input/task.md"]);
  const totalBytes = state.uploads.reduce((sum, upload) => sum + upload.file.size, 0);
  if (totalBytes > maxUploadBytes) {
    throw new Error(`Selected uploads are ${formatBytes(totalBytes)}; limit is ${formatBytes(maxUploadBytes)} for the JSON UI path.`);
  }

  const injected = [];
  for (const upload of state.uploads) {
    validateRelativePath(upload.relativePath);
    if (seen.has(upload.relativePath)) {
      throw new Error(`Duplicate injected path: ${upload.relativePath}`);
    }
    seen.add(upload.relativePath);
    injected.push({
      relative_path: upload.relativePath,
      content: encodeFileContent(new Uint8Array(await upload.file.arrayBuffer())),
      mode: 420,
    });
  }
  return injected;
}

// Text files travel as raw UTF-8 strings; only non-UTF-8 bytes keep the far
// larger JSON byte-array encoding.
function encodeFileContent(bytes) {
  try {
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    return Array.from(bytes);
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
    case "waiting_for_slot":
      appendEvent("queued", `Position ${event.position} (queue limit ${event.max_waiting})`);
      break;
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

function joinRelativePath(prefix, path) {
  const pieces = [prefix, path].filter(Boolean).join("/");
  return normalizeRelativePath(pieces);
}

function normalizeRelativePath(path) {
  return path.replaceAll("\\", "/").split("/").filter((part) => part && part !== ".").join("/");
}

function validateRelativePath(path) {
  if (!path || path.startsWith("/") || path.includes("\0")) {
    throw new Error(`Invalid injected path: ${path}`);
  }
  for (const part of path.split("/")) {
    if (part === "..") throw new Error(`Injected path cannot escape workspace: ${path}`);
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}

init();
