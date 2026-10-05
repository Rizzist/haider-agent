// Browser presence for Haider (webextract-2 integration contract, docs/cu-presence.md).
//
// Pure logic, no chrome.* access, so it is unit-testable in Node:
//   node --test browser/haider-presence-extension/presence.test.mjs

/** Tab-group identity for every tab Haider controls. */
export const GROUP_TITLE = "Haider";
export const GROUP_COLOR = "orange";
/** CDP protocol version passed to chrome.debugger.attach. */
export const DEBUGGER_PROTOCOL = "1.3";

/**
 * chrome.debugger.onDetach reasons that mean "the human stopped Haider".
 * "canceled_by_user" is the infobar's Cancel button; "target_closed" is the
 * human closing an agent tab, which also ends control of that tab.
 */
export function detachMeansStop(reason) {
  return reason === "canceled_by_user";
}

/**
 * Tracks the tabs Haider controls. Mirrors the daemon's PresenceMachine for
 * the Browser surface: the first attach shows presence, the last detach (or
 * a Stop) hides it.
 */
export class BrowserPresence {
  constructor() {
    this.tabs = new Set();
    this.groupId = null;
    this.stopped = false;
  }

  attached(tabId) {
    this.stopped = false;
    const first = this.tabs.size === 0;
    this.tabs.add(tabId);
    return first ? [{ event: "presence", surface: "browser", state: "shown" }] : [];
  }

  /** Returns the events to send to haiderd for a debugger detach. */
  detached(tabId, reason) {
    const had = this.tabs.delete(tabId);
    const events = [];
    if (had && detachMeansStop(reason) && !this.stopped) {
      this.stopped = true;
      events.push({ event: "stop", surface: "browser", tabId, reason });
    }
    if (had && this.tabs.size === 0) {
      events.push({ event: "presence", surface: "browser", state: "hidden", reason });
      this.groupId = null;
    }
    return events;
  }

  /** Every tab still attached, for detach-all on Stop/Hide. */
  controlledTabs() {
    return [...this.tabs];
  }
}
