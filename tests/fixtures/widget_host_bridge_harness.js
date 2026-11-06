// Drives the widget's inline script against a stub DOM and a fake MCP Apps
// host, then prints a JSON report of what the widget posted to the host.
//
// Usage: node widget_host_bridge_harness.js <rendered-widget.html> <tool-result.json>
//
// The stub DOM absorbs every property access, so only the host-bridge
// contract is asserted here, not rendering.
"use strict";
const fs = require("fs");
const vm = require("vm");

const html = fs.readFileSync(process.argv[2], "utf8");
const toolResult = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const scripts = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]);
const source = scripts[scripts.length - 1];

function anything() {
  const fn = function () {
    return anything();
  };
  return new Proxy(fn, {
    get(_target, key) {
      if (key === Symbol.toPrimitive) return () => "";
      if (key === Symbol.iterator) return function* () {};
      if (key === "length") return 0;
      if (key === "then") return undefined;
      return anything();
    },
    set() {
      return true;
    },
    apply() {
      return anything();
    },
  });
}

function absorbing(target) {
  return new Proxy(target, {
    get(t, key) {
      return key in t ? t[key] : anything();
    },
    set(t, key, value) {
      t[key] = value;
      return true;
    },
  });
}

const posted = [];
const listeners = {};
const frames = [];
const observed = [];
const parent = { postMessage: (message, origin) => posted.push({ message, origin }) };
const root = absorbing({ innerHTML: "" });
const documentElement = absorbing({
  style: { height: "", setProperty() {}, removeProperty() {} },
  getBoundingClientRect: () => ({ height: 321.2 }),
});
const document = absorbing({
  documentElement,
  body: anything(),
  getElementById: (id) => (id === "root" ? root : anything()),
  querySelector: () => null,
  querySelectorAll: () => [],
});
const window = {
  parent,
  innerWidth: 640.4,
  document,
  addEventListener: (type, cb) => (listeners[type] = listeners[type] || []).push(cb),
  removeEventListener() {},
  localStorage: { getItem: () => null, setItem() {} },
  matchMedia: () => ({ matches: false, addEventListener() {} }),
};
const context = {
  window,
  document,
  console,
  setTimeout: () => 0,
  clearTimeout() {},
  setInterval: () => 0,
  clearInterval() {},
  requestAnimationFrame: (cb) => frames.push(cb),
  cancelAnimationFrame() {},
  ResizeObserver: function () {
    this.observe = (element) => observed.push(element);
    this.disconnect = () => {};
  },
  localStorage: window.localStorage,
  navigator: { clipboard: anything() },
  fetch: () => new Promise(() => {}),
  Image: function () {},
  CustomEvent: function () {},
  performance: { now: () => 0 },
  getComputedStyle: () => anything(),
};
window.window = window;
for (const key of Object.keys(context)) {
  if (!(key in window)) window[key] = context[key];
}
vm.createContext(context);
vm.runInContext(source, context);

const deliver = (data, from = parent) => (listeners.message || []).forEach((cb) => cb({ source: from, data }));
const flushFrames = () => frames.splice(0).forEach((cb) => cb());
const messagesFrom = (index) => posted.slice(index).map((entry) => entry.message);

const report = { boot: messagesFrom(0) };
const initialize = report.boot[0] || {};

// A response from anything but the host frame is ignored.
deliver({ jsonrpc: "2.0", id: initialize.id, result: {} }, {});
report.postedAfterForeignResponse = posted.length;

let mark = posted.length;
deliver({
  jsonrpc: "2.0",
  id: initialize.id,
  result: { protocolVersion: "2026-01-26", hostInfo: { name: "host", version: "1" }, hostCapabilities: {}, hostContext: {} },
});
flushFrames();
report.afterInitializeResult = messagesFrom(mark);
report.resizeTargets = observed.length;

// A repeated response must not re-confirm initialization.
mark = posted.length;
deliver({ jsonrpc: "2.0", id: initialize.id, result: {} });
report.afterDuplicateResult = messagesFrom(mark);

// The tool result rides the notification itself; `window.openai` is absent.
const htmlBeforeResult = root.innerHTML;
deliver({ jsonrpc: "2.0", method: "ui/notifications/tool-result", params: toolResult });
report.toolResultRendered = root.innerHTML !== htmlBeforeResult && root.innerHTML !== "";

mark = posted.length;
deliver({ jsonrpc: "2.0", id: 7, method: "ui/resource-teardown", params: {} });
report.afterTeardown = messagesFrom(mark);
report.origins = [...new Set(posted.map((entry) => entry.origin))];

console.log(JSON.stringify(report));
