// e2e smoke: does vanish actually BOOT on its preview deployment?
//
// this is the behavioral half of the gate ci.yml cannot provide: ci proves
// the code compiles and passes suites; this proves the deployed artifact
// initializes its wasm, its ui thread boots, its agent worker comes up, and
// the dock renders interactive. grounded in the real boot sequence:
// web/index.html loads ./pkg/vanish.js, calls boot_ui(), which mounts the
// dom (#status reads "booting…"), spawns the module worker, and the
// worker's Event::Ready flips the status away from "booting…" once the
// message channel is live. a blank page, a wasm validation failure, or a
// worker that never announces itself all fail here.
//
// failure modes are reported DISTINCTLY (D4): an auth interstitial from
// vercel deployment protection is a different problem than a broken app,
// and the fix is different (relax protection for previews vs. repair the
// commit). the output names which one happened.

import { chromium } from "playwright";
import { mkdirSync } from "node:fs";

const BASE = process.env.PREVIEW_URL;
if (!BASE) {
  console.error("PREVIEW_URL not set — the workflow must resolve the preview first.");
  process.exit(2);
}

const PROTECTION_MARKERS = [
  "deployment protection",
  "vercel security checkpoint",
  "confirm you are human",
];

mkdirSync("evidence", { recursive: true });

const browser = await chromium.launch();
const page = await browser.newPage();

const pageErrors = [];
page.on("pageerror", (err) => pageErrors.push(String(err)));
page.on("console", (msg) => {
  if (msg.type() === "error") pageErrors.push(`[console.error] ${msg.text()}`);
});

let verdict;
try {
  await page.goto(BASE, { waitUntil: "domcontentloaded", timeout: 30_000 });

  // protection interstitial check FIRST, so the diagnosis is honest.
  const bodyText = (await page.textContent("body")) ?? "";
  const blocked = PROTECTION_MARKERS.find((m) => bodyText.toLowerCase().includes(m));
  if (blocked) {
    console.error(
      `EVIDENT-CAUSE FAILURE: the preview is behind vercel deployment protection ` +
        `(matched "${blocked}"). nothing can test against this — relax protection ` +
        `for preview deployments or supply a bypass token (STACKED_PRS_PLAN §3 P1).`
    );
    await page.screenshot({ path: "evidence/protection-interstitial.png" }).catch(() => {});
    process.exit(3);
  }

  // the app shell must exist at all.
  for (const sel of ["#feed", "#prompt", "#run", "#status"]) {
    try {
      await page.waitForSelector(sel, { timeout: 15_000 });
    } catch {
      console.error(`FAIL: ${sel} never appeared — the app html did not mount.`);
      verdict = `missing selector ${sel}`;
      break;
    }
  }

  if (!verdict) {
    // the real assertion: the worker announces itself. #status starts as
    // literally "booting…" in index.html and only changes once the worker
    // is listening and events flow (Event::Ready / ConfigStatus path).
    try {
      await page.waitForFunction(
        () => {
          const el = document.querySelector("#status");
          return el && el.textContent && el.textContent.trim() !== "" &&
                 el.textContent.trim().toLowerCase() !== "booting…";
        },
        { timeout: 90_000 }
      );
      const status = ((await page.textContent("#status")) ?? "").trim();
      console.log(`PASS: app booted — #status = "${status}"`);
    } catch {
      console.error(
        "FAIL: #status stayed 'booting…' for 90s — the worker never announced " +
          "itself (wasm failed to compile/validate, worker.js died, or the " +
          "module fetch 404'd)."
      );
      verdict = "worker never announced ready";
    }
  }

  if (!verdict && pageErrors.length > 0) {
    console.error(`FAIL: ${pageErrors.length} page error(s) during boot:`);
    for (const e of pageErrors.slice(0, 10)) console.error(`  - ${e}`);
    verdict = `${pageErrors.length} page errors`;
  }

  if (!verdict) {
    await page.screenshot({ path: "evidence/booted.png" });
    console.log(`evidence written: evidence/booted.png`);
    console.log(`PASS: ${BASE} boots clean.`);
  }
} catch (err) {
  verdict = String(err);
} finally {
  if (verdict) {
    await page.screenshot({ path: "evidence/failure.png" }).catch(() => {});
    await page
      .content()
      .then((html) => import("node:fs").then((fs) => fs.writeFileSync("evidence/failure.html", html)))
      .catch(() => {});
    console.error(`E2E SMOKE FAILED: ${verdict} (evidence in evidence/)`);
    process.exitCode = 1;
  }
  await browser.close();
}
