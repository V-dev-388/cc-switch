import { describe, expect, it } from "vitest";
import { getCacheWriteAvailability } from "@/types/usage";

describe("getCacheWriteAvailability", () => {
  it("distinguishes cache-write support across fixed protocols", () => {
    expect(getCacheWriteAvailability(["claude"])).toBe("ok");
    expect(getCacheWriteAvailability(["pi"])).toBe("partial");
    expect(getCacheWriteAvailability(["codex", "gemini"])).toBe("na");
    expect(getCacheWriteAvailability(["dsh"])).toBe("na");
    expect(getCacheWriteAvailability(["claude", "codex"])).toBe("partial");
    expect(getCacheWriteAvailability(["claude", "dsh"])).toBe("partial");
    expect(getCacheWriteAvailability([])).toBe("ok");
  });

  it("returns na when all active apps do not report cache creation", () => {
    expect(
      getCacheWriteAvailability(["codex", "gemini", "dsh", "codebuddy", "qodercn"]),
    ).toBe("na");
  });

  it("returns partial when some active apps report cache creation and others do not", () => {
    expect(getCacheWriteAvailability(["claude", "gemini", "dsh"])).toBe("partial");
  });
});
