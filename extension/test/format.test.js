import test from "node:test";
import assert from "node:assert/strict";

import {
  MAX_STORED_ISSUES,
  formatBadgeText,
  formatBreakdown,
  isInspectableUrl,
  storageKey,
  storageResult,
} from "../src/format.js";

test("badge text is capped for dense pages", () => {
  assert.equal(formatBadgeText(0), "");
  assert.equal(formatBadgeText(7), "7");
  assert.equal(formatBadgeText(99), "99");
  assert.equal(formatBadgeText(100), "99+");
  assert.equal(formatBadgeText(125), "99+");
});

test("only injectable schemes are offered a scan", () => {
  for (const url of [
    "https://example.tw/a",
    "http://example.tw/a",
    "HTTPS://EXAMPLE.TW",
    "file:///tmp/a.html",
  ]) {
    assert.equal(isInspectableUrl(url), true, url);
  }
  for (const url of [
    "chrome://extensions",
    "about:blank",
    "chrome-extension://abc/popup.html",
    "devtools://devtools/bundled/inspector.html",
    "",
  ]) {
    assert.equal(isInspectableUrl(url), false, url);
  }
  assert.equal(isInspectableUrl(), false);
});

test("storage keys are per tab", () => {
  assert.equal(storageKey(7), "tab:7:scan");
  assert.notEqual(storageKey(7), storageKey(8));
});

test("stored results are truncated and say so", () => {
  const issues = Array.from({ length: MAX_STORED_ISSUES + 5 }, (_, i) => ({
    offset: i,
  }));
  const stored = storageResult({ issues, page_title: "t" });

  assert.equal(stored.issues.length, MAX_STORED_ISSUES);
  assert.equal(stored.stored_issue_count, MAX_STORED_ISSUES);
  assert.equal(stored.total_issue_count, MAX_STORED_ISSUES + 5);
  assert.equal(stored.storage_truncated, true);
  // Truncation must not drop the rest of the payload.
  assert.equal(stored.page_title, "t");
});

test("a result that fits is not marked truncated", () => {
  const stored = storageResult({ issues: [{ offset: 0 }] });

  assert.equal(stored.issues.length, 1);
  assert.equal(stored.total_issue_count, 1);
  assert.equal(stored.storage_truncated, false);
});

test("a result with no issues array is still storable", () => {
  const stored = storageResult({});

  assert.deepEqual(stored.issues, []);
  assert.equal(stored.total_issue_count, 0);
  assert.equal(stored.storage_truncated, false);
});

test("breakdown defaults every severity to zero", () => {
  assert.equal(formatBreakdown({}), "錯誤 0，警告 0，資訊 0");
  assert.equal(
    formatBreakdown({ severity_counts: { error: 2, warning: 3, info: 4 } }),
    "錯誤 2，警告 3，資訊 4",
  );
});
