import type { ILink, ILinkProvider, Terminal } from "@xterm/xterm";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  activateTerminalLink,
  bufferRowReader,
  classifyTerminalLink,
  createTerminalLinkProvider,
  FILE_UNAVAILABLE_ERROR,
  findFileReferences,
  findLineLinks,
  installTerminalLinks,
  joinsNext,
  logicalLineAt,
} from "./terminalLinks";

vi.hoisted(() => {
  Object.defineProperty(globalThis, "self", { value: globalThis, configurable: true });
});

afterEach(() => vi.unstubAllGlobals());

describe("terminal destination classification", () => {
  it.each([
    ["D:/repo/file.ts:42:3", "D:/repo/file.ts", 42, 3],
    ["/D:/repo/file.ts:42", "D:/repo/file.ts", 42, undefined],
    ["D:\\repo\\file.ts:2", "D:\\repo\\file.ts", 2, undefined],
    ["src/file.ts:9", "src/file.ts", 9, undefined],
    ["file.ts:7", "file.ts", 7, undefined],
    ["/repo/file.ts", "/repo/file.ts", undefined, undefined],
    ["file:///D:/My%20Project/a%2520b.ts#L42C3", "D:/My Project/a%20b.ts", 42, 3],
    ["file://localhost/repo/file.ts:5", "/repo/file.ts", 5, undefined],
    ["src/a%20b.ts", "src/a%20b.ts", undefined, undefined],
    ["README.md", "README.md", undefined, undefined],
  ])("parses %s", (input, path, line, column) => {
    expect(classifyTerminalLink(input)).toEqual({
      kind: "file",
      path,
      ...(line ? { line } : {}),
      ...(column ? { column } : {}),
    });
  });

  it.each([
    "javascript:alert(1)",
    "data:text/html,test",
    "mailto:a@b.com",
    "https:example.com",
    "https://",
    "https://exa mple.com",
    "https://example.com\\evil",
    "file://server/share/a.ts",
    "file:///x/%ZZ.ts",
    "file:///x/a.ts?query",
    "file:///x/a.ts#unexpected",
    "file:////server/a.ts",
    "\\\\server\\a.ts",
    "\\\\?\\D:\\a.ts",
    "D:relative.ts",
    "D:/x/a.ts:stream",
    "src/a.ts:0",
    "src/a.ts\n",
    "file:///x/a%00.ts",
  ])("rejects %s", (input) => {
    expect(classifyTerminalLink(input)).toBeNull();
  });

  it("accepts only explicit web URLs", () => {
    expect(classifyTerminalLink("HTTPS://Example.com/a?q=1#part")).toEqual({
      kind: "web",
      url: "https://example.com/a?q=1#part",
    });
  });

  it("requires delimiter context for root-level filenames with spaces", () => {
    expect(classifyTerminalLink("hello world.txt")).toBeNull();
    expect(classifyTerminalLink("hello world.txt", true)).toEqual({
      kind: "file",
      path: "hello world.txt",
    });
    expect(classifyTerminalLink("ordinary prose", true)).toBeNull();
    expect(classifyTerminalLink("javascript:hello world.txt", true)).toBeNull();
  });
});

describe("printed file references", () => {
  it("recognizes delimited spaces and excludes web destinations", () => {
    const text =
      'See `src/My File.ts:4`, "D:/My Project/a.ts:5" and [file](</D:/My Project/b.ts:6>). https://example.com/src/no.ts src/yes.ts:7.';
    const matches = findFileReferences(text);
    expect(matches.map((match) => match.text)).toEqual([
      "src/My File.ts:4",
      "D:/My Project/a.ts:5",
      "/D:/My Project/b.ts:6",
      "src/yes.ts:7",
    ]);
    for (const match of matches)
      expect(text.slice(match.index, match.index + match.text.length)).toBe(match.text);
  });
  it("does not turn URL substrings into file links", () => {
    expect(findFileReferences("https://example.com/src/a.ts javascript:src/a.ts")).toEqual([]);
  });
  it("accepts filenames in Markdown and backticks", () => {
    expect(
      findFileReferences("[README](README.md) `README.md`").map((match) => match.text),
    ).toEqual(["README.md", "README.md"]);
  });
});

