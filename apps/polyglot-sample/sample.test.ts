import { expect, test } from "bun:test";
import { readFileSync } from "fs";

// We read sample.ts and manually evaluate the JS output from Bun's transpiler
const tsCode = readFileSync("apps/polyglot-sample/sample.ts", "utf-8");
const transpiler = new Bun.Transpiler({ loader: "ts" });
const jsCode = await transpiler.transform(tsCode);

// Evaluate and get the variables
const evaluated = new Function(`
  ${jsCode}
  return { answer, TypedCounter, quadruple };
`)();

const { answer, TypedCounter, quadruple } = evaluated;

test("answer returns 42", () => {
  expect(answer()).toBe(42);
});

test("TypedCounter increments value", () => {
  const counter = new TypedCounter(10);
  expect(counter.value).toBe(10);

  expect(counter.inc()).toBe(11);
  expect(counter.value).toBe(11);

  expect(counter.inc()).toBe(12);
  expect(counter.value).toBe(12);
});

test("quadruple multiplies by 4", () => {
  expect(quadruple(5)).toBe(20);
  expect(quadruple(0)).toBe(0);
  expect(quadruple(-2)).toBe(-8);
});
