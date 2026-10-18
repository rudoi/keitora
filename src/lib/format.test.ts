import { describe, expect, it } from "vitest";

import {
  formatAgo,
  formatBytes,
  formatCpu,
  formatRate,
  formatUptime,
  parseUtc,
  shortPath,
  tailPath,
} from "./format";

describe("formatBytes", () => {
  it("shows exact bytes under 1 KB", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1023)).toBe("1023 B");
  });

  it("rounds into KB/MB/GB with a space before the unit", () => {
    expect(formatBytes(1024)).toBe("1.0 KB");
    expect(formatBytes(88 * 1024 * 1024)).toBe("88 MB");
    expect(formatBytes(1.5 * 1024 * 1024 * 1024)).toBe("1.5 GB");
  });

  it("drops the decimal at or above 10 in a unit (stable width)", () => {
    expect(formatBytes(9.4 * 1024 * 1024)).toBe("9.4 MB");
    expect(formatBytes(10.6 * 1024 * 1024)).toBe("11 MB");
  });

  it("rounds 9.95 up to a no-decimal '10' (width invariant at the boundary)", () => {
    expect(formatBytes(9.95 * 1024 * 1024)).toBe("10 MB");
  });

  it("returns an em dash for invalid input", () => {
    expect(formatBytes(-1)).toBe("—");
    expect(formatBytes(Number.NaN)).toBe("—");
  });
});

describe("formatCpu", () => {
  it("always shows one decimal", () => {
    expect(formatCpu(0)).toBe("0.0%");
    expect(formatCpu(1)).toBe("1.0%");
    expect(formatCpu(12.34)).toBe("12.3%");
  });

  it("returns an em dash for invalid input", () => {
    expect(formatCpu(-1)).toBe("—");
    expect(formatCpu(Number.NaN)).toBe("—");
  });
});

describe("formatRate", () => {
  it("appends /s to the byte format", () => {
    expect(formatRate(0)).toBe("0 B/s");
    expect(formatRate(1536)).toBe("1.5 KB/s");
    expect(formatRate(5 * 1024 * 1024)).toBe("5.0 MB/s");
  });

  it("returns an em dash for invalid input", () => {
    expect(formatRate(-1)).toBe("—");
    expect(formatRate(Number.NaN)).toBe("—");
  });
});

describe("formatUptime", () => {
  it("formats seconds, minutes, hours, and days", () => {
    expect(formatUptime(45 * 1000)).toBe("45s");
    expect(formatUptime(3 * 60 * 1000 + 4 * 1000)).toBe("3m 4s");
    expect(formatUptime((3 * 60 + 12) * 60 * 1000)).toBe("3h 12m");
    expect(formatUptime(((2 * 24 + 5) * 60 + 7) * 60 * 1000)).toBe("2d 5h 7m");
  });
});

describe("formatAgo", () => {
  const now = Date.parse("2026-10-16T12:00:00Z");

  it("reads SQLite's zoneless timestamps as UTC", () => {
    expect(formatAgo("2026-10-16 11:55:00", now)).toBe("5m");
    expect(formatAgo("2026-10-16 09:00:00", now)).toBe("3h");
    expect(formatAgo("2026-10-14 11:00:00", now)).toBe("2d");
  });

  it("honours an explicit zone", () => {
    expect(formatAgo("2026-10-16T11:30:00Z", now)).toBe("30m");
    expect(formatAgo("2026-10-16T13:30:00+02:00", now)).toBe("30m");
  });

  it("says now under a minute or for a time ahead of the clock", () => {
    expect(formatAgo("2026-10-16 11:59:30", now)).toBe("now");
    expect(formatAgo("2026-10-16 12:05:00", now)).toBe("now");
  });

  it("is null for an empty or unparseable time", () => {
    expect(formatAgo("", now)).toBeNull();
    expect(formatAgo(null, now)).toBeNull();
    expect(formatAgo("yesterday", now)).toBeNull();
  });
});

