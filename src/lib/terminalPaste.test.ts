import { describe, expect, it, vi } from "vitest";
import {
  deliverPaste,
  fallbackPasteRefusedMessage,
  localPasteRefusal,
  PASTE_BLOCK_LIMIT,
  PASTE_PAYLOAD_LIMIT,
  parsePasteRefusal,
  pasteRefusedMessage,
  preparePasteText,
  utf8Length,
} from "./terminalPaste";

describe("preparePasteText", () => {
  it("turns CRLF and LF into CR", () => {
    expect(preparePasteText("a\r\nb\nc")).toBe("a\rb\rc");
  });

  it("leaves a lone CR alone", () => {
    expect(preparePasteText("a\rb")).toBe("a\rb");
  });

  it("keeps empty text empty", () => {
    expect(preparePasteText("")).toBe("");
  });

  it("normalises a trailing newline and blank lines", () => {
    expect(preparePasteText("x\n")).toBe("x\r");
    expect(preparePasteText("\r\n\r\n")).toBe("\r\r");
  });

  it("adds no bracketed-paste markers", () => {
    expect(preparePasteText("a\nb")).not.toContain("\x1b[200~");
  });
});

describe("parsePasteRefusal", () => {
  it("parses the rejection string", () => {
    expect(parsePasteRefusal("PasteTooLarge: 1048577 > 1048576")).toEqual({
      bytes: 1048577,
      limit: 1048576,
    });
  });

  it("parses an Error", () => {
    expect(parsePasteRefusal(new Error("PasteTooLarge: 30000 > 24576"))).toEqual({
      bytes: 30000,
      limit: 24576,
    });
  });

  it("returns null for any other error", () => {
    expect(parsePasteRefusal("herdr is recovering")).toBeNull();
    expect(parsePasteRefusal(new Error("no live session"))).toBeNull();
    expect(parsePasteRefusal(undefined)).toBeNull();
  });
});

describe("pasteRefusedMessage", () => {
  it("names the size and the bridge limit", () => {
    const msg = pasteRefusedMessage({ bytes: 2.4 * 1024 * 1024, limit: 1_048_576 });
    expect(msg).toBe("Paste refused: 2.4 MB is over the 1.0 MB limit.");
  });

  it("words the fallback refusal as a reconnecting estimate", () => {
    const msg = fallbackPasteRefusedMessage({ bytes: 30 * 1024, limit: 24576 });
    expect(msg).toBe("Paste too large to send while reconnecting (~30 KB, limit 24 KB).");
  });

  it("has no em dashes", () => {
    expect(pasteRefusedMessage({ bytes: 5, limit: 1 })).not.toContain("—");
    expect(fallbackPasteRefusedMessage({ bytes: 5, limit: 1 })).not.toContain("—");
  });
});

describe("utf8Length", () => {
  it("matches TextEncoder", () => {
    for (const t of [
      "",
      "abc",
      "é",
      "€",
      "😀",
      "a\u00e9\u20ac\u{1F600}z",
      "\ud800",
      "\udc00x",
      "\ud800\ud800",
    ]) {
      expect(utf8Length(t), JSON.stringify(t)).toBe(new TextEncoder().encode(t).length);
    }
  });
});

describe("localPasteRefusal", () => {
  it("passes a payload of exactly the limit", () => {
    expect(PASTE_PAYLOAD_LIMIT).toBe(PASTE_BLOCK_LIMIT - 12);
    expect(localPasteRefusal("a".repeat(PASTE_PAYLOAD_LIMIT))).toBeNull();
  });

  it("refuses one byte over, measured as the backend does (markers included)", () => {
    expect(localPasteRefusal("a".repeat(PASTE_PAYLOAD_LIMIT + 1))).toEqual({
      bytes: PASTE_BLOCK_LIMIT + 1,
      limit: PASTE_BLOCK_LIMIT,
    });
  });

  it("counts UTF-8 bytes, not UTF-16 units", () => {
    // Under the limit in units, over it in bytes.
    const euros = "€".repeat(Math.floor(PASTE_PAYLOAD_LIMIT / 3) + 1);
    expect(euros.length).toBeLessThan(PASTE_PAYLOAD_LIMIT);
    expect(localPasteRefusal(euros)?.bytes).toBe(3 * euros.length + 12);
  });

  it("leaves text with an ESC or NUL to the backend, which strips before measuring", () => {
    const over = "a".repeat(PASTE_PAYLOAD_LIMIT + 1);
    expect(localPasteRefusal(`${over}\x1b`)).toBeNull();
    expect(localPasteRefusal(`${over}\0`)).toBeNull();
  });
});

describe("deliverPaste", () => {
  const hooks = () => ({ accepted: vi.fn(), refused: vi.fn() });

  it("reports acceptance only once the backend took the paste", async () => {
    const h = hooks();
    let resolve: () => void = () => {};
    const send = vi.fn(() => new Promise<void>((r) => (resolve = r)));
    const done = deliverPaste("p", send, h);
    expect(send).toHaveBeenCalledWith("p");
    await Promise.resolve();
    expect(h.accepted).not.toHaveBeenCalled();
    resolve();
    await done;
    expect(h.accepted).toHaveBeenCalledOnce();
    expect(h.refused).not.toHaveBeenCalled();
  });

  it("toasts a backend size refusal without reporting acceptance", async () => {
    const h = hooks();
    await deliverPaste("p", () => Promise.reject("PasteTooLarge: 2000000 > 1048576"), h);
    expect(h.accepted).not.toHaveBeenCalled();
    expect(h.refused).toHaveBeenCalledWith(pasteRefusedMessage({ bytes: 2000000, limit: 1048576 }));
  });

  it("stays silent on other errors", async () => {
    const h = hooks();
    await deliverPaste("p", () => Promise.reject(new Error("NoSuchSession")), h);
    expect(h.accepted).not.toHaveBeenCalled();
    expect(h.refused).not.toHaveBeenCalled();
  });

  it("refuses an oversized paste locally without sending it", async () => {
    const h = hooks();
    const send = vi.fn(() => Promise.resolve());
    await deliverPaste("a".repeat(PASTE_PAYLOAD_LIMIT + 1), send, h);
    expect(send).not.toHaveBeenCalled();
    expect(h.accepted).not.toHaveBeenCalled();
    expect(h.refused).toHaveBeenCalledWith(
      pasteRefusedMessage({ bytes: PASTE_BLOCK_LIMIT + 1, limit: PASTE_BLOCK_LIMIT }),
    );
  });
});
