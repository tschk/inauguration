// conformance/classes/class-basic.java
// Tests: basic class declaration with fields and methods
// Expected: parse ok, graph shows Counter class methods
// @expect parse: ok
// @expect has-function: Counter_increment
// @expect has-function: Counter_getValue

class Counter {
  private int count;

  Counter() {
    count = 0;
  }

  int increment() {
    count = count + 1;
    return count;
  }

  int getValue() {
    return count;
  }
}

class ClassBasic {
  public static void main(String[] args) {}
}
