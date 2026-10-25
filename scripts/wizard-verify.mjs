import { chromium } from "playwright";

const BASE = "http://127.0.0.1:43299";
const errors = [];
const browser = await chromium.launch({ channel: "chrome", headless: true });
const page = await browser.newPage({ deviceScaleFactor: 2 });
page.on("pageerror", (e) => errors.push(`pageerror: ${e.stack || e.message}`));
page.on("console", (m) => { if (m.type() === "error") errors.push(`console: ${m.text()}`); });

const check = (name, ok) => console.log(`${ok ? "✓" : "✗ FAIL"} ${name}`);

// 1 — empty brain redirects to #/welcome
await page.goto(BASE + "/", { waitUntil: "networkidle" });
await page.waitForURL("**/#/welcome**", { timeout: 15000 }).catch(() => {});
check("empty brain lands on #/welcome", page.url().includes("#/welcome"));
await page.waitForSelector("#wz-admin-email", { timeout: 10000 }).catch(() => {});

// 2 — protect-this-brain card present (auth open, no provisioner)
const protect = await page.locator("text=protect this brain").count();
check("protect-this-brain step shown", protect > 0);

// 3 — Integrations cards mounted: AI provider, Gmail, pair-a-device
await page.waitForTimeout(1500);
check("AI provider card mounted", await page.locator("#wz-ai .card:has-text('AI provider')").count() > 0);
check("Gmail card mounted", await page.locator("#wz-gmail .card:has-text('Google Mail')").count() > 0);
check("pair-a-device card mounted", await page.locator("#wz-device .card:has-text('Pair a device')").count() > 0);

// 4 — QR mints and draws
await page.click("#pair-mint");
await page.waitForSelector("#pair-out svg", { timeout: 10000 });
check("QR svg rendered", await page.locator("#pair-out svg").count() > 0);

await page.screenshot({ path: "/tmp/hl-ui-verify/wizard-top.png", fullPage: true });

// 5 — create the admin login
await page.fill("#wz-admin-email", "wizard-test@example.com");
await page.fill("#wz-admin-pass", "hunter2hunter2");
await page.fill("#wz-admin-pass2", "hunter2hunter2");
await page.click("#wz-admin-create");
// argon2id on a debug build takes seconds — wait for the outcome, not a beat
await page.waitForFunction(() =>
  /[✓✗]/.test(document.querySelector("#wz-admin-note")?.textContent || ""),
  { timeout: 30000 });
const note = await page.textContent("#wz-admin-note");
check(`bootstrap note: "${note?.trim().slice(0, 60)}"`, note?.includes("✓"));

// 6 — auth is now required; session cookie keeps the wizard signed in
const me = await page.evaluate(() => fetch("/v1/auth/me").then((r) => r.json()));
check(`auth now required (auth=${me.auth}, user=${me.user})`, me.auth === "required" && !!me.user);

// 7 — a fresh, cookie-less context hits the login wall
const anon = await browser.newContext();
const p2 = await anon.newPage();
await p2.goto(BASE + "/", { waitUntil: "networkidle" });
await p2.waitForSelector("#login-user", { timeout: 10000 }).catch(() => {});
check("anonymous visitor sees Sign in", await p2.locator("text=Sign in").count() > 0);
await p2.screenshot({ path: "/tmp/hl-ui-verify/wizard-login-wall.png" });

// 8 — re-render for a signed-in admin: protect card gone (auth no longer open)
await page.reload({ waitUntil: "networkidle" });
await page.waitForTimeout(2000);
check("protect step hidden once admin exists", await page.locator("text=protect this brain").count() === 0);
await page.screenshot({ path: "/tmp/hl-ui-verify/wizard-after-bootstrap.png", fullPage: true });

// — 0.5.56 —

// 9 — semantic-search download card renders in the wizard (fresh node:
// not installed, download button offered)
check("embeddings card mounted", await page.locator("#wz-embeddings .card:has-text('Semantic search')").count() > 0);
check("embeddings download offered", await page.locator("#wz-embeddings #emb-dl").count() > 0);

