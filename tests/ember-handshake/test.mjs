// The ember handshake in a real browser: the dashboard's half
// (assets/ember-shell.js) and the page's half (assets/ember.js) — the real
// files — against every sender that must never be believed. CI runs it
// (ci.yml, "Ember handshake"); locally: npm ci && node test.mjs
// (PW_CHANNEL=chrome uses an installed Chrome instead of Playwright's).
//
// What the node itself adds — frame-ancestors 'self' on ember pages and
// refusing them as a tab of their own — is tested in http.rs; the pages
// here are served by this harness, so those checks are exercised only
// where a case says so.
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const here = path.dirname(fileURLToPath(import.meta.url));
const assets = process.env.ASSETS || path.resolve(here, "../../crates/hearthloom-daemon/assets");
const NODE_PORT = Number(process.env.NODE_PORT || 47901);
const OTHER_PORT = Number(process.env.OTHER_PORT || 47902);
const NODE = `http://127.0.0.1:${NODE_PORT}`; // the node: dashboard + ember pages
const OTHER = `http://127.0.0.1:${OTHER_PORT}`; // somewhere else

const forged = `{ type: "hearthloom", token: "FORGED", base: "${OTHER}" }`;
const emberPage = `<!doctype html><title>ember</title>
<a id="out" href="${OTHER}/article.html">a link to another site</a>
<script src="/assets/ember.js"></script>
<script>
  window.__got = null;
  window.hearth.then((h) => { window.__got = { base: h.base, hasToken: h.hasToken, version: h.version }; });
</script>`;
const dashboard = (body) => `<!doctype html><title>dashboard</title>
<iframe id="ember" sandbox="allow-scripts"></iframe>
<script type="module">
  import { handOver, openLinks } from "/assets/ember-shell.js";
  const frame = document.getElementById("ember");
  ${body}
  frame.src = "/plugins/demo/ui/index.html";
</script>`;

const nodePages = {
  "/plugins/demo/ui/index.html": [emberPage, { "content-security-policy": "frame-ancestors 'self'" }],
  "/plugins/demo/ui/nocsp.html": [emberPage, {}],
  // the real handshake
  "/dash.html": [dashboard(`handOver(frame, { token: "EMBER-TOKEN" }); openLinks(frame);`), {}],
  // the dashboard itself, saying the API lives elsewhere
  "/dash-badbase.html": [dashboard(`frame.addEventListener("load", () =>
    frame.contentWindow.postMessage({ type: "hearthloom", token: "T", base: "${OTHER}" }, "*"), { once: true });`), {}],
  // no handshake from the dashboard; a sibling frame from another site forges one
  "/dash-sibling.html": [dashboard(``).replace("</script>", `</script>
<iframe sandbox="allow-scripts" src="${OTHER}/sibling.html"></iframe>`), {}],
};
const otherPages = {
  "/article.html": `<!doctype html><p>another site</p>
<script>window.__msgs = []; addEventListener("message", (e) => window.__msgs.push(e.data));</script>`,
  "/sibling.html": `<script>setTimeout(() => parent.frames[0].postMessage(${forged}, "*"), 300);</script>`,
  "/opener.html": `<script>
  const w = window.open("${NODE}/plugins/demo/ui/nocsp.html");
  setTimeout(() => w.postMessage(${forged}, "*"), 600);
</script>`,
  "/frame-nocsp.html": `<iframe id="f" src="${NODE}/plugins/demo/ui/nocsp.html"></iframe>
<script>setTimeout(() => document.getElementById("f").contentWindow.postMessage(${forged}, "*"), 600);</script>`,
  "/frame-csp.html": `<iframe id="f" src="${NODE}/plugins/demo/ui/index.html"></iframe>`,
};

const serve = (port, route) => new Promise((resolve) => {
  const server = http.createServer((req, res) => {
    const url = req.url.split("?")[0];
    const hit = route(url);
    if (!hit) { res.writeHead(404); res.end(); return; }
    const [body, headers, type] = hit;
    res.writeHead(200, { "content-type": type || "text/html; charset=utf-8", ...headers });
    res.end(body);
  }).listen(port, "127.0.0.1", () => resolve(server));
});
const nodeServer = await serve(NODE_PORT, (url) => {
  if (url === "/assets/ember.js" || url === "/assets/ember-shell.js") {
    return [fs.readFileSync(path.join(assets, url.slice("/assets/".length))), {}, "text/javascript"];
  }
  return nodePages[url];
});
const otherServer = await serve(OTHER_PORT, (url) => otherPages[url] && [otherPages[url], {}]);

