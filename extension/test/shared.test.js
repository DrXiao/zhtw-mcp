import test from "node:test";
import assert from "node:assert/strict";

// shared.js is injected as a classic script and has no exports; it publishes
// onto the global, which is how content.js reads it too.  Importing it for
// side effects therefore gives the test the same object the browser sees.
import "../src/shared.js";

const {
  byteOffsetToCodeUnit,
  issueSegments,
  normalizeIssue,
  tooltipForIssue,
  utf8ByteLength,
} = globalThis.ZhtwExtensionShared;

/// Build spans the way content.js does: concatenated text, byte offsets.
function spansOf(...texts) {
  let byteStart = 0;
  return texts.map((text) => {
    const byteEnd = byteStart + utf8ByteLength(text);
    const span = { byteStart, byteEnd, text };
    byteStart = byteEnd;
    return span;
  });
}

test("UTF-8 byte offsets map back to JavaScript code units", () => {
  const text = "A軟件B";
  const start = utf8ByteLength("A");
  const end = start + utf8ByteLength("軟件");

  assert.equal(byteOffsetToCodeUnit(text, start), 1);
  assert.equal(byteOffsetToCodeUnit(text, end), 3);
});

test("byte offsets outside the text clamp to its ends", () => {
  const text = "軟件";
  assert.equal(byteOffsetToCodeUnit(text, 0), 0);
  assert.equal(byteOffsetToCodeUnit(text, -5), 0);
  assert.equal(byteOffsetToCodeUnit(text, 9999), text.length);
});

test("an offset landing mid-character does not split it", () => {
  // 軟 is three UTF-8 bytes; byte 1 and byte 2 are inside it.
  assert.equal(byteOffsetToCodeUnit("軟件", 1), 0);
  assert.equal(byteOffsetToCodeUnit("軟件", 2), 0);
  assert.equal(byteOffsetToCodeUnit("軟件", 3), 1);
});

test("astral characters cost two code units", () => {
  const text = "𠮷字";
  assert.equal(utf8ByteLength("𠮷"), 4);
  assert.equal(byteOffsetToCodeUnit(text, 4), 2);
});

test("an issue inside one span becomes one segment", () => {
  const spans = spansOf("請安裝軟件後重試");
  const offset = utf8ByteLength("請安裝");
  const segments = issueSegments(spans, {
    offset,
    length: utf8ByteLength("軟件"),
  });

  assert.deepEqual(segments, [{ index: 0, start: 3, end: 5 }]);
  assert.equal(spans[0].text.slice(3, 5), "軟件");
});

test("an issue straddling spans becomes one segment per span", () => {
  const spans = spansOf("請安裝軟", "件後重試");
  const segments = issueSegments(spans, {
    offset: utf8ByteLength("請安裝"),
    length: utf8ByteLength("軟件"),
  });

  assert.deepEqual(segments, [
    { index: 0, start: 3, end: 4 },
    { index: 1, start: 0, end: 1 },
  ]);
  assert.equal(
    spans[0].text.slice(3, 4) + spans[1].text.slice(0, 1),
    "軟件",
  );
});

test("a zero-length issue produces no segments", () => {
  assert.deepEqual(issueSegments(spansOf("軟件"), { offset: 0, length: 0 }), []);
});

test("an issue past the end of the text produces no segments", () => {
  const spans = spansOf("軟件");
  assert.deepEqual(issueSegments(spans, { offset: 99, length: 3 }), []);
  // Start inside, end past the last span: nothing to anchor the range to.
  assert.deepEqual(issueSegments(spans, { offset: 0, length: 99 }), []);
});

test("empty spans produce no segments", () => {
  assert.deepEqual(issueSegments([], { offset: 0, length: 3 }), []);
});

test("normalizeIssue fills in every field a highlight reads", () => {
  const issue = normalizeIssue({});

  assert.deepEqual(issue, {
    offset: 0,
    length: 0,
    found: "",
    suggestions: [],
    rule_type: "unknown",
    severity: "info",
    context: "",
    english: "",
  });
});

test("normalizeIssue coerces junk rather than propagating it", () => {
  const issue = normalizeIssue({
    offset: "12",
    length: null,
    suggestions: "not an array",
    severity: "error",
  });

  assert.equal(issue.offset, 12);
  assert.equal(issue.length, 0);
  assert.deepEqual(issue.suggestions, []);
  assert.equal(issue.severity, "error");
});

test("a tooltip omits the sections an issue has no data for", () => {
  const bare = tooltipForIssue(normalizeIssue({ found: "軟件", severity: "warning" }));

  assert.match(bare, /^軟件 — unknown \/ warning\n無自動建議$/);
  assert.doesNotMatch(bare, /說明/);
  assert.doesNotMatch(bare, /English/);
});

test("a tooltip lists every suggestion", () => {
  const full = tooltipForIssue(
    normalizeIssue({
      found: "軟件",
      suggestions: ["軟體", "程式"],
      rule_type: "cross_strait",
      severity: "error",
      context: "台灣用語",
      english: "software",
    }),
  );

  assert.equal(
    full,
    "軟件 — cross_strait / error\n建議：軟體、程式\n說明：台灣用語\nEnglish：software",
  );
});

test("segments read span text through a getter, so live nodes stay current", () => {
  // content.js backs each span with a getter onto the DOM node, because
  // surroundContents splits text nodes as highlights land.
  const node = { nodeValue: "請安裝軟件後重試" };
  const spans = [
    {
      node,
      byteStart: 0,
      byteEnd: utf8ByteLength(node.nodeValue),
      get text() {
        return this.node.nodeValue || "";
      },
    },
  ];
  const issue = {
    offset: utf8ByteLength("請安裝"),
    length: utf8ByteLength("軟件"),
  };

  assert.deepEqual(issueSegments(spans, issue), [{ index: 0, start: 3, end: 5 }]);

  // A node emptied by an earlier highlight is dropped rather than turned into
  // an empty range the DOM would reject.
  node.nodeValue = "";
  assert.deepEqual(issueSegments(spans, issue), []);
});