describe("parseUtc", () => {
  it("reads zoneless SQLite times as UTC and zoned ISO as given", () => {
    expect(parseUtc("2026-10-16 12:00:00")).toBe(Date.parse("2026-10-16T12:00:00Z"));
    expect(parseUtc("2026-10-16T14:00:00+02:00")).toBe(Date.parse("2026-10-16T12:00:00Z"));
    expect(parseUtc("")).toBeNull();
    expect(parseUtc("nope")).toBeNull();
  });
});

describe("shortPath", () => {
  it("shows a leading home dir as ~ with forward slashes", () => {
    expect(shortPath("C:\\Users\\me\\repos\\lodi", "C:\\Users\\me")).toBe("~/repos/lodi");
    expect(shortPath("/Users/me/repos/lodi", "/Users/me/")).toBe("~/repos/lodi");
    expect(shortPath("/Users/me", "/Users/me")).toBe("~");
  });

  it("ignores case for Windows paths only", () => {
    expect(shortPath("c:/users/ME/x", "C:\\Users\\me")).toBe("~/x");
    expect(shortPath("/home/Bob/proj", "/home/bob")).toBe("/home/Bob/proj");
  });

  it("only matches home at a path boundary", () => {
    expect(shortPath("/Users/meg/x", "/Users/me")).toBe("/Users/meg/x");
  });

  it("leaves other paths alone apart from slashes", () => {
    expect(shortPath("D:\\repos\\lodi", "C:\\Users\\me")).toBe("D:/repos/lodi");
    expect(shortPath("D:\\repos", null)).toBe("D:/repos");
    expect(shortPath("/x", "")).toBe("/x");
  });

  it("drops a Windows verbatim prefix before matching home", () => {
    expect(shortPath("\\\\?\\C:\\Users\\me\\proj", "C:\\Users\\me")).toBe("~/proj");
    expect(shortPath("\\\\?\\D:\\repos\\lodi", null)).toBe("D:/repos/lodi");
    expect(shortPath("\\\\?\\UNC\\srv\\share\\a", null)).toBe("//srv/share/a");
  });
});

describe("tailPath", () => {
  it("keeps a root plus up to two folders whole", () => {
    expect(tailPath("D:/scratch")).toBe("D:/scratch");
    expect(tailPath("D:/repos/lodi")).toBe("D:/repos/lodi");
    expect(tailPath("~/oss/some-tool")).toBe("~/oss/some-tool");
    expect(tailPath("/a/b")).toBe("/a/b");
    expect(tailPath("//srv/share/a/b")).toBe("//srv/share/a/b");
  });

  it("cuts deeper paths to their last two folders", () => {
    expect(tailPath("C:/Users/x/src/repos/shared-tools")).toBe("repos/shared-tools");
    expect(tailPath("D:/repos/lodi/x")).toBe("lodi/x");
    expect(tailPath("~/a/b/c")).toBe("b/c");
    expect(tailPath("/a/b/c")).toBe("b/c");
    expect(tailPath("//srv/share/a/b/c")).toBe("b/c");
  });

  it("drops trailing slashes, except a bare drive's", () => {
    expect(tailPath("D:/repos/")).toBe("D:/repos");
    expect(tailPath("D:/repos/lodi/x/")).toBe("lodi/x");
    expect(tailPath("D:/")).toBe("D:/");
  });

  it("handles bare roots and empty input", () => {
    expect(tailPath("~")).toBe("~");
    expect(tailPath("/")).toBe("/");
    expect(tailPath("D:")).toBe("D:");
    expect(tailPath("")).toBe("");
  });

  it("keeps at least one folder for a zero or negative levels", () => {
    expect(tailPath("D:/a/b/c", 0)).toBe("c");
    expect(tailPath("D:/a/b/c", -1)).toBe("c");
  });

  it("takes a levels override", () => {
    expect(tailPath("D:/repos/lodi", 1)).toBe("lodi");
    expect(tailPath("C:/Users/x/src/repos/shared-tools", 3)).toBe("src/repos/shared-tools");
  });
});