function actions() {
  return {
    context: () => ({ project: "project", worktree: "epsilon", active: true }),
    openUrl: vi.fn(async () => {}),
    openFile: vi.fn(async () => {}),
    showError: vi.fn(),
  };
}

describe("native activation", () => {
  it("uses current pane identity and strips locations", async () => {
    const deps = actions();
    deps.context = () => ({
      project: "new-project",
      worktree: "echo",
      active: true,
    });
    await activateTerminalLink("src/a.ts:42:3", deps);
    expect(deps.openFile).toHaveBeenCalledWith("new-project", "echo", "src/a.ts");
    expect(deps.openUrl).not.toHaveBeenCalled();
  });
  it("opens web links locally", async () => {
    const deps = actions();
    await activateTerminalLink("https://example.com", deps);
    expect(deps.openUrl).toHaveBeenCalledWith("https://example.com/");
  });
  it("reports unsupported links and native errors", async () => {
    const deps = actions();
    await activateTerminalLink("javascript:alert(1)", deps);
    expect(deps.openUrl).not.toHaveBeenCalled();
    expect(deps.openFile).not.toHaveBeenCalled();
    deps.openUrl.mockRejectedValueOnce(new Error("browser unavailable"));
    await activateTerminalLink("https://example.com", deps);
    expect(deps.showError).toHaveBeenLastCalledWith("Could not open link: browser unavailable");
    deps.openFile.mockRejectedValueOnce("Missing file");
    await activateTerminalLink("src/missing.ts", deps);
    expect(deps.showError).toHaveBeenLastCalledWith("Could not open link: Missing file");
  });
  it("tries fallbacks in order only while the file is unavailable", async () => {
    const deps = actions();
    const missing = `${FILE_UNAVAILABLE_ERROR}: not found`;
    deps.openFile.mockRejectedValueOnce(missing);
    deps.openFile.mockRejectedValueOnce("Executable files cannot be opened from terminal links");
    await activateTerminalLink("src/ab.ts", deps, false, [
      { text: "src/a.ts", delimited: false },
      { text: "b.ts", delimited: false },
    ]);
    expect(deps.openFile).toHaveBeenCalledTimes(2);
    expect(deps.openFile).toHaveBeenLastCalledWith("project", "epsilon", "src/a.ts");
    expect(deps.showError).toHaveBeenCalledWith(`Could not open link: ${missing}`);
  });
  it("ignores hidden panes", async () => {
    const deps = actions();
    deps.context = () => ({ project: "p", worktree: "w", active: false });
    await activateTerminalLink("src/a.ts", deps);
    expect(deps.openFile).not.toHaveBeenCalled();
  });
});

/** A terminal of `rows`; with `wrapped`, every row after the first is marked soft-wrapped. */
function terminalFixture(rows: string[], cols: number, wrapped = true) {
  const providers: ILinkProvider[] = [];
  const disposals: ReturnType<typeof vi.fn>[] = [];
  const disposable = () => {
    const dispose = vi.fn();
    disposals.push(dispose);
    return { dispose };
  };
  const lines = rows.map((row, index) => {
    const cells: { getChars(): string; getWidth(): number }[] = [];
    for (const segment of row.matchAll(/.[\u0300-\u036f]*/gu)) {
      const width = segment[0] === "界" ? 2 : 1;
      cells.push({ getChars: () => segment[0], getWidth: () => width });
      if (width === 2) cells.push({ getChars: () => "", getWidth: () => 0 });
    }
    while (cells.length < cols) cells.push({ getChars: () => "", getWidth: () => 1 });
    return {
      length: cols,
      isWrapped: wrapped && index > 0,
      getCell: (col: number, target?: object) => {
        if (target) Object.assign(target, cells[col]);
        return cells[col];
      },
      translateToString: (trim: boolean) => (trim ? row.trimEnd() : row.padEnd(cols)),
    };
  });
  const terminal = {
    cols,
    hasSelection: () => false,
    options: { linkHandler: undefined },
    buffer: { active: { getLine: (row: number) => lines[row], getNullCell: () => ({}) } },
    registerLinkProvider: (provider: ILinkProvider) => {
      providers.push(provider);
      return disposable();
    },
    onScroll: disposable,
    onResize: disposable,
  } as unknown as Terminal;
  return { terminal, providers, disposals };
}

