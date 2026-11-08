import assert from "node:assert/strict";
import test from "node:test";
import { copyText } from "../src/lib/clipboard.ts";

function fallback(succeeds = true) {
  let selected = false;
  let removed = false;
  let attached = false;
  let restored = false;
  const range = {};
  const field = {
    value: "",
    readOnly: false,
    style: {},
    select: () => {
      selected = true;
    },
    remove: () => {
      removed = true;
    },
  };
  const doc = {
    activeElement: null,
    getSelection: () => ({
      rangeCount: 1,
      getRangeAt: () => range,
      removeAllRanges: () => {},
      addRange: (value: unknown) => {
        restored = value === range;
      },
    }),
    createElement: () => field,
    body: {
      appendChild: () => {
        attached = true;
      },
    },
    execCommand: (command: string) => {
      assert.equal(command, "copy");
      assert.equal(selected, true);
      return succeeds;
    },
  } as unknown as Document;
  return { doc, field, state: () => ({ selected, removed, attached, restored }) };
}

// Browser-only constructor used when restoring focus after the fallback.
Object.defineProperty(globalThis, "HTMLElement", { value: class {}, configurable: true });

test("uses Clipboard API without creating a fallback field", async () => {
  const calls: string[] = [];
  await copyText(
    "https://mmx.example/callback",
    {
      writeText: async (text) => {
        calls.push(text);
      },
    },
    {} as Document,
  );
  assert.deepEqual(calls, ["https://mmx.example/callback"]);
});

test("HTTP origins without Clipboard API copy the complete URL and remove the field", async () => {
  const mock = fallback();
  // Explicitly model a browser without the secure-context Clipboard API.
  await copyText("http://mmx.example/admin/v1/oidc/callback", null, mock.doc);
  assert.equal(mock.field.value, "http://mmx.example/admin/v1/oidc/callback");
  assert.deepEqual(mock.state(), { selected: true, removed: true, attached: true, restored: true });
});

test("permission rejection falls back instead of leaving an unhandled promise", async () => {
  const mock = fallback();
  await copyText(
    "https://mmx.example/callback",
    {
      writeText: async () => {
        throw new Error("Denied");
      },
    },
    mock.doc,
  );
  assert.equal(mock.state().removed, true);
});

test("failed fallback reports failure and cleans up selection", async () => {
  const mock = fallback(false);
  await assert.rejects(
    copyText("https://mmx.example/callback", null, mock.doc),
    /Clipboard access was denied/,
  );
  assert.equal(mock.state().removed, true);
  assert.equal(mock.state().restored, true);
});
