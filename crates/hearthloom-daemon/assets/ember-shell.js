// ember-shell.js — the dashboard's half of the ember handshake (the page's
// half is /assets/ember.js). Plugin pages (app.js) and Desk widgets
// (desk.js) both go through it, so the rules live in one place:
// - The handshake carries the ember's 12-hour UI token. A sandboxed frame
//   has an opaque origin, so the message can only be addressed "*" — which
//   is why it goes out ONCE, on the first load after the frame's src is
//   set. A frame that has since loaded something else (a link inside the
//   ember's page) must never be handed the token.
// - "hearthloom-open" asks the dashboard to open a link in a new tab (the
//   sandbox allows no popups): http(s) only, never into this window.

const PALETTE_VARS = ["--bg", "--ink", "--ink-faint", "--accent", "--card", "--line",
  "--chart-1", "--chart-2", "--chart-3", "--chart-4",
  "--chart-5", "--chart-6", "--chart-7", "--chart-8", "--chart-heat"];

/** The dashboard's colours, so an ember page can match the theme. */
export function palette() {
  const styles = getComputedStyle(document.documentElement);
  const out = {};
  for (const v of PALETTE_VARS) out[v] = styles.getPropertyValue(v).trim();
  return out;
}

/** Hand `frame` its handshake once, when its next document has loaded.
    Attach before setting the frame's src. `token` is a value, a promise or
    an async function (fetched at load); a failure means no token. `then`
    are further messages for the same load (the Dial's first window). */
export function handOver(frame, { token = null, then = [] } = {}) {
  frame.addEventListener("load", async () => {
    let t = null;
    try { t = typeof token === "function" ? await token() : await token; } catch { /* tokenless */ }
    const win = frame.contentWindow;
    if (!win) return;
    // "*": sandboxed frames have an opaque origin, the only address there is.
    win.postMessage({ type: "hearthloom", token: t, base: location.origin, palette: palette() }, "*");
    for (const msg of then) win.postMessage(msg, "*");
  }, { once: true });
}

/** Open `url` in a new tab with no way back to this window. False when the
    browser blocked it. (window.open with "noopener" always returns null, so
    it can't say whether the tab opened; this can.) */
export function openExternal(url) {
  const win = window.open("", "_blank");
  if (!win) return false;
  win.opener = null;
  win.location.href = url;
  return true;
}

/** Open the http(s) links the page in `frame` asks for. `onBlocked(url)`
    runs when the browser blocks the tab. Stops by itself once the frame is
    gone; also returns a stop function. */
export function openLinks(frame, { onBlocked } = {}) {
  const on = (ev) => {
    if (!frame.isConnected) { window.removeEventListener("message", on); return; }
    if (ev.source !== frame.contentWindow || ev.data?.type !== "hearthloom-open") return;
    let url;
    try { url = new URL(String(ev.data.url)); } catch { return; }
    if (url.protocol !== "https:" && url.protocol !== "http:") return;
    if (!openExternal(url.href) && onBlocked) onBlocked(url.href);
  };
  window.addEventListener("message", on);
  return () => window.removeEventListener("message", on);
}