function linksAt(provider: ILinkProvider, row: number) {
  let links: ILink[] = [];
  provider.provideLinks(row, (result) => {
    links = result ?? [];
  });
  return links;
}

it("maps wrapped file references after wide and combining cells", () => {
  const { terminal } = terminalFixture(["界e\u0301 src/long", "file.ts:42"], 12);
  const provider = createTerminalLinkProvider(terminal, { activate: vi.fn() });
  for (const row of [1, 2]) {
    expect(linksAt(provider, row)[0]).toMatchObject({
      text: "src/longfile.ts:42",
      range: { start: { x: 5, y: 1 }, end: { x: 10, y: 2 } },
    });
  }
});

it.each([
  '"hello world.txt"',
  "'hello world.txt'",
  "`hello world.txt`",
  "[file](<hello world.txt>)",
  "[file](hello world.txt)",
])("opens the full delimited filename in %s", async (reference) => {
  const tooltip = { style: {}, setAttribute: vi.fn(), remove: vi.fn() };
  vi.stubGlobal("document", { createElement: () => tooltip, body: { appendChild: vi.fn() } });
  vi.stubGlobal("window", { addEventListener: vi.fn(), removeEventListener: vi.fn() });
  const { terminal, providers } = terminalFixture([reference], reference.length);
  const deps = actions();
  const installed = installTerminalLinks(terminal, deps);
  expect(providers).toHaveLength(1);
  const links = linksAt(providers[0], 1);
  expect(links).toHaveLength(1);
  const link = links[0];
  expect(link.text).toBe("hello world.txt");
  expect(link.range).toEqual({
    start: { x: reference.indexOf("hello world.txt") + 1, y: 1 },
    end: { x: reference.indexOf("hello world.txt") + "hello world.txt".length, y: 1 },
  });
  link.activate({ button: 0 } as MouseEvent, link.text);
  await Promise.resolve();
  expect(deps.openFile).toHaveBeenCalledOnce();
  expect(deps.openFile).toHaveBeenCalledWith("project", "epsilon", "hello world.txt");
  expect(deps.openUrl).not.toHaveBeenCalled();
  expect(deps.showError).not.toHaveBeenCalled();
  installed.dispose();
});

it("wires web links and OSC handlers and disposes hover and registrations", async () => {
  const tooltip = { style: {}, setAttribute: vi.fn(), remove: vi.fn(), textContent: "" };
  const appendChild = vi.fn();
  vi.stubGlobal("document", { createElement: () => tooltip, body: { appendChild } });
  const removeEventListener = vi.fn();
  vi.stubGlobal("window", { addEventListener: vi.fn(), removeEventListener });
  const { terminal, providers, disposals } = terminalFixture(["https://exam", "ple.com/a."], 12);
  const deps = actions();
  const installed = installTerminalLinks(terminal, deps);
  expect(providers).toHaveLength(1);
  const web = linksAt(providers[0], 1)[0];
  expect(web.text).toBe("https://example.com/a");
  const event = { button: 0 } as MouseEvent;
  web.activate(event, web.text);
  await Promise.resolve();
  expect(deps.openUrl).toHaveBeenCalledWith("https://example.com/a");
  const handler = terminal.options.linkHandler;
  expect(handler?.allowNonHttpProtocols).toBe(true);
  handler?.hover?.(event, "file:///D:/repo/a.ts#L7", web.range);
  expect(tooltip.textContent).toBe("file:///D:/repo/a.ts#L7");
  expect(appendChild).toHaveBeenCalledWith(tooltip);
  handler?.activate(event, "file:///D:/repo/a.ts#L7", web.range);
  await Promise.resolve();
  expect(deps.openFile).toHaveBeenCalledWith("project", "epsilon", "D:/repo/a.ts");
  deps.openFile.mockClear();
  handler?.activate({ button: 2 } as MouseEvent, "src/a.ts", web.range);
  expect(deps.openFile).not.toHaveBeenCalled();
  terminal.hasSelection = () => true;
  handler?.activate(event, "src/a.ts", web.range);
  expect(deps.openFile).not.toHaveBeenCalled();
  terminal.hasSelection = () => false;
  const listeners = vi.mocked(window.addEventListener).mock.calls;
  const down = listeners.find(([name]) => name === "mousedown")?.[1] as (event: MouseEvent) => void;
  const move = listeners.find(([name]) => name === "mousemove")?.[1] as (event: MouseEvent) => void;
  down({ clientX: 10, clientY: 10 } as MouseEvent);
  move({ clientX: 30, clientY: 10, buttons: 1 } as MouseEvent);
  handler?.activate(event, "src/a.ts", web.range);
  expect(deps.openFile).not.toHaveBeenCalled();
  down({ clientX: 10, clientY: 10 } as MouseEvent);
  handler?.activate(event, "src/a.ts", web.range);
  expect(deps.openFile).toHaveBeenCalledOnce();
  installed.hide();
  installed.dispose();
  expect(tooltip.remove).toHaveBeenCalled();
  expect(removeEventListener).toHaveBeenCalledWith("blur", installed.hide);
  // The link provider, scroll and resize registrations, each disposed once.
  expect(disposals).toHaveLength(3);
  expect(disposals.every((dispose) => dispose.mock.calls.length === 1)).toBe(true);
  expect(terminal.options.linkHandler).toBeUndefined();
});

