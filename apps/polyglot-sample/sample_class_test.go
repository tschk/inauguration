package main

import "testing"

func TestNewCalculator(t *testing.T) {
	calc := NewCalculator()
	if calc.value != 0 {
		t.Error("Expected Initialized Calculator value to be 0, got", calc.value)
	}
}

func TestCalculatorAdd(t *testing.T) {
	calc := NewCalculator()
	res := calc.Add(10)
	if res != 10 {
		t.Error("Expected Add to return 10, got", res)
	}
	if calc.value != 10 {
		t.Error("Expected value to be 10, got", calc.value)
	}
}

func TestCalculatorAnswer(t *testing.T) {
	calc := NewCalculator()
	if ans := calc.Answer(); ans != 42 {
		t.Error("Expected Answer to return 42, got", ans)
	}
}

func TestAnswer(t *testing.T) {
	if ans := answer(); ans != 42 {
		t.Error("Expected answer to return 42, got", ans)
	}
}
