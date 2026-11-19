import { describe, expect, it } from "vitest";
import { clampNotePosition, EMPTY_NOTE, NOTE_MARGIN } from "./stickyNotes";

const note = { width: 200, height: 100 };
const stage = { width: 800, height: 600 };

describe("clampNotePosition", () => {
  it("keeps a position that already fits", () => {
    expect(clampNotePosition({ x: 300, y: 200 }, note, stage)).toEqual({ x: 300, y: 200 });
  });

  it("pulls a note back inside the right and bottom edges", () => {
    expect(clampNotePosition({ x: 5000, y: 5000 }, note, stage)).toEqual({
      x: 800 - 200 - NOTE_MARGIN,
      y: 600 - 100 - NOTE_MARGIN,
    });
  });

  it("pulls a note back inside the left and top edges", () => {
    expect(clampNotePosition({ x: -50, y: -50 }, note, stage)).toEqual({
      x: NOTE_MARGIN,
      y: NOTE_MARGIN,
    });
  });

  it("parks a never-saved note at the top-right corner", () => {
    expect(clampNotePosition(EMPTY_NOTE, note, stage)).toEqual({
      x: 800 - 200 - NOTE_MARGIN,
      y: NOTE_MARGIN,
    });
  });

  it("favours the top-left edge when the stage is smaller than the note", () => {
    expect(clampNotePosition({ x: 100, y: 100 }, note, { width: 150, height: 80 })).toEqual({
      x: NOTE_MARGIN,
      y: NOTE_MARGIN,
    });
  });

  it("treats a non-finite position as the top-left corner", () => {
    expect(clampNotePosition({ x: Number.NaN, y: Number.NaN }, note, stage)).toEqual({
      x: NOTE_MARGIN,
      y: NOTE_MARGIN,
    });
  });
});