describe("links across repainted rows", () => {
  // A repainted screen never marks rows wrapped, so every fixture here passes wrapped=false.
  function install(rows: string[], cols: number) {
    vi.stubGlobal("document", {
      createElement: () => ({ style: {}, setAttribute: vi.fn(), remove: vi.fn() }),
      body: { appendChild: vi.fn() },
    });
    vi.stubGlobal("window", { addEventListener: vi.fn(), removeEventListener: vi.fn() });
    const { terminal, providers } = terminalFixture(rows, cols, false);
    const deps = actions();
    const installed = installTerminalLinks(terminal, deps);
    return { deps, installed, provider: providers[0] };
  }

  async function click(link: ILink) {
    link.activate({ button: 0 } as MouseEvent, link.text);
    // Let the open and any fallback open settle.
    for (let i = 0; i < 10; i++) await Promise.resolve();
  }

  it("joins a URL across a filled row and drops the gutter indent", async () => {
    const { deps, installed, provider } = install(
      ["see https://example.", "  com/docs/page then"],
      20,
    );
    for (const row of [1, 2]) {
      const links = linksAt(provider, row);
      expect(links).toHaveLength(1);
      expect(links[0]).toMatchObject({
        text: "https://example.com/docs/page",
        range: { start: { x: 5, y: 1 }, end: { x: 15, y: 2 } },
      });
    }
    await click(linksAt(provider, 2)[0]);
    expect(deps.openUrl).toHaveBeenCalledWith("https://example.com/docs/page");
    installed.dispose();
  });

  it("joins a file path across a filled row", async () => {
    const { deps, installed, provider } = install(["edit src/compone", "  nts/App.tsx:12"], 16);
    for (const row of [1, 2]) {
      expect(linksAt(provider, row)[0]).toMatchObject({
        text: "src/components/App.tsx:12",
        range: { start: { x: 6, y: 1 }, end: { x: 16, y: 2 } },
      });
    }
    await click(linksAt(provider, 1)[0]);
    expect(deps.openFile).toHaveBeenCalledOnce();
    expect(deps.openFile).toHaveBeenCalledWith("project", "epsilon", "src/components/App.tsx");
    installed.dispose();
  });

  it("falls back to the first row's part when a path absorbed the next row's prose", async () => {
    const { deps, installed, provider } = install(["open src/main.rs", "  then run it"], 16);
    const link = linksAt(provider, 1)[0];
    expect(link).toMatchObject({
      text: "src/main.rsthen",
      range: { start: { x: 6, y: 1 }, end: { x: 6, y: 2 } },
    });
    deps.openFile.mockRejectedValueOnce(new Error(`${FILE_UNAVAILABLE_ERROR}: not found`));
    await click(link);
    expect(deps.openFile).toHaveBeenCalledTimes(2);
    expect(deps.openFile).toHaveBeenNthCalledWith(1, "project", "epsilon", "src/main.rsthen");
    expect(deps.openFile).toHaveBeenNthCalledWith(2, "project", "epsilon", "src/main.rs");
    expect(deps.showError).not.toHaveBeenCalled();

    deps.openFile.mockClear();
    deps.openFile.mockRejectedValueOnce(new Error(`${FILE_UNAVAILABLE_ERROR}: not found`));
    deps.openFile.mockRejectedValueOnce(new Error(`${FILE_UNAVAILABLE_ERROR}: also missing`));
    await click(link);
    expect(deps.openFile).toHaveBeenCalledTimes(2);
    expect(deps.showError).toHaveBeenCalledWith(
      `Could not open link: ${FILE_UNAVAILABLE_ERROR}: not found`,
    );
    installed.dispose();
  });

  it("keeps a row's file link with a location when the next row's word glues onto it", async () => {
    const { deps, installed, provider } = install(["open src/main.rs:12", "  then run it"], 19);
    expect(linksAt(provider, 1)).toMatchObject([
      { text: "src/main.rs:12", range: { start: { x: 6, y: 1 }, end: { x: 19, y: 1 } } },
    ]);
    expect(linksAt(provider, 2)).toEqual([]);
    await click(linksAt(provider, 1)[0]);
    expect(deps.openFile).toHaveBeenCalledWith("project", "epsilon", "src/main.rs");
    installed.dispose();
  });

  it("falls back to the row's own path when it absorbed the last word of the row above", async () => {
    const { deps, installed, provider } = install(["see the file", "  src/a.ts:3 ok"], 12);
    const link = linksAt(provider, 2)[0];
    expect(link).toMatchObject({
      text: "filesrc/a.ts:3",
      range: { start: { x: 9, y: 1 }, end: { x: 12, y: 2 } },
    });
    deps.openFile.mockRejectedValueOnce(`${FILE_UNAVAILABLE_ERROR}: not found`);
    await click(link);
    expect(deps.openFile).toHaveBeenCalledTimes(2);
    expect(deps.openFile).toHaveBeenLastCalledWith("project", "epsilon", "src/a.ts");
    expect(deps.showError).not.toHaveBeenCalled();
    installed.dispose();
  });

  it("does not fall back when the open was refused rather than missing", async () => {
    const { deps, installed, provider } = install(["see the file", "  src/a.ts:3 ok"], 12);
    deps.openFile.mockRejectedValueOnce("Only existing regular files can be opened");
    await click(linksAt(provider, 2)[0]);
    expect(deps.openFile).toHaveBeenCalledOnce();
    expect(deps.showError).toHaveBeenCalledWith(
      "Could not open link: Only existing regular files can be opened",
    );
    installed.dispose();
  });

  it("falls back to a delimited path with the dropped indent kept as a real space", async () => {
    const { deps, installed, provider } = install(['see "src/my', ' file.ts" ok'], 11);
    const link = linksAt(provider, 1)[0];
    expect(link.text).toBe("src/myfile.ts");
    deps.openFile.mockRejectedValueOnce(new Error(`${FILE_UNAVAILABLE_ERROR}: not found`));
    await click(link);
    expect(deps.openFile).toHaveBeenCalledTimes(2);
    expect(deps.openFile).toHaveBeenLastCalledWith("project", "epsilon", "src/my file.ts");
    expect(deps.showError).not.toHaveBeenCalled();
    installed.dispose();
  });

  it("drops a no-break-space indent on a filled-row join", () => {
    const { installed, provider } = install(["edit src/compone", "  nts/App.tsx:12"], 16);
    expect(linksAt(provider, 2)[0]).toMatchObject({
      text: "src/components/App.tsx:12",
      range: { start: { x: 6, y: 1 }, end: { x: 16, y: 2 } },
    });
    installed.dispose();
  });

  it("does not let a URL at the right edge absorb the next row's word", () => {
    const { installed, provider } = install(["see https://example.com/docs", "  and more"], 28);
    expect(linksAt(provider, 1)).toMatchObject([
      { text: "https://example.com/docs", range: { start: { x: 5, y: 1 }, end: { x: 28, y: 1 } } },
    ]);
    expect(linksAt(provider, 2)).toEqual([]);
    installed.dispose();
  });

  it("does not join a row that stops short of the last cell", () => {
    const { installed, provider } = install(["see https://example", "  .com/docs"], 20);
    expect(linksAt(provider, 1)).toMatchObject([
      { text: "https://example", range: { start: { x: 5, y: 1 }, end: { x: 19, y: 1 } } },
    ]);
    installed.dispose();
  });

  it("does not join a filled row onto a blank row", () => {
    const { installed, provider } = install(["src/a.ts:1", "      "], 10);
    expect(linksAt(provider, 1)).toMatchObject([
      { text: "src/a.ts:1", range: { start: { x: 1, y: 1 }, end: { x: 10, y: 1 } } },
    ]);
    installed.dispose();
  });

  it("does not join a row ending in a box-drawing border", () => {
    const { installed, provider } = install(["│ https://exam │", "│ ple.com      │"], 16);
    expect(linksAt(provider, 1)).toMatchObject([
      { text: "https://exam", range: { start: { x: 3, y: 1 }, end: { x: 14, y: 1 } } },
    ]);
    installed.dispose();
  });

  it("joins at a wide glyph filling the last two cells", () => {
    const { installed, provider } = install(["see src/界", "  a.ts:3"], 10);
    for (const row of [1, 2]) {
      expect(linksAt(provider, row)[0]).toMatchObject({
        text: "src/界a.ts:3",
        range: { start: { x: 5, y: 1 }, end: { x: 8, y: 2 } },
      });
    }
    installed.dispose();
  });

  it("joins past the padding cell a wrapped wide glyph leaves", () => {
    const { installed, provider } = install(["see src/a", "界.ts:3"], 10);
    for (const row of [1, 2]) {
      expect(linksAt(provider, row)[0]).toMatchObject({
        text: "src/a界.ts:3",
        range: { start: { x: 5, y: 1 }, end: { x: 7, y: 2 } },
      });
    }
    installed.dispose();
  });
});

