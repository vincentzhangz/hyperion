import { describe, expect, it } from "vitest";
import { formatMetric, profileId } from "./api";

describe("formatMetric", () => {
  it("renders missing values as an em dash", () => {
    expect(formatMetric(undefined)).toBe("—");
    expect(formatMetric(null)).toBe("—");
  });

  it("keeps integers exact", () => {
    expect(formatMetric(12)).toBe("12");
  });
});

describe("profileId", () => {
  it("reads string ids", () => {
    expect(profileId({ id: "abc" })).toBe("abc");
  });
});
