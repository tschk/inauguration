package main

import "testing"

func TestNewCalculator(t *testing.T) {
	t.Run("initialization", func(t *testing.T) {
		calc := NewCalculator()
		if calc.value != 0 {
			t.Errorf("Expected Initialized Calculator value to be 0, got %d", calc.value)
		}
	})

	t.Run("multiple instances do not share state", func(t *testing.T) {
		calc1 := NewCalculator()
		calc2 := NewCalculator()

		calc1.Add(5)

		if calc1.value != 5 {
			t.Errorf("Expected calc1 value to be 5, got %d", calc1.value)
		}
		if calc2.value != 0 {
			t.Errorf("Expected calc2 value to remain 0, got %d", calc2.value)
		}
	})
}

func TestCalculatorAdd(t *testing.T) {
	calc := NewCalculator()
	res := calc.Add(10)
	if res != 10 {
		t.Errorf("Expected Add to return 10, got %d", res)
	}
	if calc.value != 10 {
		t.Errorf("Expected value to be 10, got %d", calc.value)
	}
}

func TestCalculatorAnswer(t *testing.T) {
	calc := NewCalculator()
	if ans := calc.Answer(); ans != 42 {
		t.Errorf("Expected Answer to return 42, got %d", ans)
	}
}

func TestAnswer(t *testing.T) {
	if ans := answer(); ans != 42 {
		t.Errorf("Expected answer to return 42, got %d", ans)
	}
}
