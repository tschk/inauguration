import { expect, test } from "bun:test";
import { helper, main } from "./control_flow";

test("helper returns the same value", () => {
    expect(helper(5)).toBe(5);
});

test("main returns 4", () => {
    expect(main()).toBe(4);
});