// 10 — git auto-discovery: scan lists repos with checkboxes (walks the
// daemon machine's home — give it a minute)
await page.click("#wz-git-scan");
await page.waitForFunction(() => {
  const el = document.querySelector("#wz-git-found");
  return el && (el.querySelector("input[type=checkbox]") || /no repos|none/i.test(el.textContent));
}, { timeout: 90000 });
const repoBoxes = await page.locator("#wz-git-found input[type=checkbox]").count();
check(`repo scan lists checkboxes (${repoBoxes} repos)`, repoBoxes > 0);
await page.screenshot({ path: "/tmp/hl-ui-verify/wizard-git-scan.png", fullPage: true });

// 11 — Desk "recently deleted" drawer restores a soft-deleted task
const taskId = await page.evaluate(async () => {
  const mk = await fetch("/v1/tasks", { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ actor: "human:verify", kind: "plain", description: "drawer check task" }) }).then((r) => r.json());
  const id = mk.id ?? mk.task?.id;
  await fetch(`/v1/tasks/${id}`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ actor: "human:verify", status: "deleted" }) });
  // the drawer lists journal tombstones for PURGED rows (soft-delete keeps
  // the row and stays out of it) — purge to land in "recently deleted"
  await fetch(`/v1/tasks/${id}?actor=${encodeURIComponent("human:verify")}`, { method: "DELETE" });
  return id;
});
await page.goto(BASE + "/#/desk", { waitUntil: "networkidle" });
// leaving #/welcome mid-setup raises the one-time "Leave setup?" modal —
// dismiss it or it intercepts every click below
await page.click("#ob-leave", { timeout: 5000 }).catch(() => {});
await page.waitForSelector("#task-deleted", { timeout: 15000 });
await page.click("#task-deleted summary");
await page.waitForSelector(`[data-del="${taskId}"] .del-restore`, { timeout: 10000 });
check("deleted drawer shows the task + restore", await page.locator(`[data-del="${taskId}"] .del-restore`).count() > 0);
await page.click(`[data-del="${taskId}"] .del-restore`);
// desk soft-delete IS archive (update_task aliases deleted→archived), so
// "delete forever" tombstones restore into the archive drawer
await page.waitForSelector("text=restored from the journal", { timeout: 10000 });
await page.click("#task-archive summary");
await page.waitForSelector("#task-archive-list .task-desc:has-text('drawer check task')", { timeout: 10000 });
check("restore brings the task back (archive)", await page.locator("#task-archive-list .task-desc:has-text('drawer check task')").count() > 0);
await page.screenshot({ path: "/tmp/hl-ui-verify/desk-deleted-drawer.png", fullPage: true });

// 12 — note editor "history" mode lists journaled versions
const noteId = await page.evaluate(async () => {
  const mk = await fetch("/v1/notes", { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ actor: "human:verify", title: "history check", body: "first draft" }) }).then((r) => r.json());
  const id = mk.id ?? mk.note?.id;
  const cur = await fetch(`/v1/notes/${id}`).then((r) => r.json());
  await fetch(`/v1/notes/${id}`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ actor: "human:verify", body: "second draft", version: cur.version ?? cur.note?.version ?? 1 }) });
  return id;
});
await page.goto(BASE + "/#/notes", { waitUntil: "networkidle" });
await page.click(`.note-row[data-note="${noteId}"]`);
await page.waitForSelector(".ne-mode[data-mode=history]", { timeout: 10000 });
await page.click(".ne-mode[data-mode=history]");
await page.waitForSelector("#ne-history .ne-ver", { timeout: 10000 });
const versions = await page.locator("#ne-history .ne-ver").count();
check(`note history lists versions (${versions} ≥ 2)`, versions >= 2);
await page.screenshot({ path: "/tmp/hl-ui-verify/note-history.png", fullPage: true });

console.log(errors.length ? `PAGE ERRORS:\n${errors.join("\n")}` : "no page errors");
await browser.close();
