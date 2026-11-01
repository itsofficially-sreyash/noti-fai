// One native port per tab with a ball on screen. The port is opened only
// while that ball exists, so nothing runs on the desktop side otherwise.
const HOST = "com.notifai.host";
const ports = new Map(); // tabId -> Port
const icons = new Map(); // agent -> base64 of the bundled icons/<agent>.png

async function bundledIcon(agent) {
  if (!icons.has(agent)) {
    let b64 = "";
    try {
      const bytes = new Uint8Array(await (await fetch(chrome.runtime.getURL(`icons/${agent}.png`))).arrayBuffer());
      b64 = btoa(Array.from(bytes, (b) => String.fromCharCode(b)).join(""));
    } catch {
      // no bundled icon: the desktop side falls back to its own
    }
    icons.set(agent, b64);
  }
  return icons.get(agent);
}

function close(tabId) {
  ports.get(tabId)?.disconnect();
  ports.delete(tabId);
}

chrome.runtime.onMessage.addListener((msg, sender) => {
  const tab = sender.tab;
  if (!tab) return;
  close(tab.id);
  if (msg.type !== "done") return; // "seen": the user came back to the tab

  const port = chrome.runtime.connectNative(HOST);
  ports.set(tab.id, port);
  port.onMessage.addListener((reply) => {
    if (reply.event === "click") {
      chrome.tabs.update(tab.id, { active: true });
      chrome.windows.update(tab.windowId, { focused: true });
    }
    close(tab.id);
  });
  port.onDisconnect.addListener(() => {
    if (ports.get(tab.id) === port) ports.delete(tab.id);
  });
  bundledIcon(msg.agent).then((icon) => port.postMessage({ agent: msg.agent, icon }));
});

chrome.tabs.onRemoved.addListener(close);
