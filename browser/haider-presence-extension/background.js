// Haider browser presence (MV3 service worker).
//
// Contract with haiderd (webextract-2): a Native Messaging port named
// "ai.diffforge.haider.browser" carries JSON messages:
//   daemon -> extension {op:"open", url}         open an agent tab in the Haider group and attach
//                       {op:"cdp", tabId, method, params}  forwarded to chrome.debugger.sendCommand
//                       {op:"release"}           detach every agent tab (run ended / idle / Stop)
//   extension -> daemon {event:"attached", tabId}
//                       {event:"cdp_result", id, result|error}
//                       {event:"stop", surface:"browser", tabId, reason:"canceled_by_user"}
//                       {event:"presence", surface:"browser", state:"shown"|"hidden"}
// The daemon maps {event:"stop"} to PresenceEvent::Stop for PresenceSurface::Browser,
// which cancels the run exactly like the desktop overlay's Stop.
import { BrowserPresence, DEBUGGER_PROTOCOL, GROUP_COLOR, GROUP_TITLE } from "./presence.js";

const presence = new BrowserPresence();
const recent = [];
let port = null;

function send(message) {
  console.log("haider-presence", JSON.stringify(message));
  if (port) port.postMessage(message);
  // Bounded diagnostic trail (also what the proof harness reads back).
  recent.push(message);
  if (recent.length > 32) recent.shift();
  chrome.storage.local.set({ events: recent }).catch(() => {});
}

async function ensureGroup(tabId) {
  if (presence.groupId !== null) {
    try {
      await chrome.tabs.group({ groupId: presence.groupId, tabIds: [tabId] });
      return;
    } catch (_) {
      presence.groupId = null;
    }
  }
  presence.groupId = await chrome.tabs.group({ tabIds: [tabId] });
  await chrome.tabGroups.update(presence.groupId, { title: GROUP_TITLE, color: GROUP_COLOR, collapsed: false });
}

export async function openAgentTab(url) {
  const tab = await chrome.tabs.create({ url, active: true });
  await ensureGroup(tab.id);
  // Attaching chrome.debugger makes Chrome show its own, un-spoofable bar:
  // "“Haider” started debugging this browser  [Cancel]".
  await chrome.debugger.attach({ tabId: tab.id }, DEBUGGER_PROTOCOL);
  presence.attached(tab.id).forEach(send);
  send({ event: "attached", tabId: tab.id });
  return tab.id;
}

export async function releaseAll() {
  for (const tabId of presence.controlledTabs()) {
    try {
      await chrome.debugger.detach({ tabId });
    } catch (_) {
      // Already detached.
    }
    presence.detached(tabId, "released").forEach(send);
  }
}

chrome.debugger.onDetach.addListener((source, reason) => {
  if (source.tabId === undefined) return;
  const events = presence.detached(source.tabId, reason);
  events.forEach(send);
  if (events.some((event) => event.event === "stop")) {
    // A Stop on one tab stops the whole run: release every other agent tab.
    releaseAll();
  }
});

async function onMessage(message) {
  if (message.op === "open") return openAgentTab(message.url);
  if (message.op === "release") return releaseAll();
  if (message.op === "cdp") {
    try {
      const result = await chrome.debugger.sendCommand({ tabId: message.tabId }, message.method, message.params || {});
      send({ event: "cdp_result", id: message.id, result });
    } catch (error) {
      send({ event: "cdp_result", id: message.id, error: String(error) });
    }
  }
  return undefined;
}

export const NATIVE_HOST = "ai.diffforge.haider.browser";

/**
 * Makes a missing daemon connection visible instead of silently dropping
 * every message: a red "!" badge and tooltip on the toolbar icon, plus the
 * diagnostic trail. The native host itself is webextract-2's deliverable.
 */
function reportHostProblem(message) {
  console.error("haider-presence: native host unavailable:", message);
  send({ event: "error", surface: "browser", message: `native host ${NATIVE_HOST} unavailable: ${message}` });
  chrome.action?.setBadgeText({ text: "!" }).catch(() => {});
  chrome.action?.setBadgeBackgroundColor({ color: "#E5484D" }).catch(() => {});
  chrome.action
    ?.setTitle({ title: `Haider: cannot reach the Haider daemon (${message})` })
    .catch(() => {});
}

function connectHost() {
  try {
    port = chrome.runtime.connectNative(NATIVE_HOST);
  } catch (error) {
    port = null;
    reportHostProblem(String(error));
    return;
  }
  port.onMessage.addListener((message) => onMessage(message));
  port.onDisconnect.addListener(() => {
    // Chrome reports a host that is missing, not allowed or crashed here.
    const reason = chrome.runtime.lastError?.message || "disconnected";
    port = null;
    reportHostProblem(reason);
  });
}

// Self-test (proof harness only): loaded unpacked with a `selftest.json`
// beside the manifest, the worker opens one agent tab on start so the tab
// group and Chrome's debugging bar can be observed without haiderd.
async function maybeSelfTest() {
  try {
    const response = await fetch(chrome.runtime.getURL("selftest.json"));
    if (!response.ok) return;
    const { url } = await response.json();
    await openAgentTab(url || "about:blank");
  } catch (_) {
    // No self-test file: production path.
  }
}

chrome.runtime.onStartup.addListener(maybeSelfTest);
chrome.runtime.onInstalled.addListener(maybeSelfTest);
connectHost();
