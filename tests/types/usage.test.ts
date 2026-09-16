import { describe, expect, it } from "vitest";
import { getCacheWriteAvailability } from "@/types/usage";

describe("getCacheWriteAvailability", () => {
  it("distinguishes cache-write support across fixed protocols", () => {
    expect(getCacheWriteAvailability(["claude"])).toBe("ok");
    expect(getCacheWriteAvailability(["pi"])).toBe("partial");
    expect(getCacheWriteAvailability(["codex", "gemini"])).toBe("na");
    expect(getCacheWriteAvailability(["claude", "codex"])).toBe("partial");
    expect(getCacheWriteAvailability([])).toBe("ok");
  });

  it("never returns na in all-apps view", () => {
    expect(getCacheWriteAvailability(["codex", "gemini"], true)).not.toBe("na");
    expect(getCacheWriteAvailability(["codex", "gemini"], true)).toBe("partial");
    expect(getCacheWriteAvailability(["claude"], true)).toBe("ok");
    expect(getCacheWriteAvailability([], true)).toBe("ok");
  });
});