describe("logical lines", () => {
  function buffer(rows: string[], cols: number) {
    return terminalFixture(rows, cols, false).terminal.buffer.active;
  }

  it("builds the same line from any of its rows, with the indent left out of the maps", () => {
    const reader = bufferRowReader(buffer(["abcd", "  e", "gh"], 4), 4);
    for (const row of [0, 1]) {
      expect(logicalLineAt(reader, row, 100)).toEqual({
        text: "abcde ",
        xs: [1, 2, 3, 4, 3, 4],
        ys: [1, 1, 1, 1, 2, 2],
        breaks: [4],
        indents: ["  "],
      });
    }
    // "  e" stops short of the last cell, so the third row starts a line of its own.
    expect(logicalLineAt(reader, 2, 100).text).toBe("gh  ");
  });

  it("caps the rows a line spans", () => {
    const reader = bufferRowReader(buffer(["abcd", "efgh", "ijkl", "mnop"], 4), 4);
    expect(logicalLineAt(reader, 1, 1).text).toBe("abcdefgh");
    expect(joinsNext(reader, 3)).toBeNull();
  });

  it("joins soft-wrapped rows verbatim", () => {
    const { terminal } = terminalFixture(["ab  ", "  cd"], 4);
    const reader = bufferRowReader(terminal.buffer.active, 4);
    expect(joinsNext(reader, 0)).toBe("wrap");
    expect(logicalLineAt(reader, 1, 100)).toMatchObject({ text: "ab    cd", breaks: [] });
  });

  it("keeps a URL joined while the text past the join is more than letters", () => {
    expect(findLineLinks({ text: "https://example.com/do/csand", breaks: [22, 25] })).toMatchObject(
      [{ text: "https://example.com/do/cs", index: 0 }],
    );
  });

  it("prefers a web link over a file reading of the same text", () => {
    expect(findLineLinks({ text: "see https://example.com/src/a.ts:3 now", breaks: [] })).toEqual([
      { text: "https://example.com/src/a.ts:3", index: 4, delimited: false },
    ]);
  });
});
