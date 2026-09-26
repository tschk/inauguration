// conformance/classes/class-methods.java
// Tests: multiple methods on a class, interface declarations
// Expected: parse ok, graph shows Calculator methods
// @expect parse: ok
// @expect has-function: Calculator_add
// @expect has-function: Calculator_subtract
// @expect has-function: Calculator_multiply

interface MathOp {
  int compute(int a, int b);
}

class Calculator {
  int add(int a, int b) {
    return a + b;
  }

  int subtract(int a, int b) {
    return a - b;
  }

  int multiply(int a, int b) {
    return a * b;
  }
}

class ClassMethods {
  public static void main(String[] args) {}
}
