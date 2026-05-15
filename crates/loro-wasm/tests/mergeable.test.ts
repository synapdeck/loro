import { describe, expect, test } from "vitest";
import { LoroDoc } from "../bundler/index";

function sync(a: LoroDoc, b: LoroDoc) {
  const aBytes = a.export({ mode: "update", from: b.version() });
  const bBytes = b.export({ mode: "update", from: a.version() });
  a.import(bBytes);
  b.import(aBytes);
}

describe("mergeable containers (WASM bindings)", () => {
  test("concurrent counter increments converge", () => {
    const a = new LoroDoc();
    const b = new LoroDoc();
    a.setPeerId("1");
    b.setPeerId("2");

    a.getMap("state").getMergeableCounter("revision").increment(1);
    b.getMap("state").getMergeableCounter("revision").increment(1);
    a.commit();
    b.commit();
    sync(a, b);

    expect(a.toJSON()).toEqual({ state: { revision: 2 } });
    expect(b.toJSON()).toEqual({ state: { revision: 2 } });
  });

  test("delete on mergeable key detaches and preserves state", () => {
    const doc = new LoroDoc();
    doc.setPeerId("1");
    const root = doc.getMap("state");
    const counter = root.getMergeableCounter("revision");
    counter.increment(3);
    doc.commit();
    expect(doc.toJSON()).toEqual({ state: { revision: 3 } });

    root.delete("revision");
    doc.commit();
    expect(doc.toJSON()).toEqual({ state: {} });

    // Re-get returns a working handle to preserved state; the cid does NOT auto-resurrect.
    const counter2 = root.getMergeableCounter("revision");
    expect(doc.toJSON()).toEqual({ state: {} });

    // A post-tombstone op resurrects the cid.
    counter2.increment(10);
    doc.commit();
    expect(doc.toJSON()).toEqual({ state: { revision: 13 } });
  });

  test("getMergeableMap, getMergeableList, getMergeableText smoke", () => {
    const doc = new LoroDoc();
    doc.setPeerId("1");
    const root = doc.getMap("state");

    root.getMergeableMap("nested").set("k", "v");
    root.getMergeableList("items").insert(0, 1);
    root.getMergeableText("body").insert(0, "hello");
    doc.commit();

    expect(doc.toJSON()).toEqual({
      state: {
        nested: { k: "v" },
        items: [1],
        body: "hello",
      },
    });
  });
});
