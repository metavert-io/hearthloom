/* ember.js — the page-side half of Hearthloom's ember handshake, served by
   every node at /assets/ember.js (pre-auth, like chart-svg.js). An ember's
   page loads it with one script tag instead of carrying its own copy:

     <script src="/assets/ember.js"></script>
     <script>
       window.hearth.then(async (h) => { const n = await h.kv("count"); … });
     </script>

   The dashboard renders ember pages in <iframe sandbox="allow-scripts"> and
   posts {type:"hearthloom", token, palette} into the frame once, when the
   page has loaded (its half is /assets/ember-shell.js). `window.hearth`
   resolves with a small client bound to that token: the only authority the
   page has.

   Who may talk to the page is decided here, once, for every ember:
   - only the window that embeds this page (not a sibling frame, not a
     window that opened it), and only when that window is the dashboard
     itself: the node serves both, so the dashboard's origin is this page's
     URL origin. A site that frames the page can't pass, and the node
     forbids that framing anyway (frame-ancestors 'self');
   - the client always talks to the node that served this page
     (location.origin), whatever a message says, so a forged handshake can
     never point the page at another server;
   - links to other sites open in a new tab through the dashboard, so the
     frame never leaves the ember's page (the token is handed over once,
     on the first load, and never again). */
(() => {
  const VERSION = 1;
  const framed = window.parent !== window;
  // The sandboxed page's own origin is opaque; location.origin is still its
  // URL's origin, which is the node's (and so the dashboard's).
  const node = location.origin;
  const fromShell = (ev) => framed && ev.source === window.parent && ev.origin === node;
  // Addressed to the dashboard's origin, so nothing else that might be
  // holding this frame can read what the page says.
  const tell = (msg) => {
    if (!framed) return;
    try { parent.postMessage(msg, node); } catch { /* the parent went away */ }
  };

  // The Dial's time window (pages whose manifest entry says `dial: true`).
  let span = null;
  const spanListeners = [];
  addEventListener("message", (ev) => {
    if (!fromShell(ev) || ev.data?.type !== "hearthloom-timespan") return;
    span = { from: ev.data.from, to: ev.data.to };
    for (const fn of spanListeners) fn(span.from, span.to);
  });

  const open = (url) => {
    let href;
    try { href = new URL(url, location.href).href; } catch { return; }
    tell({ type: "hearthloom-open", url: href });
  };

  window.hearth = new Promise((resolve) => {
    const onHandshake = (ev) => {
      if (!fromShell(ev) || ev.data?.type !== "hearthloom") return;
      removeEventListener("message", onHandshake);
      const d = ev.data;
      for (const [k, v] of Object.entries(d.palette || {})) {
        if (v) document.documentElement.style.setProperty(k, v);
      }
      document.documentElement.dataset.framed = "1";
      const headers = { authorization: `Bearer ${d.token}` };
      const url = (p) => `${node}${p}`;
      const check = async (r) => {
        if (!r.ok) {
          let msg = `${r.status}`;
          try { msg = (await r.text()) || msg; } catch { /* keep the status */ }
          const e = new Error(msg);
          e.status = r.status;
          throw e;
        }
        return r;
      };
      const parsed = (text) => { try { return JSON.parse(text); } catch { return text; } };
      const file = async (path) => {
        const r = await fetch(url(`/v1/plugins/self/files/${String(path).split("/").map(encodeURIComponent).join("/")}`),
          { headers, cache: "no-store" });
        if (r.status === 404) return null;
        await check(r);
        return parsed(await r.text());
      };
      resolve({
        version: VERSION,
        base: node,
        hasToken: !!d.token,
        /** Your ember's key-value store: the stored value (JSON-decoded when
            it is JSON), or null when the key is unset. */
        async kv(key) {
          const r = await fetch(url(`/v1/plugins/self/kv/${encodeURIComponent(key)}`), { headers, cache: "no-store" });
          if (r.status === 404) return null;
          await check(r);
          const v = (await r.json()).value;
          return typeof v === "string" ? parsed(v) : v;
        },
        async kvPut(key, value) {
          await check(await fetch(url(`/v1/plugins/self/kv/${encodeURIComponent(key)}`), {
            method: "PUT", headers, body: JSON.stringify(value),
          }));
        },
        /** A file your program wrote under its data folder: JSON-decoded
            when it is JSON, else text; null when it doesn't exist. */
        file,
        fileGet: file,
        /** The names of your ember's secrets (never their values). */
        async secrets() {
          return (await check(await fetch(url("/v1/plugins/self/secrets"), { headers, cache: "no-store" }))).json();
        },
        /** Store a secret your program can read; this page never can. */
        async secretPut(key, text) {
          await check(await fetch(url(`/v1/plugins/self/secrets/${encodeURIComponent(key)}`), {
            method: "PUT", headers, body: text,
          }));
        },
        /** Register a pending OAuth sign-in, then call signIn(). */
        async oauthPending(state, authorizeUrl, ttlSecs = 900) {
          return (await check(await fetch(url("/v1/plugins/self/oauth/pending"), {
            method: "POST",
            headers: { ...headers, "content-type": "application/json" },
            body: JSON.stringify({ state, authorize_url: authorizeUrl, ttl_secs: ttlSecs }),
          }))).json();
        },
        /** Ask the dashboard to open the sign-in the daemon holds for you. */
        signIn() { tell({ type: "hearthloom-oauth" }); },
        /** Open a link in a new tab (the sandbox has no popups). */
        open,
        /** Follow the Dial's window: fn(from, to), now and on every change. */
        onTimespan(fn) {
          spanListeners.push(fn);
          if (span) setTimeout(() => fn(span.from, span.to));
        },
      });
    };
    addEventListener("message", onHandshake);
  });

  // Links to other sites open in a new tab through the dashboard instead of
  // replacing the ember's page inside the frame.
  addEventListener("click", (ev) => {
    const a = ev.target instanceof Element ? ev.target.closest("a[href]") : null;
    if (!a || ev.defaultPrevented) return;
    let target;
    try { target = new URL(a.getAttribute("href"), location.href); } catch { return; }
    if (target.origin === node || !["http:", "https:"].includes(target.protocol)) return;
    ev.preventDefault();
    open(target.href);
  }, true);

  // Report the content height so the dashboard sizes the frame to fit (no
  // inner scrollbar: the page flows in the dashboard's own scroll).
  const report = () => tell({ type: "hearthloom-height", px: Math.ceil(document.documentElement.scrollHeight) });
  addEventListener("load", report);
  if (window.ResizeObserver) {
    const ro = new ResizeObserver(report);
    ro.observe(document.documentElement);
    addEventListener("DOMContentLoaded", () => document.body && ro.observe(document.body));
  }
  setInterval(report, 1000);
})();
