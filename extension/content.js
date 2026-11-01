// Watches an AI chat page for the end of a reply. Every one of these sites
// shows a "Stop" button while it generates, so a reply has finished when that
// button goes away (and stays away briefly: tool calls can blink it).
(() => {
  const AGENTS = {
    "claude.ai": "claude",
    "chatgpt.com": "chatgpt",
    "grok.com": "grok",
    "gemini.google.com": "gemini",
    "www.perplexity.ai": "perplexity",
  };
  const agent = AGENTS[location.hostname];
  const STOP = 'button[aria-label*="stop" i], [data-testid="stop-button"]';
  const NOT_GENERATION = /record|dictat|listen|voice|speak|read aloud/i;
  const SETTLE_MS = 1000;

  let generating = false;
  let notified = false;
  let pending = 0;
  let lastCheck = 0;
  let doneTimer = 0;

  const isGenerating = () =>
    [...document.querySelectorAll(STOP)].some(
      (b) => !NOT_GENERATION.test(b.getAttribute("aria-label") || "")
    );
  const lookingAtIt = () => document.visibilityState === "visible" && document.hasFocus();

  function send(msg) {
    try {
      chrome.runtime.sendMessage(msg).catch(() => {});
    } catch {
      // extension was reloaded; this page's script is orphaned
    }
  }

  function check() {
    pending = 0;
    lastCheck = performance.now();
    const now = isGenerating();
    if (now === generating) return;
    generating = now;
    clearTimeout(doneTimer);
    if (!now) doneTimer = setTimeout(finished, SETTLE_MS);
  }

  // Streaming fires many mutations; look at most every 250 ms, but always
  // once more after the last one so the final state is never missed.
  function schedule() {
    if (!pending) pending = setTimeout(check, Math.max(0, 250 - (performance.now() - lastCheck)));
  }

  function finished() {
    if (generating || lookingAtIt()) return;
    notified = true;
    send({ type: "done", agent });
  }

  function seen() {
    if (notified && lookingAtIt()) {
      notified = false;
      send({ type: "seen" });
    }
  }

  if (!agent) return;
  new MutationObserver(schedule).observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
    attributeFilter: ["aria-label", "data-testid"],
  });
  addEventListener("focus", seen);
  document.addEventListener("visibilitychange", seen);
  check();
})();