const browser = await chromium.launch({ headless: true, channel: process.env.PW_CHANNEL || undefined });
const context = await browser.newContext();
const failures = [];
const check = (name, ok, detail) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${name}${ok ? "" : ` — ${detail}`}`);
  if (!ok) failures.push(name);
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const emberFrame = (page) => page.frames().find((f) => f.url().includes("/plugins/demo/ui/"));
const got = async (frame) => (frame ? frame.evaluate(() => window.__got) : "no ember frame");

try {
  // 1 · the dashboard's handshake is believed, and the client talks to the node
  {
    const page = await context.newPage();
    await page.goto(`${NODE}/dash.html`);
    await sleep(800);
    const g = await got(emberFrame(page));
    check("the dashboard's handshake is accepted", g?.hasToken === true && g?.base === NODE, JSON.stringify(g));

    // 2 · a link to another site opens a new tab; the frame stays on the ember
    const popup = context.waitForEvent("page", { timeout: 5000 }).catch(() => null);
    await emberFrame(page).click("#out");
    const tab = await popup;
    if (tab) await tab.waitForURL(`${OTHER}/article.html`, { timeout: 5000 }).catch(() => {});
    check("an outbound link opens in a new tab", tab?.url() === `${OTHER}/article.html`, tab?.url() ?? "no tab");
    check("the frame stays on the ember's page", !!emberFrame(page), page.frames().map((f) => f.url()).join(", "));
    check("the new tab can't reach the dashboard", (await tab?.evaluate(() => window.opener === null)) === true, "window.opener is set");
    await tab?.close();

    // 3 · a frame that has moved on to another site is never handed the token
    await emberFrame(page).evaluate((u) => { location.href = u; }, `${OTHER}/article.html`).catch(() => {});
    await sleep(1200);
    const moved = page.frames().find((f) => f.url() === `${OTHER}/article.html`);
    const msgs = moved ? await moved.evaluate(() => window.__msgs) : null;
    check("a navigated frame gets no handshake", Array.isArray(msgs) && msgs.length === 0, JSON.stringify(msgs));
    await page.close();
  }
  // 4 · the API address is the node's, whatever a message says
  {
    const page = await context.newPage();
    await page.goto(`${NODE}/dash-badbase.html`);
    await sleep(800);
    const g = await got(emberFrame(page));
    check("a message can't move the API elsewhere", g?.base === NODE, JSON.stringify(g));
    await page.close();
  }
  // 5 · a sibling frame can't hand the page a token
  {
    const page = await context.newPage();
    await page.goto(`${NODE}/dash-sibling.html`);
    await sleep(1200);
    const g = await got(emberFrame(page));
    check("a sibling frame is refused", g === null, JSON.stringify(g));
    await page.close();
  }
  // 6 · a window that opened the page can't either
  {
    const page = await context.newPage();
    const popup = context.waitForEvent("page", { timeout: 5000 });
    await page.goto(`${OTHER}/opener.html`);
    const opened = await popup;
    await sleep(1200);
    const g = await opened.evaluate(() => window.__got);
    check("an opener is refused", g === null, JSON.stringify(g));
    await opened.close();
    await page.close();
  }
  // 7 · nor a site that frames the page (the ember.js check alone)
  {
    const page = await context.newPage();
    await page.goto(`${OTHER}/frame-nocsp.html`);
    await sleep(1200);
    const g = await got(emberFrame(page));
    check("a framing site is refused", g === null, JSON.stringify(g));
    await page.close();
  }
  // 8 · and with the node's header, such a site can't even load the page
  {
    const page = await context.newPage();
    await page.goto(`${OTHER}/frame-csp.html`);
    await sleep(800);
    const f = emberFrame(page);
    const loaded = f ? await f.evaluate(() => typeof window.hearth).catch(() => "blocked") : "blocked";
    check("frame-ancestors keeps the page out of other sites", loaded !== "object", loaded);
    await page.close();
  }
} finally {
  await browser.close();
  nodeServer.close();
  otherServer.close();
}
if (failures.length) {
  console.error(`\n${failures.length} check(s) failed`);
  process.exit(1);
}
console.log("\nall checks passed");
