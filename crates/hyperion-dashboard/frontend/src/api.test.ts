import { describe, expect, it } from "vitest";
import {
  dashboardError,
  formatMetric,
  isLocalProfile,
  profileId,
  serializePreviewHello,
  serializePreviewInput,
} from "./api";

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

describe("profile locality and preview commands", () => {
  it("honors the local flag from the catalog", () => {
    expect(isLocalProfile({ local: false })).toBe(false);
    expect(isLocalProfile({ owner_node: null })).toBe(true);
  });

  it("serializes preview hello/input for the websocket", () => {
    expect(serializePreviewHello("", "p")).toEqual({ type: "hello", profileId: "p" });
    expect(
      serializePreviewInput({ id: 8, generation: 1, action: "text", text: "hi", nodeId: 3 }),
    ).toMatchObject({ type: "input", action: "text", text: "hi", nodeId: 3 });
  });

  it("extracts named dashboard error codes", () => {
    expect(dashboardError({ id: "ok" })).toBeNull();
    expect(dashboardError({ code: -32000, message: "Overloaded: too many targets" })).toEqual({
      code: "server",
      message: "Overloaded: too many targets",
    });
  });
});
