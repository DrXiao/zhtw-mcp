// Pure helpers shared by the service worker and the popup, both of which are
// ES modules.  They live apart from background.js so a test can import them
// without pulling in scanner.js, which imports the wasm glue and therefore
// only resolves after `sh extension/build-wasm.sh` has run.
//
// The content script cannot reach this file: chrome.scripting.executeScript
// injects classic scripts, so its shared code lives in shared.js instead.

/// Beyond three characters Chrome truncates the badge, so cap it there.
export function formatBadgeText(count) {
  if (!count) {
    return "";
  }
  return count > 99 ? "99+" : String(count);
}

/// chrome:// and about: pages reject injection, and a clear message beats the
/// permission error Chrome would otherwise raise.
export function isInspectableUrl(url = "") {
  return /^(https?|file):/i.test(url);
}

export function storageKey(tabId) {
  return `tab:${tabId}:scan`;
}

/// Session storage has a per-extension quota, and a dense page can produce
/// thousands of issues, so only a bounded prefix is stored.  The counts below
/// record what was dropped.  Nothing renders them today: the popup lists at
/// most 30 issues and takes its headline count from `badge_count`, which is
/// computed over the whole page before truncation.
export const MAX_STORED_ISSUES = 200;

export function storageResult(result) {
  const issues = Array.isArray(result.issues) ? result.issues : [];
  return {
    ...result,
    issues: issues.slice(0, MAX_STORED_ISSUES),
    stored_issue_count: Math.min(issues.length, MAX_STORED_ISSUES),
    total_issue_count: issues.length,
    storage_truncated: issues.length > MAX_STORED_ISSUES,
  };
}

export function formatBreakdown(result) {
  const counts = result.severity_counts || {};
  return `錯誤 ${counts.error || 0}，警告 ${counts.warning || 0}，資訊 ${counts.info || 0}`;
}
